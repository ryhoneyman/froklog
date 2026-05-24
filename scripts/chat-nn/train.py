#!/usr/bin/env python3
"""train.py — train the ChatDecoder on the tokenised corpus.

Loss is computed only on response tokens (positions at or after [RESPONSE] in
each sequence), preventing the model from learning to emit control tokens.

Training strategy:
  - AdamW with weight decay
  - Cosine LR decay with linear warmup
  - Gradient clipping at 1.0
  - Best checkpoint saved by validation loss

Usage:
    python scripts/train.py --dataset data/chat/build/dataset/
    python scripts/train.py --dataset data/chat/build/dataset/ --epochs 40 --lr 3e-4 --device cuda
"""

import argparse
import json
import math
import sys
from pathlib import Path

import numpy as np
import torch
import torch.nn.functional as F
from collections import Counter
from torch.utils.data import DataLoader, Dataset, WeightedRandomSampler

# Some CPU builds require AVX2 in their multi-threaded paths; pinning to one
# thread avoids SIGILL on older hardware (e.g. pre-Haswell).
torch.set_num_threads(1)
torch.set_num_interop_threads(1)

sys.path.insert(0, str(Path(__file__).parent))
from model import ChatDecoder


# ── Dataset ───────────────────────────────────────────────────────────────────

class ChatDataset(Dataset):
    def __init__(self, seqs: np.ndarray, prefix_lens: np.ndarray) -> None:
        self.seqs = torch.from_numpy(seqs).long()
        self.prefix_lens = torch.from_numpy(prefix_lens).long()

    def __len__(self) -> int:
        return len(self.seqs)

    def __getitem__(self, idx: int) -> tuple[torch.Tensor, torch.Tensor]:
        return self.seqs[idx], self.prefix_lens[idx]


# ── Loss ──────────────────────────────────────────────────────────────────────

def masked_cross_entropy(
    logits: torch.Tensor,
    tokens: torch.Tensor,
    prefix_lens: torch.Tensor,
    pad_id: int,
) -> torch.Tensor:
    """Cross-entropy loss masked to response tokens only.

    logits:      (B, T, V)
    tokens:      (B, T)   — full sequence (prefix + response + padding)
    prefix_lens: (B,)     — number of prefix control tokens per example
    """
    B, T, V = logits.shape

    # Shift: logits[t] predicts tokens[t+1]
    shift_logits  = logits[:, :-1, :].contiguous()    # (B, T-1, V)
    shift_targets = tokens[:, 1:].contiguous()         # (B, T-1)

    # Mask: compute loss only where position >= prefix_len (i.e. response tokens)
    # Position i in shift_targets corresponds to predicting token at i+1,
    # so the response starts at i >= prefix_len - 1.
    pos = torch.arange(T - 1, device=tokens.device).unsqueeze(0)   # (1, T-1)
    response_mask = pos >= (prefix_lens.unsqueeze(1) - 1)           # (B, T-1)
    pad_mask      = shift_targets != pad_id
    mask          = response_mask & pad_mask                         # (B, T-1)

    n_active = mask.float().sum()
    if n_active == 0:
        return logits.sum() * 0.0   # differentiable zero

    per_token_loss = F.cross_entropy(
        shift_logits.view(-1, V),
        shift_targets.view(-1),
        ignore_index=pad_id,
        reduction="none",
        label_smoothing=0.1,
    ).view(B, T - 1)

    return (per_token_loss * mask.float()).sum() / n_active


# ── LR schedule ───────────────────────────────────────────────────────────────

def cosine_lr(
    step: int, warmup_steps: int, total_steps: int, min_lr: float, max_lr: float
) -> float:
    if step < warmup_steps:
        return max_lr * step / max(warmup_steps, 1)
    progress = (step - warmup_steps) / max(total_steps - warmup_steps, 1)
    return min_lr + 0.5 * (max_lr - min_lr) * (1.0 + math.cos(math.pi * progress))


# ── Main ──────────────────────────────────────────────────────────────────────

def main() -> None:
    ap = argparse.ArgumentParser(description=__doc__,
                                 formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("--dataset",       default="data/chat/build/dataset",
                    help="Directory produced by build_dataset.py")
    ap.add_argument("--output",        default="data/chat/build/checkpoints",
                    help="Directory to save checkpoints")
    ap.add_argument("--epochs",        type=int,   default=60)
    ap.add_argument("--batch-size",    type=int,   default=64)
    ap.add_argument("--lr",            type=float, default=3e-4)
    ap.add_argument("--min-lr",        type=float, default=1e-5)
    ap.add_argument("--warmup-steps",  type=int,   default=200)
    ap.add_argument("--weight-decay",  type=float, default=0.01)
    ap.add_argument("--dropout",       type=float, default=0.10)
    ap.add_argument("--grad-clip",     type=float, default=1.0)
    ap.add_argument("--device",        default="cpu")
    # Model architecture (smaller defaults = faster CPU training)
    ap.add_argument("--d-model",       type=int,   default=128,
                    help="Transformer hidden dimension (default: 128)")
    ap.add_argument("--n-layers",      type=int,   default=4,
                    help="Number of decoder layers (default: 4)")
    ap.add_argument("--n-heads",       type=int,   default=4,
                    help="Number of attention heads (default: 4)")
    ap.add_argument("--d-ff",          type=int,   default=512,
                    help="Feed-forward inner dimension (default: 512)")
    args = ap.parse_args()

    # Device
    if args.device == "cuda" and not torch.cuda.is_available():
        print("CUDA requested but unavailable — falling back to CPU", flush=True)
        args.device = "cpu"
    device = torch.device(args.device)

    data_dir = Path(args.dataset)
    out_dir  = Path(args.output)
    out_dir.mkdir(parents=True, exist_ok=True)

    with open(data_dir / "meta.json") as fh:
        meta = json.load(fh)

    train_seqs = np.load(data_dir / "train_seqs.npy")
    train_pfx  = np.load(data_dir / "train_prefix_lens.npy")
    val_seqs   = np.load(data_dir / "val_seqs.npy")
    val_pfx    = np.load(data_dir / "val_prefix_lens.npy")

    # Weighted sampling: seqs[:, 1] is the archetype control token.
    # Inverse-frequency weights let underrepresented archetypes (e.g. RaidLeader)
    # appear at the same effective rate as the majority classes.
    arch_ids = train_seqs[:, 1].tolist()
    arch_counts = Counter(arch_ids)
    n_classes = len(arch_counts)
    total = len(arch_ids)
    sample_weights = torch.tensor(
        [total / (n_classes * arch_counts[a]) for a in arch_ids], dtype=torch.float32
    )
    sampler = WeightedRandomSampler(sample_weights, num_samples=total, replacement=True)

    train_dl = DataLoader(
        ChatDataset(train_seqs, train_pfx),
        batch_size=args.batch_size, sampler=sampler, drop_last=True,
    )
    val_dl = DataLoader(
        ChatDataset(val_seqs, val_pfx),
        batch_size=args.batch_size,
    )

    model = ChatDecoder(
        vocab_size=meta["vocab_size"],
        d_model=args.d_model,
        n_layers=args.n_layers,
        n_heads=args.n_heads,
        d_ff=args.d_ff,
        max_len=meta["max_len"],
        pad_id=meta["pad_id"],
        dropout=args.dropout,
    ).to(device)

    n_params = sum(p.numel() for p in model.parameters() if p.requires_grad)
    print(f"Model parameters : {n_params:,}")
    print(f"Train batches    : {len(train_dl):,}  Val batches: {len(val_dl):,}")

    optimizer = torch.optim.AdamW(
        model.parameters(), lr=args.lr, weight_decay=args.weight_decay, fused=False,
    )

    total_steps = args.epochs * len(train_dl)
    global_step = 0
    best_val_loss = float("inf")
    pad_id = meta["pad_id"]

    for epoch in range(1, args.epochs + 1):
        # ── Train ──────────────────────────────────────────────────────────────
        model.train()
        train_loss_sum = 0.0
        for tokens, pfx_lens in train_dl:
            tokens   = tokens.to(device)
            pfx_lens = pfx_lens.to(device)

            lr = cosine_lr(global_step, args.warmup_steps, total_steps, args.min_lr, args.lr)
            for pg in optimizer.param_groups:
                pg["lr"] = lr

            logits = model(tokens)
            loss   = masked_cross_entropy(logits, tokens, pfx_lens, pad_id)

            optimizer.zero_grad()
            loss.backward()
            torch.nn.utils.clip_grad_norm_(model.parameters(), args.grad_clip)
            optimizer.step()

            train_loss_sum += loss.item()
            global_step += 1

        # ── Validate ───────────────────────────────────────────────────────────
        model.eval()
        val_loss_sum = 0.0
        with torch.no_grad():
            for tokens, pfx_lens in val_dl:
                tokens   = tokens.to(device)
                pfx_lens = pfx_lens.to(device)
                logits   = model(tokens)
                val_loss_sum += masked_cross_entropy(logits, tokens, pfx_lens, pad_id).item()

        train_loss = train_loss_sum / max(len(train_dl), 1)
        val_loss   = val_loss_sum   / max(len(val_dl), 1)
        train_ppl  = math.exp(min(train_loss, 20))
        val_ppl    = math.exp(min(val_loss,   20))

        print(
            f"Epoch {epoch:3d}/{args.epochs}  "
            f"train {train_loss:.4f} (ppl {train_ppl:6.1f})  "
            f"val {val_loss:.4f} (ppl {val_ppl:6.1f})  "
            f"lr {lr:.2e}",
            flush=True,
        )

        if val_loss < best_val_loss:
            best_val_loss = val_loss
            torch.save(
                {
                    "epoch":           epoch,
                    "model_state":     model.state_dict(),
                    "optimizer_state": optimizer.state_dict(),
                    "val_loss":        val_loss,
                    "meta":            meta,
                    "arch": {
                        "d_model":  args.d_model,
                        "n_layers": args.n_layers,
                        "n_heads":  args.n_heads,
                        "d_ff":     args.d_ff,
                    },
                },
                out_dir / "best.pt",
            )
            print(f"  → checkpoint saved (val_loss={val_loss:.4f})", flush=True)

    print(f"\nTraining complete.  Best val loss: {best_val_loss:.4f}")


if __name__ == "__main__":
    main()
