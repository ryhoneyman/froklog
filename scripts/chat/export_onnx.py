#!/usr/bin/env python3
"""export_onnx.py — export the trained ChatDecoder to ONNX for Rust (ort) inference.

Exported model signature:
  Input  — tokens  : int64  [1, seq_len]   (dynamic seq_len axis)
  Output — logits  : float32 [1, seq_len, vocab_size]

Autoregressive decoding in the Rust NeuralBackend:
  1. Build the prefix token sequence (control IDs from model_meta.json).
  2. Run the model to obtain logits for the last non-pad position.
  3. Apply temperature scaling + top-k filter, sample the next token.
  4. Append the sampled token and repeat until EOS or max_new_tokens.

Outputs (in same directory as --output):
  model.onnx         — the ONNX graph
  model_meta.json    — control token IDs and vocab metadata for the Rust runtime
  vocab.json         — BPE token→id map (copied from data/vocab.json if present)

Usage:
    python scripts/export_onnx.py --checkpoint data/chat/build/checkpoints/best.pt
    python scripts/export_onnx.py --checkpoint data/chat/build/checkpoints/best.pt \
        --output data/chat/models/v1/model.onnx --simplify
"""

import argparse
import json
import shutil
import sys
from pathlib import Path

import torch

sys.path.insert(0, str(Path(__file__).parent))
from model import ChatDecoder


def main() -> None:
    ap = argparse.ArgumentParser(description=__doc__,
                                 formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("--checkpoint", required=True,
                    help="Path to best.pt produced by train.py")
    ap.add_argument("--output", default="data/chat/models/v1/model.onnx",
                    help="Output ONNX file path (default: data/chat/models/v1/model.onnx)")
    ap.add_argument("--vocab", default="data/chat/build/vocab.model",
                    help="SentencePiece .model file to copy alongside the exported model")
    ap.add_argument("--simplify", action="store_true",
                    help="Run onnx-simplifier after export (pip install onnxsim)")
    ap.add_argument("--opset", type=int, default=17,
                    help="ONNX opset version (default: 17)")
    args = ap.parse_args()

    ckpt = torch.load(args.checkpoint, map_location="cpu", weights_only=False)
    meta = ckpt["meta"]
    arch = ckpt.get("arch", {})   # absent in old checkpoints → use model defaults

    model = ChatDecoder(
        vocab_size=meta["vocab_size"],
        d_model=arch.get("d_model", 128),
        n_layers=arch.get("n_layers", 4),
        n_heads=arch.get("n_heads", 4),
        d_ff=arch.get("d_ff", 512),
        max_len=meta["max_len"],
        pad_id=meta["pad_id"],
    )
    model.load_state_dict(ckpt["model_state"], strict=False)
    model.eval()

    n_params = sum(p.numel() for p in model.parameters())
    print(f"Model parameters : {n_params:,}")

    out_path = Path(args.output)
    out_path.parent.mkdir(parents=True, exist_ok=True)

    # Dummy input with a short sequence so ONNX traces cleanly
    dummy = torch.zeros(1, 16, dtype=torch.long)

    print(f"Exporting to {out_path} (opset {args.opset})...")
    torch.onnx.export(
        model,
        dummy,
        str(out_path),
        export_params=True,
        opset_version=args.opset,
        do_constant_folding=True,
        input_names=["tokens"],
        output_names=["logits"],
        dynamic_axes={
            "tokens": {1: "seq_len"},
            "logits": {1: "seq_len"},
        },
    )
    print("Export complete.")

    # Optional simplification
    if args.simplify:
        try:
            import onnx
            import onnxsim

            model_onnx = onnx.load(str(out_path))
            model_simplified, ok = onnxsim.simplify(model_onnx)
            if ok:
                onnx.save(model_simplified, str(out_path))
                print("onnxsim simplification successful.")
            else:
                print("onnxsim check failed — original export kept.")
        except ImportError:
            print("onnxsim not installed; skipping (pip install onnxsim).")

    # Copy SentencePiece model alongside the ONNX file.
    # The Rust sentencepiece-rs crate needs the binary .model file, not vocab.json.
    vocab_src = Path(args.vocab)
    vocab_dst = out_path.parent / "vocab.model"
    if vocab_src.exists():
        shutil.copy(vocab_src, vocab_dst)
        print(f"Copied vocab     : {vocab_dst}")
    else:
        print(f"Warning: vocab file not found at {vocab_src} — copy manually.")

    # Write runtime metadata for the Rust NeuralBackend
    rt_meta = {
        "vocab_size":     meta["vocab_size"],
        "base_vocab":     meta["base_vocab"],
        "control_tokens": meta["control_tokens"],
        "max_len":        meta["max_len"],
        "pad_id":         meta["pad_id"],
        "bos_id":         meta["bos_id"],
        "eos_id":         meta["eos_id"],
    }
    meta_path = out_path.parent / "model_meta.json"
    with open(meta_path, "w", encoding="utf-8") as fh:
        json.dump(rt_meta, fh, indent=2)

    print(f"\nReady for Rust inference (pass as --model-dir):")
    print(f"  {out_path.parent}/")
    print(f"    model.onnx")
    print(f"    vocab.model")
    print(f"    model_meta.json")


if __name__ == "__main__":
    main()
