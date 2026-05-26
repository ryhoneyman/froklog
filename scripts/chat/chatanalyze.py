#!/usr/bin/env python3
"""chatanalyze.py — extract and correlate group/raid chat from EQ log files.

Reads a raw EverQuest log and produces structured JSONL output by correlating
chat messages against nearby game events using a sliding time window.

Modes:
  correlated  (default) — each chat line annotated with nearest game event
  corpus      — flat speaker+channel+text pairs for fragment extraction
  stats       — per-speaker summary counts and timing distributions

The log owner's lines ("You tell your party/raid, '...'") are attributed to
the player name derived from the filename (eqlog_<Name>_*) or --player-name.

Usage:
    python scripts/chat/chatanalyze.py --input eqlog_Icestorm_test.txt
    python scripts/chat/chatanalyze.py --input eqlog.txt --player-name Icestorm --mode stats
    python scripts/chat/chatanalyze.py --input eqlog.txt --mode corpus --speaker Talodar
"""

import argparse
import json
import re
import sys
from collections import deque, defaultdict
from datetime import datetime
from pathlib import Path

# ── Timestamp parsing ─────────────────────────────────────────────────────────

_TS_RE = re.compile(r"^\[([A-Za-z]+ [A-Za-z]+ +\d+ \d+:\d+:\d+ \d+)\] ")
_TS_FMT = "%a %b %d %H:%M:%S %Y"


def _parse_ts(line: str) -> datetime | None:
    m = _TS_RE.match(line)
    if not m:
        return None
    try:
        return datetime.strptime(m.group(1).strip(), _TS_FMT)
    except ValueError:
        return None


def _body(line: str) -> str | None:
    m = _TS_RE.match(line)
    return line[m.end():] if m else None


# ── Player name extraction ────────────────────────────────────────────────────

def _name_from_filename(path: Path) -> str | None:
    stem = path.stem
    if not stem.startswith("eqlog_"):
        return None
    after = stem[len("eqlog_"):]
    name = after.split("_")[0]
    return name or None


# ── Chat line parsing ─────────────────────────────────────────────────────────

def _parse_chat(body: str, player_name: str) -> tuple[str, str, str] | None:
    """Returns (speaker, channel, text) or None."""
    if body.startswith("You tell your "):
        rest = body[len("You tell your "):]
        if ", '" not in rest:
            return None
        channel_raw, rest = rest.split(", '", 1)
        channel = {"party": "group", "raid": "raid"}.get(channel_raw)
        if channel is None:
            return None
        return (player_name, channel, rest.rstrip("'"))

    if " tells the " not in body:
        return None
    speaker, rest = body.split(" tells the ", 1)
    if " " in speaker:
        return None
    if ", '" not in rest:
        return None
    channel_raw, rest = rest.split(", '", 1)
    channel = {"group": "group", "raid": "raid"}.get(channel_raw)
    if channel is None:
        return None
    return (speaker, channel, rest.rstrip("'"))


# ── Game event detection ──────────────────────────────────────────────────────

def _classify_event(body: str) -> tuple[str, str] | None:
    """Returns (kind, detail) or None."""
    if " has been slain by " in body:
        return ("slay", body[: body.index(" has been slain by ")])
    if body.startswith("You have slain "):
        return ("slay", body[len("You have slain "):].rstrip("!"))
    if body.startswith("You have been slain by "):
        return ("player_death", body[len("You have been slain by "):].rstrip("!"))
    if body.endswith(" died."):
        return ("player_death", body[: -len(" died.")])
    if " resisted " in body or body.startswith("You resist "):
        return ("resist", "")
    if body.startswith("--You have looted "):
        return ("item_loot", body[len("--You have looted "):].split(" from")[0])
    if body.startswith("You receive ") and " from " in body:
        return ("item_loot", body[len("You receive "):].split(" from")[0])
    if body.startswith("You gain") and "experience" in body:
        return ("exp_gain", "")
    return None


# ── Message classification ────────────────────────────────────────────────────

_RL_KEYWORDS = {
    "brb", "afk", "back", "bio", "bathroom", "phone", "irl", "food",
    "snack", "drink", "water", "eat", "pizza", "waffle", "kitchen",
    "wife", "gf", "boyfriend", "husband", "kid", "dog", "cat", "baby",
    "stepping out", "gonna grab", "gotta go", "distract", "interrupt",
    "someone at the", "at the door",
}

_TACTICAL_KEYWORDS = {
    "incoming", "inc", "pulling", "pull", "slow", "mezz", "mez", "mezzing",
    "root", "rooting", "charm", "fd", "feign", "camp", "safe", "clear",
    "assist", "adds", "add", "train", "aggro", "agro", "snare", "kite",
    "burn", "main assist", "don't break", "dont break", "hold cc",
}


def _kw_match(lower: str, kw: str) -> bool:
    return (lower == kw
            or lower.startswith(kw + " ")
            or lower.endswith(" " + kw)
            or (" " + kw + " ") in lower)


def _classify_message(text: str, has_recent_event: bool) -> str:
    lower = text.lower()
    words = lower.split()

    # Loot bid: 2-7 words, last token is a signed integer, first word capitalized
    if 2 <= len(words) <= 7:
        last = words[-1].lstrip("+-")
        orig_words = text.split()
        if last.isdigit() and orig_words and orig_words[0][:1].isupper():
            return "loot"

    for kw in _RL_KEYWORDS:
        if _kw_match(lower, kw):
            return "real_life"

    for kw in _TACTICAL_KEYWORDS:
        if _kw_match(lower, kw):
            return "tactical"

    if has_recent_event:
        return "reactive"

    return "social"


# ── Core pass ─────────────────────────────────────────────────────────────────

def _process(path: Path, player_name: str, window_secs: int, speaker_filter: str | None):
    """Yields (ts, speaker, channel, text, msg_class, recent_event_or_None) per chat line."""
    recent: deque[tuple[datetime, str, str]] = deque()  # (ts, kind, detail)

    with path.open(encoding="utf-8", errors="replace") as fh:
        for raw in fh:
            line = raw.rstrip()
            ts = _parse_ts(line)
            if ts is None:
                continue
            body = _body(line)
            if body is None:
                continue

            chat = _parse_chat(body, player_name)
            if chat:
                spk, chan, text = chat
                if speaker_filter and spk != speaker_filter:
                    continue

                cutoff = ts.timestamp() - window_secs
                while recent and recent[0][0].timestamp() < cutoff:
                    recent.popleft()

                last_event = recent[-1] if recent else None
                has_recent = last_event is not None
                msg_class = _classify_message(text, has_recent)
                yield ts, spk, chan, text, msg_class, last_event
            else:
                ev = _classify_event(body)
                if ev:
                    kind, detail = ev
                    cutoff = ts.timestamp() - window_secs
                    while recent and recent[0][0].timestamp() < cutoff:
                        recent.popleft()
                    recent.append((ts, kind, detail))


# ── Modes ─────────────────────────────────────────────────────────────────────

def mode_correlated(path: Path, player_name: str, window: int,
                    speaker: str | None, out) -> None:
    for ts, spk, chan, text, msg_class, ev in _process(path, player_name, window, speaker):
        rec: dict = {
            "ts": int(ts.timestamp()),
            "speaker": spk,
            "channel": chan,
            "text": text,
            "msg_class": msg_class,
        }
        if ev:
            ev_ts, ev_kind, ev_detail = ev
            rec["event_kind"] = ev_kind
            rec["event_detail"] = ev_detail
            rec["event_lag_secs"] = int(ts.timestamp() - ev_ts.timestamp())
        out.write(json.dumps(rec) + "\n")


def mode_corpus(path: Path, player_name: str, window: int,
                speaker: str | None, out) -> None:
    for ts, spk, chan, text, _cls, _ev in _process(path, player_name, window, speaker):
        out.write(json.dumps({"speaker": spk, "channel": chan, "text": text}) + "\n")


def mode_stats(path: Path, player_name: str, window: int,
               speaker: str | None, out) -> None:
    stats: dict[str, dict] = defaultdict(lambda: {
        "total": 0, "reactive": 0, "rl_meta": 0, "tactical": 0,
        "loot": 0, "social": 0, "total_len": 0, "reaction_lags": [],
    })

    for ts, spk, chan, text, msg_class, ev in _process(path, player_name, window, speaker):
        s = stats[spk]
        s["total"] += 1
        s["total_len"] += len(text)
        if msg_class == "reactive":
            s["reactive"] += 1
            if ev:
                s["reaction_lags"].append(int(ts.timestamp() - ev[0].timestamp()))
        elif msg_class == "real_life":
            s["rl_meta"] += 1
        elif msg_class == "tactical":
            s["tactical"] += 1
        elif msg_class == "loot":
            s["loot"] += 1
        else:
            s["social"] += 1

    for spk, s in sorted(stats.items(), key=lambda x: -x[1]["total"]):
        avg_len = s["total_len"] / s["total"] if s["total"] else 0.0
        lags = sorted(s["reaction_lags"])
        median_lag = lags[len(lags) // 2] if lags else 0
        rec = {
            "speaker": spk,
            "total": s["total"],
            "reactive": s["reactive"],
            "rl_meta": s["rl_meta"],
            "tactical": s["tactical"],
            "loot": s["loot"],
            "social": s["social"],
            "avg_msg_len": f"{avg_len:.1f}",
            "median_reaction_secs": median_lag,
        }
        out.write(json.dumps(rec) + "\n")


# ── CLI ───────────────────────────────────────────────────────────────────────

def main() -> None:
    ap = argparse.ArgumentParser(
        description=__doc__,
        formatter_class=argparse.RawDescriptionHelpFormatter,
    )
    ap.add_argument("--input", "-i", required=True, type=Path,
                    help="Input EQ log file")
    ap.add_argument("--player-name",
                    help="Name of the log-owning player (defaults to extraction from filename)")
    ap.add_argument("--window", type=int, default=30,
                    help="Lookback window in seconds for event correlation (default: 30)")
    ap.add_argument("--mode", choices=["correlated", "corpus", "stats"],
                    default="correlated",
                    help="Output mode (default: correlated)")
    ap.add_argument("--speaker",
                    help="Filter to a single speaker (optional)")
    ap.add_argument("--output", "-o", type=Path,
                    help="Output file (default: stdout)")
    args = ap.parse_args()

    if not args.input.exists():
        print(f"error: {args.input} does not exist", file=sys.stderr)
        sys.exit(1)

    player_name = (args.player_name
                   or _name_from_filename(args.input)
                   or "You")

    if args.output:
        args.output.parent.mkdir(parents=True, exist_ok=True)
        out = args.output.open("w", encoding="utf-8")
    else:
        out = sys.stdout

    try:
        if args.mode == "correlated":
            mode_correlated(args.input, player_name, args.window, args.speaker, out)
        elif args.mode == "corpus":
            mode_corpus(args.input, player_name, args.window, args.speaker, out)
        else:
            mode_stats(args.input, player_name, args.window, args.speaker, out)
    finally:
        if args.output:
            out.close()


if __name__ == "__main__":
    try:
        main()
    except BrokenPipeError:
        sys.exit(0)
