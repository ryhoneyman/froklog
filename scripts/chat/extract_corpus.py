#!/usr/bin/env python3
"""extract_corpus.py — extract (speaker, archetype, text, context) triples
from EverQuest log files for neural chat model training.

Each output record (JSONL) has:
  timestamp   : ISO-8601 string
  speaker     : character name (resolved for "You" lines)
  archetype   : personality archetype label
  channel     : "group" | "say" | "shout" | "ooc" | "general" | "voice"
  text        : cleaned utterance
  context     : list of up to WINDOW_SECS seconds of prior game events
  source_file : filename the line came from

Archetype labels are derived from speaker name via ARCHETYPE_MAP.
Unlabelled speakers get archetype "Unknown" and are excluded from training
by default (use --include-unknown to keep them).

Usage:
    python scripts/extract_corpus.py --logs logs/
    python scripts/extract_corpus.py --logs logs/ --output data/chat/build/corpus.jsonl
    python scripts/extract_corpus.py --logs logs/eqlog_Icestorm_test.txt --stats
"""

import argparse
import json
import re
import sys
from collections import deque
from datetime import datetime
from pathlib import Path

# ── Regex patterns ─────────────────────────────────────────────────────────────

# EQ log timestamp: [Day Mon DD HH:MM:SS YYYY]
_TS = r"\[(?P<ts>[A-Za-z]+ [A-Za-z]+ +\d+ \d+:\d+:\d+ \d+)\]"

# Chat line patterns — order matters (most specific first)
CHAT_PATTERNS: list[tuple[str, str, re.Pattern]] = [
    # Guild chat: Playername tells the guild, '...'
    ("guild", "other",
     re.compile(_TS + r" (?P<speaker>\w+) tells the guild, ['\"](?P<text>.+?)['\"]$")),
    # Self guild chat: You say to your guild, '...'
    ("guild", "self",
     re.compile(_TS + r" You say to your guild, ['\"](?P<text>.+?)['\"]$")),
    # Other player group chat: Talodar tells the group, '...'
    ("group", "other",
     re.compile(_TS + r" (?P<speaker>\w+) tells the group, ['\"](?P<text>.+?)['\"]$")),
    # Self group chat: You tell your party, '...'
    ("group", "self",
     re.compile(_TS + r" You tell your party, ['\"](?P<text>.+?)['\"]$")),
    # Voice-tell group: Name voice-tells the group, "..."
    ("voice", "other",
     re.compile(_TS + r" (?P<speaker>\w+) voice-tells the group, ['\"](?P<text>.+?)['\"]$")),
    # You say
    ("say", "self",
     re.compile(_TS + r" You say, ['\"](?P<text>.+?)['\"]$")),
    # Other says
    ("say", "other",
     re.compile(_TS + r" (?P<speaker>\w+) says, ['\"](?P<text>.+?)['\"]$")),
    # You shout
    ("shout", "self",
     re.compile(_TS + r" You shout, ['\"](?P<text>.+?)['\"]$")),
    # Other shouts
    ("shout", "other",
     re.compile(_TS + r" (?P<speaker>\w+) shouts, ['\"](?P<text>.+?)['\"]$")),
    # OOC
    ("ooc", "self",
     re.compile(_TS + r" You say out of character, ['\"](?P<text>.+?)['\"]$")),
    ("ooc", "other",
     re.compile(_TS + r" (?P<speaker>\w+) says out of character, ['\"](?P<text>.+?)['\"]$")),
]

# Game event patterns — these go into the context window
GAME_EVENT_PATTERNS: list[tuple[str, re.Pattern]] = [
    ("slay",
     re.compile(_TS + r" (?:(?P<killer>\w+) has slain|You have slain) (?P<mob>.+)!")),
    ("death",
     re.compile(_TS + r" (?P<player>\w+) has been slain by (?P<mob>.+)!")),
    ("death_self",
     re.compile(_TS + r" You have been slain by (?P<mob>.+)!")),
    ("resist",
     re.compile(_TS + r" Your (?P<spell>.+?) spell did not take hold")),
    ("resist_other",
     re.compile(_TS + r" (?P<spell>.+?) hit (?P<mob>.+?) for \d+ points? of \w+ damage\.")),
    ("cast",
     re.compile(_TS + r" (?P<caster>You begin|(?P<name>\w+) begins) casting (?P<spell>.+?)\.")),
    ("big_hit",
     re.compile(_TS + r" (?P<src>You|(?P<name>\w+)) (?:hit|struck) (?P<tgt>.+?) for (?P<dmg>\d+) points?")),
    ("loot",
     re.compile(_TS + r" (?:You|(?P<name>\w+)) (?:have looted|looted) (?:a|an|the) (?P<item>.+?) from")),
    ("zone",
     re.compile(_TS + r" You have entered (?P<zone>.+?)\.")),
    ("oom",
     re.compile(_TS + r" (?P<player>You are|(?P<name>\w+) is) out of mana\.")),
    ("level",
     re.compile(_TS + r" (?:You have|(?P<name>\w+) has) reached level (?P<level>\d+)")),
]

# ── Timestamp parsing ──────────────────────────────────────────────────────────

_TS_FMT = "%a %b %d %H:%M:%S %Y"

def parse_ts(ts_str: str) -> datetime:
    """Parse EQ log timestamp string to datetime."""
    return datetime.strptime(ts_str.strip(), _TS_FMT)

# ── Player name from filename ──────────────────────────────────────────────────

def player_from_filename(path: Path) -> str:
    """Extract player name from EQ log filename: eqlog_<Name>_<Server>.txt"""
    stem = path.stem  # e.g. eqlog_Icestorm_test
    parts = stem.split("_")
    # parts[0] == "eqlog", parts[1] == player name
    if len(parts) >= 2 and parts[0].lower() == "eqlog":
        return parts[1]
    return "Unknown"

# ── Single-file extraction ─────────────────────────────────────────────────────

def extract_from_file(
    path: Path,
    window_secs: int,
    include_unknown: bool,
    archetype_map: dict[str, str],
) -> list[dict]:
    """Extract all labelled chat triples from a single log file."""
    owner = player_from_filename(path)
    records: list[dict] = []

    # Rolling buffer of (datetime, event_kind, event_summary) for context
    event_buffer: deque[tuple[datetime, str, str]] = deque()

    with path.open(encoding="utf-8", errors="replace") as fh:
        for raw_line in fh:
            line = raw_line.rstrip()
            if not line:
                continue

            # ── Try game event patterns first (cheap — just accumulate) ──────
            game_matched = False
            for kind, pat in GAME_EVENT_PATTERNS:
                m = pat.match(line)
                if m:
                    try:
                        ts = parse_ts(m.group("ts"))
                    except ValueError:
                        break
                    # Build a terse summary string for the context list
                    summary = _summarise_event(kind, m)
                    event_buffer.append((ts, kind, summary))
                    game_matched = True
                    break

            # ── Try chat patterns ─────────────────────────────────────────────
            for channel, who, pat in CHAT_PATTERNS:
                m = pat.match(line)
                if not m:
                    continue

                try:
                    ts = parse_ts(m.group("ts"))
                except ValueError:
                    break

                # Resolve speaker
                if who == "self":
                    speaker = owner
                else:
                    speaker = m.group("speaker")

                text = m.group("text").strip()
                if not text:
                    break

                # Archetype label
                archetype = archetype_map.get(speaker, "Unknown")
                if archetype == "Unknown" and not include_unknown:
                    break

                # Prune stale events from the buffer
                cutoff = ts.timestamp() - window_secs
                while event_buffer and event_buffer[0][0].timestamp() < cutoff:
                    event_buffer.popleft()

                context = [
                    {"kind": k, "summary": s}
                    for (_, k, s) in event_buffer
                ]

                records.append({
                    "timestamp": ts.isoformat(),
                    "speaker": speaker,
                    "archetype": archetype,
                    "channel": channel,
                    "text": text,
                    "context": context,
                    "source_file": path.name,
                })
                break  # only one chat pattern per line

    return records


def _summarise_event(kind: str, m: re.Match) -> str:
    """Build a short human-readable summary of a game event match."""
    try:
        if kind == "slay":
            killer = m.group("killer") or "You"
            return f"{killer} slew {m.group('mob')}"
        if kind in ("death", "death_self"):
            victim = m.group("player") if kind == "death" else "You"
            return f"{victim} slain by {m.group('mob')}"
        if kind == "resist":
            return f"resist: {m.group('spell')}"
        if kind == "cast":
            caster = m.group("name") or "You"
            return f"{caster} casting {m.group('spell')}"
        if kind == "big_hit":
            src = m.group("name") or "You"
            return f"{src} hit {m.group('tgt')} for {m.group('dmg')}"
        if kind == "loot":
            who = m.group("name") or "You"
            return f"{who} looted {m.group('item')}"
        if kind == "zone":
            return f"zone: {m.group('zone')}"
        if kind == "oom":
            who = m.group("name") or "You"
            return f"{who} OOM"
        if kind == "level":
            who = m.group("name") or "You"
            return f"{who} reached level {m.group('level')}"
    except IndexError:
        pass
    return kind

# ── Multi-file extraction ──────────────────────────────────────────────────────

def collect_log_files(path: Path) -> list[Path]:
    """Return all EQ log files under path (file or directory)."""
    if path.is_file():
        return [path]
    return sorted(path.glob("eqlog_*.txt"))

# ── Stats output ───────────────────────────────────────────────────────────────

def print_stats(records: list[dict], archetype_map: dict[str, str]) -> None:
    from collections import Counter
    total = len(records)
    by_arch = Counter(r["archetype"] for r in records)
    by_chan = Counter(r["channel"] for r in records)
    by_speaker = Counter(r["speaker"] for r in records)

    print(f"Total records : {total:,}")
    print()
    print("By archetype:")
    for arch, n in by_arch.most_common():
        print(f"  {arch:<20} {n:>6,}  ({n/total*100:.1f}%)")
    print()
    print("By channel:")
    for chan, n in by_chan.most_common():
        print(f"  {chan:<20} {n:>6,}")
    print()
    print("Top speakers:")
    for spk, n in by_speaker.most_common(15):
        arch = archetype_map.get(spk, "Unknown")
        print(f"  {spk:<20} {n:>6,}  [{arch}]")

# ── CLI ────────────────────────────────────────────────────────────────────────

def main() -> None:
    ap = argparse.ArgumentParser(description=__doc__,
                                 formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("--logs", required=True,
                    help="Path to a log file or directory of log files")
    ap.add_argument("--archetype-map", default="data/chat/build/archetype_map.json",
                    help="JSON file mapping speaker names to archetypes (default: data/chat/build/archetype_map.json)")
    ap.add_argument("--output", default="-",
                    help="Output JSONL path (default: stdout)")
    ap.add_argument("--window", type=int, default=60,
                    help="Context window in seconds before each chat line (default: 60)")
    ap.add_argument("--include-unknown", action="store_true",
                    help="Include utterances from speakers not in the archetype map")
    ap.add_argument("--stats", action="store_true",
                    help="Print per-archetype statistics instead of writing JSONL")
    ap.add_argument("--channels", nargs="+",
                    default=["guild", "group", "say", "voice", "ooc"],
                    help="Channels to include (default: guild group say voice ooc)")
    args = ap.parse_args()

    map_path = Path(args.archetype_map)
    if map_path.exists():
        archetype_map: dict[str, str] = json.loads(map_path.read_text(encoding="utf-8"))
    else:
        archetype_map = {}
        print(f"note: {map_path} not found — all speakers treated as Unknown", file=sys.stderr)
        print(f"      run with --include-unknown, then use analyze_speakers.py --update-map to create it", file=sys.stderr)

    log_path = Path(args.logs)
    if not log_path.exists():
        print(f"error: {log_path} does not exist", file=sys.stderr)
        sys.exit(1)

    files = collect_log_files(log_path)
    if not files:
        print(f"error: no eqlog_*.txt files found under {log_path}", file=sys.stderr)
        sys.exit(1)

    all_records: list[dict] = []
    for f in files:
        recs = extract_from_file(f, args.window, args.include_unknown, archetype_map)
        # Filter by channel
        recs = [r for r in recs if r["channel"] in args.channels]
        all_records.extend(recs)
        print(f"  {f.name}: {len(recs):,} records", file=sys.stderr)

    print(f"Total: {len(all_records):,} records", file=sys.stderr)

    if args.stats:
        print_stats(all_records, archetype_map)
        return

    if args.output == "-":
        out = sys.stdout
        for rec in all_records:
            out.write(json.dumps(rec) + "\n")
    else:
        out_path = Path(args.output)
        out_path.parent.mkdir(parents=True, exist_ok=True)
        with out_path.open("w", encoding="utf-8") as fh:
            for rec in all_records:
                fh.write(json.dumps(rec) + "\n")
        print(f"Wrote {len(all_records):,} records to {out_path}", file=sys.stderr)


if __name__ == "__main__":
    main()
