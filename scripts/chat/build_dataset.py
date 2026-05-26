#!/usr/bin/env python3
"""build_dataset.py — tokenise corpus into (sequence, prefix_len) pairs for training.

Prefix format (control tokens prepended before BPE text):
    [ARCH] <archetype_id> [REG] <register_id> [TRIG] <trigger_id> [RESPONSE]

Response tokens follow the prefix and are terminated with <eos>.
Loss is computed only on positions at or after [RESPONSE] — the prefix is masked.

Outputs (in --output directory):
    train_seqs.npy        int32 (N_train, max_len)
    train_prefix_lens.npy int32 (N_train,)
    val_seqs.npy          int32 (N_val,   max_len)
    val_prefix_lens.npy   int32 (N_val,)
    meta.json             vocabulary metadata for train.py / eval.py

Usage:
    python scripts/build_dataset.py \
        --corpus data/chat/build/corpus.jsonl \
        --vocab  data/chat/build/vocab.model
    python scripts/build_dataset.py --corpus data/chat/build/corpus.jsonl --vocab data/chat/build/vocab.model \
        --output data/chat/build/dataset --max-len 128 --val-frac 0.05
"""

import argparse
import json
from pathlib import Path

import numpy as np
import sentencepiece as spm

# ── Control tokens appended after the BPE vocab ───────────────────────────────
#
# Ordering here is stable — do not reorder without retraining.

CONTROL_TOKENS: list[str] = [
    # Structural delimiters
    "[ARCH]", "[REG]", "[TRIG]", "[RESPONSE]",
    # Archetype values
    "[QuietAnchor]", "[ChaoticNarrator]", "[RaidLeader]", "[Unknown]",
    # Emotional register values
    "[Neutral]", "[Frustrated]", "[Elated]", "[Tired]", "[Engaged]",
    # Trigger kind values
    "[MobKilled]", "[PlayerDied]", "[SpellResisted]", "[BigHit]",
    "[NearDeath]", "[CasterOom]", "[Incoming]", "[Loot]", "[LevelUp]",
    "[Wipe]", "[Banter]",
    # Personality bucket tokens (mirrors thresholds in neural.rs build_prefix)
    "[VERBOSE]", "[TERSE]",
    "[HUMOROUS]", "[SERIOUS]",
    "[PATIENT]", "[IMPATIENT]",
]

# Personality trait values per archetype — mirrors PersonalityProfile presets in personality.rs.
# Used to derive bucketed personality control tokens during dataset construction.
ARCHETYPE_PERSONALITY: dict[str, dict[str, float]] = {
    "QuietAnchor":      {"verbosity": 0.28, "humor": 0.40, "patience": 0.78},
    "ChaoticNarrator":  {"verbosity": 0.78, "humor": 0.65, "patience": 0.58},
    "RaidLeader":       {"verbosity": 0.48, "humor": 0.30, "patience": 0.72},
    "ReactiveObserver": {"verbosity": 0.18, "humor": 0.25, "patience": 0.80},
    "TacticalFocused":  {"verbosity": 0.50, "humor": 0.20, "patience": 0.70},
}

MAX_LEN = 128


# ── Context classification ─────────────────────────────────────────────────────

def classify_trigger(context: list[dict]) -> str:
    """Heuristically classify the most likely trigger from context event list."""
    if not context:
        return "Banter"
    kinds = [ev.get("kind", "") for ev in context]
    # Walk backwards — most recent event is most salient
    for ev in reversed(context):
        kind = ev.get("kind", "")
        if kind == "slay":
            return "MobKilled"
        if kind in ("death", "death_self"):
            death_count = kinds.count("death") + kinds.count("death_self")
            return "Wipe" if death_count >= 2 else "PlayerDied"
        if kind == "resist":
            return "SpellResisted"
        if kind == "big_hit":
            # Repeated hits without a kill suggest the party is taking sustained damage
            return "NearDeath" if kinds.count("big_hit") >= 2 else "BigHit"
        if kind == "oom":
            return "CasterOom"
        if kind == "level":
            return "LevelUp"
        if kind == "loot":
            return "Loot"
        if kind == "cast":
            return "Incoming"
    return "Banter"


def classify_register(context: list[dict]) -> str:
    """Classify emotional register from recent context events.

    Priority: death/loss events dominate; achievements come next; active combat
    engagement last.  Neutral is the fallback when context is quiet.
    """
    if not context:
        return "Neutral"
    for ev in reversed(context):
        kind = ev.get("kind", "")
        if kind in ("death", "death_self"):
            return "Frustrated"
        if kind == "level":
            return "Elated"
        if kind == "slay":
            return "Elated"
        if kind == "oom":
            return "Tired"
        if kind == "resist":
            return "Frustrated"
        if kind in ("big_hit", "cast"):
            return "Engaged"
        if kind == "loot":
            return "Elated"
    return "Neutral"


# ── Slot extraction ───────────────────────────────────────────────────────────

def extract_slot_text(context: list[dict], trigger_kind: str) -> str | None:
    """Extract the primary entity name for trigger_kind from the context event list."""
    for ev in reversed(context):
        kind    = ev.get("kind", "")
        summary = ev.get("summary", "")
        if trigger_kind == "MobKilled" and kind == "slay":
            if " slew " in summary:
                return summary.split(" slew ", 1)[1]
        elif trigger_kind in ("BigHit", "NearDeath") and kind == "big_hit":
            if " hit " in summary and " for " in summary:
                return summary.split(" hit ", 1)[1].split(" for ")[0]
        elif trigger_kind in ("PlayerDied", "Wipe") and kind in ("death", "death_self"):
            if " slain by " in summary:
                return summary.split(" slain by ", 1)[1]
        elif trigger_kind == "SpellResisted" and kind == "resist":
            if ": " in summary:
                return summary.split(": ", 1)[1]
        elif trigger_kind == "CasterOom" and kind == "oom":
            if summary.endswith(" OOM"):
                return summary[:-4]
        elif trigger_kind == "Incoming" and kind == "cast":
            if " casting " in summary:
                return summary.split(" casting ", 1)[1]
    return None


# ── Prefix construction ───────────────────────────────────────────────────────

def build_prefix_ids(rec: dict, ctrl: dict[str, int], sp: "spm.SentencePieceProcessor") -> list[int]:
    """Return the integer control-token list that prefixes the response."""
    arch = rec.get("archetype", "Unknown")
    ctx  = rec.get("context", [])
    reg  = classify_register(ctx)
    trig = classify_trigger(ctx)

    ids: list[int] = [
        ctrl["[ARCH]"],  ctrl.get(f"[{arch}]",  ctrl["[Unknown]"]),
        ctrl["[REG]"],   ctrl.get(f"[{reg}]",   ctrl["[Neutral]"]),
        ctrl["[TRIG]"],  ctrl.get(f"[{trig}]",  ctrl["[Banter]"]),
    ]

    # Personality bucket tokens (thresholds mirror neural.rs build_prefix)
    profile   = ARCHETYPE_PERSONALITY.get(arch, {})
    verbosity = profile.get("verbosity", 0.50)
    humor     = profile.get("humor",     0.40)
    patience  = profile.get("patience",  0.65)

    if verbosity > 0.55:
        ids.append(ctrl["[VERBOSE]"])
    elif verbosity < 0.40:
        ids.append(ctrl["[TERSE]"])

    if humor > 0.55:
        ids.append(ctrl["[HUMOROUS]"])
    elif humor < 0.35:
        ids.append(ctrl["[SERIOUS]"])

    if patience > 0.65:
        ids.append(ctrl["[PATIENT]"])
    elif patience < 0.50:
        ids.append(ctrl["[IMPATIENT]"])

    # SP-encoded primary entity name from context
    slot_text = extract_slot_text(ctx, trig)
    if slot_text:
        ids.extend(sp.encode(slot_text, out_type=int))

    ids.append(ctrl["[RESPONSE]"])
    return ids


# ── Main ──────────────────────────────────────────────────────────────────────

def main() -> None:
    ap = argparse.ArgumentParser(description=__doc__,
                                 formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("--corpus", required=True, help="JSONL from extract_corpus.py")
    ap.add_argument("--vocab", required=True, help="SentencePiece .model file")
    ap.add_argument("--output", default="data/dataset",
                    help="Output directory (default: data/dataset)")
    ap.add_argument("--max-len", type=int, default=MAX_LEN,
                    help=f"Max total sequence length (default: {MAX_LEN})")
    ap.add_argument("--val-frac", type=float, default=0.05,
                    help="Fraction of data held out for validation (default: 0.05)")
    args = ap.parse_args()

    sp = spm.SentencePieceProcessor()
    sp.load(args.vocab)
    base_vocab = sp.get_piece_size()

    # Build control token → ID map (contiguous IDs after BPE vocab)
    ctrl: dict[str, int] = {
        tok: base_vocab + i for i, tok in enumerate(CONTROL_TOKENS)
    }
    total_vocab = base_vocab + len(CONTROL_TOKENS)
    print(f"BPE vocab      : {base_vocab}")
    print(f"Control tokens : {len(CONTROL_TOKENS)}")
    print(f"Total vocab    : {total_vocab}")

    records: list[dict] = []
    with open(args.corpus, encoding="utf-8") as fh:
        for line in fh:
            records.append(json.loads(line.strip()))
    print(f"Records loaded : {len(records):,}")

    seqs: list[list[int]] = []
    prefix_lens: list[int] = []

    for rec in records:
        prefix_ids = build_prefix_ids(rec, ctrl, sp)
        response_ids: list[int] = sp.encode(rec["text"], out_type=int)
        response_ids.append(sp.eos_id())

        full = prefix_ids + response_ids
        if len(full) > args.max_len:
            # Truncate the response tail, never the prefix
            full = prefix_ids + response_ids[: args.max_len - len(prefix_ids)]

        pad_count = args.max_len - len(full)
        padded = full + [sp.pad_id()] * pad_count

        seqs.append(padded)
        prefix_lens.append(len(prefix_ids))

    seqs_arr = np.array(seqs, dtype=np.int32)
    pfx_arr = np.array(prefix_lens, dtype=np.int32)

    # Reproducible shuffle then split
    rng = np.random.default_rng(42)
    idx = rng.permutation(len(seqs_arr))
    seqs_arr = seqs_arr[idx]
    pfx_arr = pfx_arr[idx]

    n_val = max(1, int(len(seqs_arr) * args.val_frac))
    splits = {
        "train": (seqs_arr[n_val:], pfx_arr[n_val:]),
        "val":   (seqs_arr[:n_val],  pfx_arr[:n_val]),
    }

    out_dir = Path(args.output)
    out_dir.mkdir(parents=True, exist_ok=True)

    for name, (s, p) in splits.items():
        np.save(out_dir / f"{name}_seqs.npy", s)
        np.save(out_dir / f"{name}_prefix_lens.npy", p)
        print(f"{name:>6}: {len(s):,} sequences")

    meta = {
        "vocab_size": total_vocab,
        "base_vocab": base_vocab,
        "control_tokens": ctrl,
        "max_len": args.max_len,
        "pad_id": int(sp.pad_id()),
        "bos_id": int(sp.bos_id()),
        "eos_id": int(sp.eos_id()),
    }
    with open(out_dir / "meta.json", "w", encoding="utf-8") as fh:
        json.dump(meta, fh, indent=2)

    print(f"Dataset saved to {out_dir}/")


if __name__ == "__main__":
    main()
