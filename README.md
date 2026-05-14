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
       │ crossbeam channel (String lines)
       ▼
┌─────────────┐
│  Parser     │  dedicated OS thread
│  (parser.rs)│  regex hot-loop, no allocs beyond HashMap updates
└──────┬──────┘
       │ Arc<ArcSwap<CombatState>>  +  tokio::sync::mpsc events
       ▼
┌─────────────┐
│  Pusher     │  tokio single-thread runtime
│  (pusher.rs)│  batches CombatEvents, sends to server via WebSocket
└──────┬──────┘
       │ WebSocket (wss://<server>)
       ▼
┌─────────────────┐
│ froklog-server  │  tokio multi-thread
│ (Axum on :8765) │  POST /ingest — receive event batches
│                 │  GET  /stream/:id/ws — viewer WebSocket push
│                 │  GET  /stream/:id — HTML viewer
└─────────────────┘
```

### Binaries

| Binary | Description |
|--------|-------------|
| `froklog.exe` | Windows tray client — watches EQ log, pushes events to server |
| `froklog-server` | Axum server — ingest, stream registry, web viewer |
| `froklog-replay` | CLI tool — replays a log file into a running server |
| `froklog-debug` | Debug build — prints parsed events to stdout |

## Quick start

```bash
# First time: install Rust + Windows cross-compile target
just setup
sudo apt-get install -y gcc-mingw-w64-x86-64

# Run the server (serves viewer UI on http://localhost:8765)
cargo run --bin froklog-server

# Development: tail a log and push events to a local server
just run /path/to/eqlog_Player_server.txt

# Generate fake log lines and smoke-test without EQ running
just fake-log
just run-test

# Headless mode (no tray, no server — raw parse + local web UI)
just run-headless

# Build Windows .exe
just build-windows
# → target/x86_64-pc-windows-gnu/release/froklog.exe
```

## Web UI

While `froklog-server` is running, open `http://<server-ip>:8765` in any browser
on the LAN to see live DPS/HPS tables. The page connects via WebSocket and updates
in real time as the client pushes events.

## Windows client — setup

1. Drop `froklog.exe` on the Windows machine.
2. On first run, configure via the tray right-click menu:
   - **Set log file** — point to your `eqlog_Name_server.txt`
   - **Register with server** — enter the server URL and stream ID
3. The tray icon appears; froklog runs silently in the background.

## Passing the log path (headless / dev mode)

```
froklog.exe C:\Users\You\Documents\EverQuest\Logs\eqlog_Myrtle_server.txt
```

If omitted, it looks for `eqlog_Player_server.txt` in the current directory.

## Patterns parsed

| Pattern | Example |
|---------|---------|
| Melee hit | `Soandso hit Fippy Darkpaw for 1234 points of cold damage.` |
| Spell hit | `Wizard hit Fippy Darkpaw for 800 points of fire damage by Fireball.` |
| Attributed spell | `Wizard's Fireball hit Fippy Darkpaw for 800 points.` |
| DoT tick | `Fippy Darkpaw has been damaged by Wizard's Pyrocruor for 240.` |
| Riposte | `Fippy Darkpaw was injured by Warrior's riposte for 120.` |
| Damage shield | `Fippy Darkpaw was struck by Warrior's damage shield for 60.` |
| Heal | `Cleric healed Soandso for 800 (1200) hit points by Complete Heal.` |
| Death (slain) | `Soandso has slain Fippy Darkpaw!` |
| Death (passive) | `Fippy Darkpaw was slain by Soandso!` |
| Miss/dodge/parry | `Soandso tries to hit Fippy Darkpaw, but Fippy Darkpaw dodges!` |
| Resist | `Fippy Darkpaw resisted Wizard's Fireball!` |
| /who | `[65 Warrior] Crunchy (Human)` — up to 3 classes per player |

Add or adjust patterns in [src/patterns.rs](src/patterns.rs) — each is a compiled `once_cell::sync::Lazy<Regex>`.
