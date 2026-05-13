# froklog — EverQuest Log Parser

Real-time EQ combat stats: native Windows GUI (egui) + embedded web server (Axum).

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
       │ Arc<ArcSwap<CombatState>> (lock-free atomic swap)
       ├─────────────────────────────────┐
       ▼                                 ▼
┌─────────────┐                 ┌─────────────────┐
│  egui GUI   │  main thread    │  Axum web server │  tokio multi-thread
│  (main.rs)  │  ~20 fps        │  (web.rs)        │  :8765
│             │                 │  GET /stats JSON │
│             │                 │  GET /  HTML UI  │
└─────────────┘                 └─────────────────┘
```

## Quick start

```bash
# First time: install Rust + Windows cross-compile target
just setup
sudo apt-get install -y gcc-mingw-w64-x86-64

# Development (native Linux, no GUI — good for testing parser/server)
just run /path/to/eqlog_Player_server.txt

# Generate fake log lines to smoke-test without EQ running
just fake-log
just run-test

# Build Windows .exe
just build-windows
# → target/x86_64-pc-windows-gnu/release/froklog.exe
```

## Web UI

While running, open `http://<machine-ip>:8765` in any browser on the LAN to see
live DPS/HPS tables. The page polls `/stats` every second.

## Passing the log path

```
froklog.exe C:\Users\You\Documents\EverQuest\Logs\eqlog_Myrtle_server.txt
```

If omitted, it looks for `eqlog_Player_server.txt` in the current directory.

## Patterns parsed

| Pattern | Example |
|---------|---------|
| Melee hit | `Soandso hit Fippy Darkpaw for 1234 points of cold damage.` |
| Non-melee | `Fippy Darkpaw was hit by non-melee for 500 points of damage.` |
| Heal | `Cleric healed Soandso for 800 (1200) hit points.` |
| Spell cast | `Wizard begins casting Fireball.` |

Add patterns in `src/parser.rs` — each is a compiled `once_cell::sync::Lazy<Regex>`.
