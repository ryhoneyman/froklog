# froklog — EverQuest Log Parser

[![CI](https://github.com/ryhoneyman/froklog/actions/workflows/ci.yml/badge.svg)](https://github.com/ryhoneyman/froklog/actions/workflows/ci.yml)

Real-time EQ combat stats: Windows tray client + embedded web server (Axum).

## Architecture

```
EQ log file
     │
     ▼  (async, seeks to EOF on start)
┌─────────────┐
│  Tailer     │  tokio single-thread runtime
│  (tailer.rs)│  handles files up to 10 GB+
└──────┬──────┘
       │ crossbeam channel (String lines) — line_tx
       ▼
┌─────────────┐
│  Splitter   │  OS thread — fans output to parser_tx AND trigger_tx
└──────┬──────┘  (trigger_tx drops lines if full rather than blocking parser)
       │
       ├─► parser_tx ─►
       │   ┌─────────────┐
       │   │  Parser     │  OS thread — regex hot-loop, updates Arc<ArcSwap<CombatState>>
       │   │  (parser.rs)│  emits CombatEvents via tokio::sync::mpsc
       │   └──────┬──────┘
       │          │
       │          ▼
       │   ┌─────────────┐
       │   │  Pusher     │  tokio single-thread — batches CombatEvents, WebSocket push
       │   │  (pusher.rs)│  auto-reconnects on disconnect
       │   └──────┬──────┘
       │          │ WebSocket (wss://<server>/ingest/:id)
       │          ▼
       │   ┌─────────────────────────────────────────────┐
       │   │ froklog-server  (Axum on :8766)             │
       │   │  POST /stream          — create stream       │
       │   │  GET  /ingest/:id      — ingest WebSocket   │
       │   │  GET  /stream/:id      — HTML viewer        │
       │   │  GET  /stream/:id/ws   — viewer WebSocket   │
       │   │  GET  /player/:g/:s/:n — public player page │
       │   │  GET  /admin           — admin panel        │
       │   └─────────────────────────────────────────────┘
       │
       └─► trigger_tx ─►
           ┌─────────────────────────┐
           │  Trigger Engine         │  OS thread — 100ms tick loop
           │  (triggers.rs::engine)  │  fires OverlayEvents to overlay queue
           └────────────┬────────────┘
                        │ Arc<Mutex<Vec<OverlayEvent>>>
                        ▼
           ┌─────────────────────────┐
           │  Overlay window         │  OS thread (Windows only)
           │  (overlay.rs)           │  Win32 WS_EX_LAYERED DIB popup
           └─────────────────────────┘
```

The trigger engine and overlay are only active in `--features tray` (Windows) builds.

### Binaries

| Binary | Description |
|--------|-------------|
| `froklog.exe` | Windows tray client — watches EQ log, pushes events to server |
| `froklog-server` | Axum server — ingest, stream registry, web viewer |
| `froklog-replay` | CLI tool — replays a log file into a running server |
| `froklog-debug` | Debug build — prints parsed events to stdout |
| `froklog-loggen` | Log generator — produces synthetic EQ log files for testing |
| `froklog-migrate` | One-shot migration — converts legacy JSONL journals to binary format |

## Quick start

```bash
# First time: install Rust + Windows cross-compile target
just setup
sudo apt-get install -y gcc-mingw-w64-x86-64

# Run the server (serves viewer UI on http://localhost:8766)
cargo run --bin froklog-server

# Build Windows .exe (includes system tray, overlay, trigger engine)
just build-windows
# → target/x86_64-pc-windows-gnu/release/froklog.exe

# Build all Linux binaries (server, replay, debug, migrate, loggen)
just build-all

# Lint + format check + tests (required before pushing)
just ci
```

### Testing without a live game

```bash
# Generate fake EQ log lines into /tmp/eq_test.log
just fake-log

# Use froklog-replay to push a log file into a running local server:
cargo run --bin froklog-replay -- \
    --log logs/eqlog_Icestorm_test.txt \
    --server http://localhost:8766 \
    --admin-token <token-from-froklog-server.toml> \
    --speed 10.0

# Dump an entire log file as fast as possible (ignores replay speed)
cargo run --bin froklog-replay -- \
    --log logs/eqlog_Icestorm_test.txt \
    --server http://localhost:8766 \
    --admin-token <token> \
    --dump

# Replay a specific time window
cargo run --bin froklog-replay -- \
    --log logs/eqlog_Icestorm_test.txt \
    --server http://localhost:8766 \
    --admin-token <token> \
    --from "2024-01-02 20:00:00" \
    --to "2024-01-02 21:00:00"
```

## Web UI

While `froklog-server` is running, open `http://<server-ip>:8766` in any browser
on the LAN to see live DPS/HPS tables. The page connects via WebSocket and updates
in real time as the client pushes events.

The index page (`/`) lists all active streams. Each stream has a viewer at
`/stream/<id>?vtok=<view_token>`. Public player pages are available without a
token at `/player/<game>/<server>/<name>`.

## Server configuration

On first run, `froklog-server` writes `froklog-server.toml` next to the binary.
The config path can be overridden with the `FROKLOG_CONFIG` environment variable.
All settings can also be overridden via `FROKLOG_*` environment variables.

| Setting | Default | Env var | Description |
|---------|---------|---------|-------------|
| `bind` | `0.0.0.0:8766` | `FROKLOG_BIND` | Listen address |
| `data_dir` | `streams` | `FROKLOG_DATA_DIR` | Stream journal directory |
| `admin_token` | (generated) | `FROKLOG_ADMIN_TOKEN` | Token for `GET /admin` |
| `stream_password` | (empty = open) | `FROKLOG_STREAM_PASSWORD` | Password to create streams |
| `rust_log` | `froklog_server=info` | `RUST_LOG` | Log filter |
| `rate_max` | `100` | `FROKLOG_RATE_MAX` | Max requests per IP per window |
| `rate_window_secs` | `10` | `FROKLOG_RATE_WINDOW_SECS` | Rate-limit window (seconds) |
| `ban_secs` | `300` | `FROKLOG_BAN_SECS` | DoS ban duration (seconds) |

### Admin panel

`GET /admin?atok=<admin_token>` — lists all streams, sessions, and journal stats.

## Session tracking

The server tracks play-session boundaries within each stream journal. Sessions are
cut by three mechanisms (in priority order):

1. **Login event** — "Welcome to EverQuest Legends!" received in an ingest batch.
2. **WS reconnect gap** — client reconnects after ≥ 10 minutes of inactivity.
3. **Retroactive scan** — gap of ≥ 30 minutes between consecutive combat log timestamps (applied to journals that predate session tracking).

Sessions are listed at `/stream/<id>/sessions` and can be replayed individually
by passing `?session=<num>` to the viewer.

## Windows client — setup

1. Drop `froklog.exe` on the Windows machine.
2. On first run, configure via the tray right-click menu:
   - **Settings…** — enter the server URL, then click Register to get stream credentials
   - **Set log file** — point to your `eqlog_Name_server.txt` (typically `C:\Users\<You>\Documents\EverQuest\Logs\`)
3. The tray icon appears; froklog runs silently in the background.

Config is saved to `%APPDATA%\froklog\config.toml`. Trigger rules are in `%APPDATA%\froklog\triggers.toml`.

The client binary does **not** accept command-line arguments for the log path — all configuration is done via the tray menu and the config file.

## Building the Windows client

The tray UI requires the `tray` feature flag (only meaningful when
cross-compiling for Windows):

```bash
cargo build --release --target x86_64-pc-windows-gnu --features tray
# or
just build-windows
```

Without `--features tray` the binary still compiles and connects to the server, but
has no system-tray icon, overlay window, or trigger engine.

## Trigger engine / overlay (`--features tray`)

On Windows, `froklog.exe` includes a data-driven trigger engine that processes log
lines in real-time and fires overlay messages or TTS speech. Triggers are defined in
`%APPDATA%\froklog\triggers.toml` and can be edited via the **Overlay Settings…** tray menu item.

Each trigger has conditions (`match` with exact/regex/glob, or `var` for stored variables)
combined with `all` (AND) or `any` (OR) logic, and actions (`overlay`, `voice_alert`, `store_var`).

Overlay messages appear in a stacked-deck Win32 popup (always on top, per-pixel alpha, ~60 fps
eased animation). Position and appearance are configured via **Overlay Settings…** in the tray menu.

## Log generator (`froklog-loggen`)

Generates a synthetic EQ log file for parser and UI testing without a live game:

```bash
cargo run --bin froklog-loggen -- \
    --player-name Talodar --players 5 --encounters 30 \
    --output logs/gen_Talodar_test.txt

# Deterministic run (same seed = same output)
cargo run --bin froklog-loggen -- --seed 42 --encounters 50
```

## Journal migration (`froklog-migrate`)

One-shot tool to convert legacy JSONL journal files to the current binary format.
Run with the server stopped:

```bash
cargo run --bin froklog-migrate -- /path/to/streams/
```

## Patterns parsed

| Pattern | Example |
|---------|---------|
| Melee hit | `Soandso hit Fippy Darkpaw for 1234 points of cold damage.` |
| Spell hit | `Wizard hit Fippy Darkpaw for 800 points of fire damage by Fireball.` |
| Attributed spell | `Wizard's Fireball hit Fippy Darkpaw for 800 points.` |
| Spell hit (lookup) | `Fireball hit Fippy Darkpaw for 800 points.` |
| DoT tick | `Fippy Darkpaw has been damaged by Wizard's Pyrocruor for 240.` |
| Has-taken | `Fippy Darkpaw has taken 500 damage from Wizard's Pyrocruor.` |
| Extra/bane damage | `Fippy Darkpaw has taken an extra 50 points of non-melee damage from Wizard's Bane spell.` |
| Riposte | `Fippy Darkpaw was injured by Warrior's riposte for 120.` |
| Damage shield (hit) | `Fippy Darkpaw was struck by Warrior's damage shield for 60.` |
| Damage shield (proc) | `Fippy Darkpaw is burned by YOUR flames for 20 points of non-melee damage.` |
| Rune absorption | `Warrior has shielded itself from 200 points of damage.` |
| Skin absorption | `Warrior's magical skin absorbs the damage of Fippy Darkpaw's thorns.` |
| Heal | `Cleric healed Soandso for 800 (1200) hit points by Complete Heal.` |
| Casting | `Wizard begins casting Fireball.` |
| Death (slain) | `Soandso has slain Fippy Darkpaw!` |
| Death (you) | `You have slain Fippy Darkpaw!` |
| Death (passive) | `Fippy Darkpaw was slain by Soandso!` |
| Death (no killer) | `Fippy Darkpaw died.` |
| Miss/dodge/parry | `Soandso tries to hit Fippy Darkpaw, but Fippy Darkpaw dodges!` |
| Resist | `Fippy Darkpaw resisted Wizard's Fireball!` |
| Loot (kept) | `--You have looted a Crystallized Sulfur from an abhorrent's corpse.--` |
| Loot (sold) | `You looted a Bat Meat from a sonic bat's corpse and sold it for 8 silver.` |
| Loot (hoard) | `You looted a Darkbrood Mask from a fire giant's corpse and stored it in your Dragon Hoard` |
| Loot (enhance) | `You looted a Darkbrood Mask +1 from Innoruuk's corpse to create a Darkbrood Mask +1` |
| Currency | `You receive 6 platinum, 1 gold, 8 silver and 3 copper from the corpse.` |
| /who | `[65 Warrior] Crunchy (Human)` — up to 3 classes per player |

Add or adjust patterns in [src/patterns.rs](src/patterns.rs) — each is a compiled `once_cell::sync::Lazy<Regex>`.
