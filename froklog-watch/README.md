# froklog-watch — the Linux client

A native tray service for Linux that watches an EverQuest install, parses
every enabled character's log with the same engine as the Windows client,
and streams each one to a froklog server — plus the on-screen pieces:

- **DPS meter overlay** (Wayland layer-shell, above fullscreen games; X11
  always-on-top fallback) with class-gradient share bars, pet→owner labels,
  a mob picker, and a local last-5-fights memory.
- **Trigger message overlay** — fly-in announcements with icons and a
  history list, driven by the same `triggers.toml` the Windows client reads.
- **Voice alerts** through speech-dispatcher or piper (neural TTS), with a
  speaking queue: duplicate texts within 2 s collapse, distinct alerts are
  paced half a second apart.
- **Local-only mode**: unregistered characters parse, meter and trigger
  without any server at all.

Builds against the repo's own library (`froklog = { path = ".." }`), so the
pair always compiles in lockstep:

    cd froklog-watch
    cargo build --release
    ./install.sh        # binary, desktop entry, hicolor icons

Deliberately not part of any cargo workspace: the root crate's CI and
Windows cross-checks are unaffected by this directory.
