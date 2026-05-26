#!/usr/bin/env python3
"""eval.py — evaluate the trained ChatDecoder.

Outputs:
  - Per-archetype perplexity on the validation split
  - Sample generations for each archetype × trigger combination
  - Optional BLEU-1/BLEU-2 vs the phrasebook corpus (--corpus-compare)

Usage:
    python scripts/eval.py \
        --checkpoint data/checkpoints/best.pt \
        --vocab      data/vocab.model \
        --dataset    data/dataset/
    # With verbose generation samples:
    python scripts/eval.py --checkpoint data/checkpoints/best.pt \
        --vocab data/vocab.model --samples 8
"""

import argparse
import json
import math
import sys
from collections import defaultdict
from pathlib import Path

import numpy as np
import sentencepiece as spm
import torch
import torch.nn.functional as F

sys.path.insert(0, str(Path(__file__).parent))
from model import ChatDecoder
from build_dataset import build_prefix_ids, classify_trigger, CONTROL_TOKENS


# ── Checkpoint loading ────────────────────────────────────────────────────────

def load_checkpoint(path: str, device: torch.device) -> tuple[ChatDecoder, dict]:
    ckpt = torch.load(path, map_location=device, weights_only=False)
    meta = ckpt["meta"]
    model = ChatDecoder(
        vocab_size=meta["vocab_size"],
        max_len=meta["max_len"],
        pad_id=meta["pad_id"],
    ).to(device)
    model.load_state_dict(ckpt["model_state"])
    model.eval()
    return model, meta


# ── Perplexity ────────────────────────────────────────────────────────────────

def batch_perplexity(
    model: ChatDecoder,
    tokens: torch.Tensor,
    prefix_lens: torch.Tensor,
    pad_id: int,
) -> float:
    """Return average perplexity over the batch (response tokens only)."""
    B, T, V = (tokens.shape[0], tokens.shape[1], model.vocab_size)
    with torch.no_grad():
        logits = model(tokens)   # (B, T, V)

    shift_logits  = logits[:, :-1, :].contiguous()
    shift_targets = tokens[:, 1:].contiguous()

    pos           = torch.arange(T - 1, device=tokens.device).unsqueeze(0)
    response_mask = pos >= (prefix_lens.unsqueeze(1) - 1)
    pad_mask      = shift_targets != pad_id
    mask          = response_mask & pad_mask

    n_active = mask.float().sum().item()
    if n_active == 0:
        return float("inf")

    per_token = F.cross_entropy(
        shift_logits.view(-1, V),
        shift_targets.view(-1),
        ignore_index=pad_id,
        reduction="none",
    ).view(B, T - 1)

    avg_loss = (per_token * mask.float()).sum().item() / n_active
    return math.exp(min(avg_loss, 20))


# ── Generation ────────────────────────────────────────────────────────────────

_TRIGGER_TO_CONTEXT_KIND: dict[str, str] = {
    "MobKilled":      "slay",
    "PlayerDied":     "death",
    "SpellResisted":  "resist",
    "BigHit":         "big_hit",
    "NearDeath":      "big_hit",
    "CasterOom":      "oom",
    "Incoming":       "",
    "Loot":           "loot",
    "LevelUp":        "level",
    "Wipe":           "death",
    "Banter":         "",
}


def generate_sample(
    model: ChatDecoder,
    sp: spm.SentencePieceProcessor,
    meta: dict,
    archetype: str,
    trigger: str,
    temperature: float = 0.9,
    top_k: int = 40,
) -> str:
    """Generate a single sample utterance for the given archetype and trigger."""
    base_vocab = meta["base_vocab"]
    ctrl: dict[str, int] = {tok: base_vocab + i for i, tok in enumerate(CONTROL_TOKENS)}

    context_kind = _TRIGGER_TO_CONTEXT_KIND.get(trigger, "")
    fake_rec = {
        "archetype": archetype,
        "context": [{"kind": context_kind, "summary": ""}] if context_kind else [],
    }
    prefix_ids = build_prefix_ids(fake_rec, ctrl)
    prefix = torch.tensor([prefix_ids], dtype=torch.long)

    out_ids = model.generate(
        prefix, temperature=temperature, top_k=top_k, eos_id=meta["eos_id"]
    )
    if out_ids and out_ids[-1] == meta["eos_id"]:
        out_ids = out_ids[:-1]

    return sp.decode(out_ids)


# ── BLEU helpers ──────────────────────────────────────────────────────────────

def ngrams(tokens: list[str], n: int) -> list[tuple]:
    return [tuple(tokens[i: i + n]) for i in range(len(tokens) - n + 1)]


def bleu_n(hypothesis: str, references: list[str], n: int) -> float:
    """Micro-averaged BLEU-n precision (no brevity penalty) over a list of refs."""
    if not hypothesis.strip():
        return 0.0
    hyp_tokens = hypothesis.lower().split()
    hyp_ng = ngrams(hyp_tokens, n)
    if not hyp_ng:
        return 0.0

    total_matches = 0
    for ref in references:
        ref_ng = ngrams(ref.lower().split(), n)
        ref_counts: dict[tuple, int] = {}
        for g in ref_ng:
            ref_counts[g] = ref_counts.get(g, 0) + 1
        matches = sum(min(hyp_ng.count(g), ref_counts.get(g, 0)) for g in set(hyp_ng))
        total_matches = max(total_matches, matches)

    return total_matches / len(hyp_ng)


# ── Main ──────────────────────────────────────────────────────────────────────

def main() -> None:
    ap = argparse.ArgumentParser(description=__doc__,
                                 formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("--checkpoint", required=True)
    ap.add_argument("--vocab",      required=True, help="SentencePiece .model file")
    ap.add_argument("--dataset",    default="data/dataset")
    ap.add_argument("--corpus",     default="data/corpus.jsonl",
                    help="Original JSONL corpus (for archetype labels + BLEU refs)")
    ap.add_argument("--device",     default="cpu")
    ap.add_argument("--samples",    type=int, default=5,
                    help="Generation samples per archetype×trigger (default: 5)")
    ap.add_argument("--temperature", type=float, default=0.9)
    ap.add_argument("--bleu",       action="store_true",
                    help="Compute BLEU-1/2 against phrasebook corpus references")
    args = ap.parse_args()

    device = torch.device(args.device)
    model, meta = load_checkpoint(args.checkpoint, device)
    sp = spm.SentencePieceProcessor()
    sp.load(args.vocab)

    data_dir = Path(args.dataset)
    val_seqs = np.load(data_dir / "val_seqs.npy")
    val_pfx  = np.load(data_dir / "val_prefix_lens.npy")

    # Load corpus records to recover archetype labels for the val split
    records: list[dict] = []
    with open(args.corpus, encoding="utf-8") as fh:
        for line in fh:
            records.append(json.loads(line.strip()))

    rng = np.random.default_rng(42)
    perm = rng.permutation(len(records))
    n_val = max(1, int(len(records) * 0.05))
    val_idx_original = perm[:n_val]
    val_records = [records[i] for i in val_idx_original]

    # ── Per-archetype perplexity ───────────────────────────────────────────────
    archetypes = sorted({r["archetype"] for r in val_records})
    print("Per-archetype validation perplexity")
    print("-" * 45)
    for arch in archetypes:
        idxs = [i for i, r in enumerate(val_records) if r["archetype"] == arch]
        if not idxs:
            continue
        s = torch.from_numpy(val_seqs[idxs]).long().to(device)
        p = torch.from_numpy(val_pfx[idxs]).long().to(device)
        ppl = batch_perplexity(model, s, p, meta["pad_id"])
        print(f"  {arch:<22}  ppl = {ppl:7.2f}  (n={len(idxs)})")

    # ── Sample generations ────────────────────────────────────────────────────
    triggers = ["MobKilled", "SpellResisted", "BigHit", "CasterOom", "Banter"]
    print("\nSample generations")
    print("-" * 45)
    for arch in ["QuietAnchor", "ChaoticNarrator", "RaidLeader"]:
        for trig in triggers:
            print(f"\n  [{arch}] [{trig}]")
            for _ in range(args.samples):
                text = generate_sample(
                    model, sp, meta, arch, trig, temperature=args.temperature
                )
                print(f"    → {text!r}")

    # ── Optional BLEU ─────────────────────────────────────────────────────────
    if args.bleu:
        print("\nBLEU-1 / BLEU-2 vs corpus references")
        print("-" * 45)
        # Build per-archetype reference lists from the full corpus
        refs_by_arch: dict[str, list[str]] = defaultdict(list)
        for rec in records:
            refs_by_arch[rec["archetype"]].append(rec["text"])

        for arch in ["QuietAnchor", "ChaoticNarrator", "RaidLeader"]:
            refs = refs_by_arch[arch]
            if not refs:
                continue
            b1_scores, b2_scores = [], []
            for trig in triggers:
                for _ in range(args.samples):
                    hyp = generate_sample(model, sp, meta, arch, trig,
                                          temperature=args.temperature)
                    b1_scores.append(bleu_n(hyp, refs, 1))
                    b2_scores.append(bleu_n(hyp, refs, 2))
            print(
                f"  {arch:<22}  "
                f"BLEU-1 {sum(b1_scores)/len(b1_scores):.3f}  "
                f"BLEU-2 {sum(b2_scores)/len(b2_scores):.3f}"
            )


if __name__ == "__main__":
    main()
