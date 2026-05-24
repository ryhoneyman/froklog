#!/usr/bin/env python3
"""build_vocab.py — train a SentencePiece BPE vocabulary on corpus utterances.

Produces:
  <output>.model  — SentencePiece binary model (used by build_dataset.py / eval.py)
  <output>.json   — token→id mapping for the Rust runtime

Usage:
    python scripts/build_vocab.py --corpus data/chat/build/corpus.jsonl
    python scripts/build_vocab.py --corpus data/chat/build/corpus.jsonl --output data/chat/build/vocab --vocab-size 512
"""

import argparse
import json
import tempfile
from pathlib import Path

import sentencepiece as spm

VOCAB_SIZE = 512


def main() -> None:
    ap = argparse.ArgumentParser(description=__doc__,
                                 formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("--corpus", required=True, help="JSONL corpus from extract_corpus.py")
    ap.add_argument("--output", default="data/chat/build/vocab",
                    help="Output path prefix (produces .model and .json, default: data/chat/build/vocab)")
    ap.add_argument("--vocab-size", type=int, default=VOCAB_SIZE,
                    help=f"BPE vocabulary size (default: {VOCAB_SIZE})")
    args = ap.parse_args()

    corpus_path = Path(args.corpus)
    output = Path(args.output)
    output.parent.mkdir(parents=True, exist_ok=True)

    # Extract utterance text to a temp file for SentencePiece trainer
    texts: list[str] = []
    with corpus_path.open(encoding="utf-8") as fh:
        for line in fh:
            rec = json.loads(line)
            t = rec["text"].strip()
            if t:
                texts.append(t)

    with tempfile.NamedTemporaryFile(
        mode="w", suffix=".txt", delete=False, encoding="utf-8"
    ) as tmp:
        tmp.write("\n".join(texts))
        tmp_path = tmp.name

    print(f"Training BPE on {len(texts):,} utterances (vocab_size={args.vocab_size})...")

    spm.SentencePieceTrainer.train(
        input=tmp_path,
        model_prefix=str(output),
        vocab_size=args.vocab_size,
        # Fixed special token IDs expected by model.py / build_dataset.py
        pad_id=0,
        bos_id=1,
        eos_id=2,
        unk_id=3,
        character_coverage=1.0,
        model_type="bpe",
        pad_piece="<pad>",
        bos_piece="<bos>",
        eos_piece="<eos>",
        unk_piece="<unk>",
    )

    # Export plain JSON token→id map for the Rust runtime (ort inference)
    sp = spm.SentencePieceProcessor()
    sp.load(str(output) + ".model")
    vocab = {sp.id_to_piece(i): i for i in range(sp.get_piece_size())}

    json_path = str(output) + ".json"
    with open(json_path, "w", encoding="utf-8") as fh:
        json.dump(vocab, fh, ensure_ascii=False, indent=2)

    print(f"BPE vocab size : {sp.get_piece_size()}")
    print(f"Saved model    : {output}.model")
    print(f"Saved vocab    : {json_path}")


if __name__ == "__main__":
    main()
