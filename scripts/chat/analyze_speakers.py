#!/usr/bin/env python3
"""analyze_speakers.py — infer archetype labels for unlabelled speakers.

Extracts per-speaker chat features from a corpus JSONL, scores them against
the known archetype personality profiles, and prints ranked suggestions.

Features extracted per speaker:
  verbosity  — normalised average word count per message
  humor      — fraction of messages containing laughter / levity markers
  patience   — inverse of exclamation + rapid-fire short-burst patterns

These map to the (verbosity, humor, patience) personality dimensions from
ARCHETYPE_PERSONALITY and are scored by nearest-archetype Euclidean distance.

Usage:
    python scripts/chat/analyze_speakers.py \\
        --corpus data/chat/build/corpus.jsonl
    python scripts/chat/analyze_speakers.py \\
        --corpus data/chat/build/corpus.jsonl --min-msgs 10 --all-speakers
    python scripts/chat/analyze_speakers.py \\
        --corpus data/chat/build/corpus.jsonl --suggest-map
    python scripts/chat/analyze_speakers.py \\
        --corpus data/chat/build/corpus.jsonl --top 3
"""

import argparse
import json
import math
import re
from collections import defaultdict
from pathlib import Path

# ── Archetype personality reference ───────────────────────────────────────────
# Mirrors ARCHETYPE_PERSONALITY in build_dataset.py — keep in sync.

ARCHETYPES: dict[str, dict[str, float]] = {
    "QuietAnchor":      {"verbosity": 0.28, "humor": 0.40, "patience": 0.78},
    "ChaoticNarrator":  {"verbosity": 0.78, "humor": 0.65, "patience": 0.58},
    "RaidLeader":       {"verbosity": 0.48, "humor": 0.30, "patience": 0.72},
    "ReactiveObserver": {"verbosity": 0.18, "humor": 0.25, "patience": 0.80},
    "TacticalFocused":  {"verbosity": 0.50, "humor": 0.20, "patience": 0.70},
}

_HUMOR_RE = re.compile(
    r"\b(lol|lmao|lmfao|rofl|haha|hehe|heh|xd|kek|gg)\b"
    r"|(?<!\w):[-o]?[)DdPp3]"   # :) :D :P :3
    r"|(?<!\w);[-]?[)]",         # ;)
    re.IGNORECASE,
)

_CAPS_WORD_RE = re.compile(r"\b[A-Z]{3,}\b")   # ALL-CAPS words (skip initials)

_DIMS = ["verbosity", "humor", "patience"]


# ── Feature extraction ────────────────────────────────────────────────────────

def _word_count(text: str) -> int:
    return len(text.split())


def extract_raw(msgs: list[str]) -> dict[str, float]:
    n = len(msgs)
    avg_words    = sum(_word_count(m) for m in msgs) / n
    humor_frac   = sum(1 for m in msgs if _HUMOR_RE.search(m)) / n
    exclaim_frac = sum(1 for m in msgs if m.rstrip().endswith("!")) / n
    caps_frac    = sum(1 for m in msgs if _CAPS_WORD_RE.search(m)) / n
    return {
        "avg_words":    avg_words,
        "humor_frac":   humor_frac,
        "exclaim_frac": exclaim_frac,
        "caps_frac":    caps_frac,
        "n_msgs":       float(n),
    }


def to_personality(raw: dict[str, float], max_avg_words: float) -> dict[str, float]:
    """Map raw features to [0, 1] personality dimensions."""
    verbosity = min(raw["avg_words"] / max(max_avg_words, 1.0), 1.0)

    # Humor: laughter markers are strong signal; caps / exclamations add levity
    humor = min(raw["humor_frac"] * 2.5
                + raw["exclaim_frac"] * 0.4
                + raw["caps_frac"] * 0.3,
                1.0)

    # Patience: high exclamation + caps usage suggests lower patience
    patience = max(0.0, min(1.0, 0.92
                            - raw["exclaim_frac"] * 0.7
                            - raw["caps_frac"] * 0.3))

    return {"verbosity": verbosity, "humor": humor, "patience": patience}


# ── Scoring ───────────────────────────────────────────────────────────────────

def rank_archetypes(features: dict[str, float]) -> list[tuple[str, float]]:
    """Return archetypes sorted by ascending Euclidean distance (best first)."""
    results = []
    for arch, profile in ARCHETYPES.items():
        dist = math.sqrt(sum((features[d] - profile[d]) ** 2 for d in _DIMS))
        results.append((arch, dist))
    return sorted(results, key=lambda x: x[1])


# ── Main ──────────────────────────────────────────────────────────────────────

def main() -> None:
    ap = argparse.ArgumentParser(description=__doc__,
                                 formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("--corpus", required=True,
                    help="JSONL corpus from extract_corpus.py")
    ap.add_argument("--archetype-map", default="data/chat/build/archetype_map.json",
                    help="JSON file mapping speaker names to archetypes (default: data/chat/build/archetype_map.json)")
    ap.add_argument("--min-msgs", type=int, default=15,
                    help="Minimum messages per speaker to analyse (default: 15)")
    ap.add_argument("--top", type=int, default=len(ARCHETYPES),
                    help="Number of ranked archetype suggestions per speaker (default: all)")
    ap.add_argument("--all-speakers", action="store_true",
                    help="Include already-labelled speakers (useful for sanity-checking)")
    ap.add_argument("--suggest-map", action="store_true",
                    help="Print suggested new entries to stdout")
    ap.add_argument("--update-map", action="store_true",
                    help="Write suggested new entries directly into the archetype map file")
    args = ap.parse_args()

    map_path = Path(args.archetype_map)
    archetype_map: dict[str, str] = {}
    if map_path.exists():
        archetype_map = json.loads(map_path.read_text(encoding="utf-8"))

    records: list[dict] = []
    with open(args.corpus, encoding="utf-8") as fh:
        for line in fh:
            records.append(json.loads(line.strip()))

    msgs_by_speaker: dict[str, list[str]] = defaultdict(list)

    for rec in records:
        spk = rec["speaker"]
        msgs_by_speaker[spk].append(rec["text"])

    # Reference max_avg_words across all qualified speakers for normalisation
    qualified = {
        spk: msgs
        for spk, msgs in msgs_by_speaker.items()
        if len(msgs) >= args.min_msgs
    }
    if not qualified:
        print("No speakers meet --min-msgs threshold.")
        return

    avgs = [sum(_word_count(m) for m in msgs) / len(msgs)
            for msgs in qualified.values()]
    max_avg = max(avgs) if avgs else 20.0

    # ── Table ─────────────────────────────────────────────────────────────────
    print(f"{'Speaker':<20} {'N':>5}  {'vb':>4} {'hm':>4} {'pt':>4}  Suggestions (dist)")
    print("─" * 80)

    new_entries: dict[str, str] = {}

    for spk, msgs in sorted(qualified.items(), key=lambda x: -len(x[1])):
        current = archetype_map.get(spk, "Unknown")
        if not args.all_speakers and current != "Unknown":
            continue

        raw      = extract_raw(msgs)
        features = to_personality(raw, max_avg)
        ranked   = rank_archetypes(features)

        label = f"[{current}]" if current != "Unknown" else ""
        top_str = "  ".join(
            f"{arch}({dist:.2f})" for arch, dist in ranked[: args.top]
        )
        print(
            f"{spk:<20} {int(raw['n_msgs']):>5}  "
            f"{features['verbosity']:.2f} {features['humor']:.2f} {features['patience']:.2f}  "
            f"{top_str}  {label}"
        )

        if current == "Unknown":
            new_entries[spk] = ranked[0][0]

    print()
    print("Columns: vb=verbosity  hm=humor  pt=patience  (dist lower = closer match)")

    if not new_entries:
        return

    # ── Suggest / update ──────────────────────────────────────────────────────
    if args.suggest_map:
        print()
        print("# Suggested new entries for archetype map:")
        for spk, arch in sorted(new_entries.items(), key=lambda x: x[1]):
            print(f'  "{spk}": "{arch}"')

    if args.update_map:
        archetype_map.update(new_entries)
        map_path.write_text(
            json.dumps(archetype_map, indent=2) + "\n", encoding="utf-8"
        )
        print(f"\nWrote {len(new_entries)} new entry/entries to {map_path}:")
        for spk, arch in sorted(new_entries.items()):
            print(f"  {spk} → {arch}")


if __name__ == "__main__":
    main()
