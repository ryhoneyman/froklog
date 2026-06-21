# Chat Neural Pipeline — scripts/chat

Tools for extracting, labelling, training, and analyzing EverQuest group/raid chat.
The pipeline runs in three phases: **explore** a log, **label** speakers, then **train**.

---

## Phase 1 — Explore a log file (chatanalyze.py)

`chatanalyze.py` reads a single raw EQ log and produces JSONL.  Use it to get
a feel for what's in a new log before committing anything to the training pipeline.
It has three modes.

### Mode: stats — who is talking and how?

Run this first on any new log to see which speakers are active and how they communicate.

```bash
# Raw
python3 scripts/chat/chatanalyze.py \
    --input logs/eqlog_Talodar_test.txt \
    --mode stats

# just
just chat-chatanalyze --input logs/eqlog_Talodar_test.txt --mode stats
```

One JSONL record per speaker, sorted by total message count:

```json
{"speaker": "Icestorm", "total": 3229, "reactive": 985, "rl_meta": 85, "tactical": 208,
 "loot": 20, "social": 1931, "avg_msg_len": "23.1", "median_reaction_secs": 12}

{"speaker": "Talodar",  "total": 2630, "reactive": 944, "rl_meta": 92, "tactical": 97,
 "loot": 18, "social": 1479, "avg_msg_len": "32.9", "median_reaction_secs": 12}

{"speaker": "Zephon",   "total": 160,  "reactive": 45,  "rl_meta": 5,  "tactical": 1,
 "loot": 2,  "social": 107,  "avg_msg_len": "23.3", "median_reaction_secs": 6}
```

Pipe into `jq` for a quick table:

```bash
python3 scripts/chat/chatanalyze.py \
    --input logs/eqlog_Talodar_test.txt \
    --mode stats \
    | jq -r '[.speaker, .total, .reactive, .tactical, .rl_meta, .avg_msg_len, .median_reaction_secs] | @tsv'
```

### Mode: correlated — what was each message reacting to?

Each chat line is annotated with the nearest preceding game event (kill, loot, death, etc.)
and how many seconds after it the message was sent.  Useful for auditing classifier
behaviour or investigating a specific session.

```bash
# All speakers, print to stdout
python3 scripts/chat/chatanalyze.py \
    --input logs/eqlog_Talodar_test.txt \
    --mode correlated

# Single speaker, save to file
python3 scripts/chat/chatanalyze.py \
    --input logs/eqlog_Talodar_test.txt \
    --speaker Talodar \
    --output /tmp/talodar_correlated.jsonl

# Wider event window (60 s instead of the default 30 s)
python3 scripts/chat/chatanalyze.py \
    --input logs/eqlog_Talodar_test.txt \
    --window 60

# just — filter to one speaker
just chat-chatanalyze \
    --input logs/eqlog_Talodar_test.txt \
    --mode correlated \
    --speaker Icestorm \
    --output /tmp/icestorm.jsonl
```

Example output — a plain social message, a reactive message 9 s after a loot event,
and a real-life message during the same loot window:

```json
{"ts": 1773010430, "speaker": "Icestorm", "channel": "group",
 "text": "yeah for sure", "msg_class": "social"}

{"ts": 1773010609, "speaker": "Talodar", "channel": "group",
 "text": "scattered and splattered as my grandma would say",
 "msg_class": "reactive", "event_kind": "item_loot",
 "event_detail": "a Velium Gem Dusted Rune", "event_lag_secs": 9}

{"ts": 1773010834, "speaker": "Icestorm", "channel": "group",
 "text": "bish on phone",
 "msg_class": "real_life", "event_kind": "item_loot",
 "event_detail": "a Shrieking Ahlspiess +4", "event_lag_secs": 8}
```

**Message classes:**

| Class | Description |
|---|---|
| `reactive` | Sent within the lookback window after a game event (kill, loot, death, resist, exp) |
| `real_life` | Real-life meta: afk, brb, food, phone, dog, kid, etc. |
| `tactical` | Pull calls, CC instructions, positioning: inc, mez, assist, burn, etc. |
| `loot` | Loot bid format: `Item Name +N` or `Item Name -N` |
| `social` | Everything else |

**Event kinds** (`event_kind` field):

| Value | Trigger |
|---|---|
| `slay` | A mob was killed |
| `player_death` | A player died |
| `resist` | Spell resisted |
| `item_loot` | Item looted |
| `exp_gain` | Experience gained |

### Mode: corpus — flat text dump for a single file

Outputs `{speaker, channel, text}` for every group/raid line.  Useful for quick
single-file inspection; for training use `extract_corpus.py` instead (it adds
archetype labels and richer context).

```bash
# All speakers
python3 scripts/chat/chatanalyze.py \
    --input logs/eqlog_Talodar_test.txt \
    --mode corpus

# One speaker only
python3 scripts/chat/chatanalyze.py \
    --input logs/eqlog_Talodar_test.txt \
    --mode corpus \
    --speaker Icestorm

# just
just chat-chatanalyze --input logs/eqlog_Talodar_test.txt --mode corpus --speaker Talodar
```

```json
{"speaker": "Talodar", "channel": "group", "text": "had to reset my log file..."}
{"speaker": "Icestorm", "channel": "group", "text": "yeah for sure"}
```

### chatanalyze.py options

```
--input / -i     EQ log file (required)
--mode           correlated | corpus | stats  (default: correlated)
--player-name    Log owner name; derived from eqlog_<Name>_* filename if omitted
--window         Lookback window in seconds for event correlation (default: 30)
--speaker        Filter output to a single speaker
--output / -o    Write to file instead of stdout
```

---

## Phase 2 — Label speakers (extract_corpus.py + analyze_speakers.py)

Speaker→archetype labels live in `data/chat/build/archetype_map.json`.  Both
`extract_corpus.py` and `analyze_speakers.py` read from and write to that file —
no source editing required.

Run this phase whenever you add new log files or when `just chat-stats` shows a
significant number of `Unknown` records.

### Step 1 — Extract the corpus (including unknowns for inspection)

```bash
# Raw — extract everything so you can see who is unlabelled
python3 scripts/chat/extract_corpus.py \
    --logs logs/ \
    --archetype-map data/chat/build/archetype_map.json \
    --output data/chat/build/corpus.jsonl \
    --include-unknown

# just (passes --archetype-map automatically)
just chat-corpus
```

`extract_corpus.py` walks every `eqlog_*.txt` in `logs/`, parses group, guild,
say, shout, ooc, and voice-tell lines, attaches a rolling window of recent game
events as `context`, and emits one JSONL record per chat line:

```json
{
  "timestamp": "2026-05-04T18:06:44",
  "speaker": "Talodar",
  "archetype": "ChaoticNarrator",
  "channel": "group",
  "text": "scattered and splattered as my grandma would say",
  "context": [{"kind": "item_loot", "summary": "You looted a Velium Gem Dusted Rune"}],
  "source_file": "eqlog_Talodar_test.txt"
}
```

### Step 2 — Check corpus statistics

```bash
# Archetype breakdown, channel counts, top speakers
python3 scripts/chat/extract_corpus.py \
    --logs logs/ \
    --archetype-map data/chat/build/archetype_map.json \
    --stats

# just
just chat-stats
```

Output shows how many records each archetype contributed and flags how much
of the corpus is `Unknown`.

### Step 3 — Score unknown speakers (analyze_speakers.py)

`analyze_speakers.py` reads `corpus.jsonl`, measures each speaker's verbosity,
humor, and patience from their messages, and ranks all archetypes by Euclidean
distance.  Run it after step 1 whenever there are `Unknown` speakers worth labelling.

By default it **only shows speakers not yet in the archetype map** — already-labelled
speakers (like Icestorm or Talodar) are intentionally hidden.  Use `--all-speakers`
to include them, which is useful for sanity-checking that existing labels still fit.

```bash
# Score all unknown speakers with ≥ 15 messages (default threshold)
python3 scripts/chat/analyze_speakers.py \
    --corpus data/chat/build/corpus.jsonl \
    --archetype-map data/chat/build/archetype_map.json

# just
just chat-analyze

# Lower threshold to catch occasional speakers
just chat-analyze --min-msgs 5

# Include already-labelled speakers to sanity-check the map
just chat-analyze --min-msgs 10 --all-speakers

# Preview suggested entries without writing anything
just chat-analyze --suggest-map
```

Example output:

```
Speaker               N     vb   hm   pt  Suggestions (dist)
────────────────────────────────────────────────────────────────────────────────
Sick                 40   0.41 0.12 0.82  RaidLeader(0.18)  TacticalFocused(0.21)  QuietAnchor(0.31)  ReactiveObserver(0.38)  ChaoticNarrator(0.52)
Matthias             12   0.29 0.09 0.88  QuietAnchor(0.21) ReactiveObserver(0.23) RaidLeader(0.28)   TacticalFocused(0.33)   ChaoticNarrator(0.71)
```

All 5 archetypes are shown by default, ranked nearest to furthest.  Pass `--top 3`
to trim the output if you only care about the closest matches.

### Step 4 — Write new labels into the map file

Once you're happy with the suggestions, write them directly into `archetype_map.json`
with `--update-map`.  No file editing required.

```bash
# Raw — writes top-ranked archetype for each Unknown speaker into the map file
python3 scripts/chat/analyze_speakers.py \
    --corpus data/chat/build/corpus.jsonl \
    --archetype-map data/chat/build/archetype_map.json \
    --update-map

# just
just chat-update-map
```

Output confirms what was written:

```
Wrote 2 new entry/entries to data/chat/build/archetype_map.json:
  Matthias → QuietAnchor
  Sick → RaidLeader
```

If you want to review suggestions before committing, run with `--suggest-map` first
(stdout only), then re-run with `--update-map` when satisfied.

### Step 5 — Re-extract with updated labels

```bash
# Raw
python3 scripts/chat/extract_corpus.py \
    --logs logs/ \
    --archetype-map data/chat/build/archetype_map.json \
    --output data/chat/build/corpus.jsonl

# just
just chat-corpus
```

---

## Phase 3 — Train the model

Run these steps in order after the corpus is labelled and clean.

```bash
# 1. Train a SentencePiece BPE vocabulary on the corpus
python3 scripts/chat/build_vocab.py \
    --corpus data/chat/build/corpus.jsonl \
    --output data/chat/build/vocab
# just
just chat-vocab

# 2. Tokenise the corpus into padded training arrays
python3 scripts/chat/build_dataset.py \
    --corpus data/chat/build/corpus.jsonl \
    --vocab  data/chat/build/vocab.model \
    --output data/chat/build/dataset
# just
just chat-dataset

# 3. Train the model (add --device cuda if available)
python3 scripts/chat/train.py \
    --dataset data/chat/build/dataset \
    --output  data/chat/build/checkpoints
# just
just chat-train
just chat-train --device cuda       # GPU

# 4. Export the best checkpoint to ONNX for Rust inference
python3 scripts/chat/export_onnx.py \
    --checkpoint data/chat/build/checkpoints/best.pt \
    --output     data/chat/models/model.onnx
# just
just chat-export
```

### Run the full pipeline in one shot

```bash
just chat-all
```

This runs `chat-corpus → chat-vocab → chat-dataset → chat-train → chat-export` in order.
It does not run the labeling phase — make sure `ARCHETYPE_MAP` is up to date first.

---

## Quick reference — all just recipes

| Recipe | What it runs |
|---|---|
| `just chat-chatanalyze [args]` | `chatanalyze.py` — explore a single log file |
| `just chat-corpus` | `extract_corpus.py` — extract labelled corpus from all logs |
| `just chat-stats` | `extract_corpus.py --stats` — print archetype/channel breakdown |
| `just chat-analyze [args]` | `analyze_speakers.py` — score unknown speakers |
| `just chat-update-map` | `analyze_speakers.py --update-map` — write new labels into `archetype_map.json` |
| `just chat-vocab` | `build_vocab.py` — train BPE vocabulary |
| `just chat-dataset` | `build_dataset.py` — tokenise corpus into training arrays |
| `just chat-train [args]` | `train.py` — train the model |
| `just chat-export` | `export_onnx.py` — export best checkpoint to ONNX |
| `just chat-all` | Run the full training pipeline (phases 2–3, minus labeling) |

---

## Run order

```bash
# First time only — bootstrap archetype_map.json from scratch
just chat-corpus --include-unknown   # extract all speakers; map file will be created empty
just chat-update-map                 # writes data/chat/build/archetype_map.json

# Train
just chat-vocab
just chat-dataset
just chat-train
just chat-export
```
