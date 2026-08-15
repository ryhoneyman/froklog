# froklog-watch — install guide (Fedora)

*Interim testing client for Linux. A unified Slint-based Windows/Linux
client is in the works; this build is for trying the pipeline today.*

## 1. Install the package

    sudo dnf install ./froklog-watch-0.1.0-*.fcXX.x86_64.rpm

Match the `fcXX` to your Fedora release (builds exist for fc42 and fc44).
`dnf` pulls the Vulkan loader automatically.

## 2. Prerequisites

**Graphics (overlays).** The DPS meter renders through Vulkan.

- AMD / Intel GPUs: nothing to do — `mesa-vulkan-drivers` ships by default.
- NVIDIA (proprietary driver): your driver already includes Vulkan; the
  negotiated GPU is chosen automatically (integrated GPU preferred, so the
  game keeps the discrete card).

**Audio (trigger sounds + voice).**

    sudo dnf install pulseaudio-utils speech-dispatcher

`pulseaudio-utils` provides `paplay` (works fine under PipeWire);
`speech-dispatcher` is the zero-setup voice. It sounds robotic — see
step 5 for the good voice.

**Desktop environment notes.**

| Desktop | Meter/message overlays | Tray icon |
|---|---|---|
| KDE, COSMIC, Sway, Hyprland | Native (above fullscreen, click-through) | Works |
| GNOME | Meter falls back to an always-on-top window; message overlay unavailable | Needs the **AppIndicator and KStatusNotifierItem** extension |

The app lives in the tray: closing the window hides it, the tray icon
brings it back. Green = streaming, orange = attention, gray = idle.

## 3. First run

1. Launch **froklog-watch** from the app grid (or run `froklog-watch`).
2. **Server tab** — set the froklog server URL you were given
   (e.g. `https://froklog.example.net`). Leave the password blank unless
   the server owner says otherwise.
3. **Characters tab** — add your EverQuest `Logs` directory (for
   Lutris/Wine installs it's inside the prefix, e.g.
   `.../drive_c/EverQuest/Logs` or wherever `eqlog_<Name>_<server>.txt`
   files live). The app rescans automatically; your characters appear.
4. Tick a character to watch it:
   - **Registered** (click *Register* first): streams to the server —
     you get a private web link, an optional public page, and the
     shareable front-door page listing all your characters.
   - **Unregistered**: runs *local-only* — meter and triggers work with
     no server at all; a blue "local" tag shows.
   - IMPORTANT: make sure your in-game logging is ON: `/log on`.
5. **Meter tab** — enable the meter, then pick the **monitor** your game
   runs on (a compositor overlay is bound to one monitor). Drag it into
   place by its title bar; **lock** it when done — locked means clicks
   pass straight through to the game.

## 4. Triggers (optional, recommended)

**Triggers tab** — build audio/visual alerts from your own log lines:
paste a line (or use *Scan for message types*), click the words that
vary, add a sound, spoken text, or on-screen message. Same
`triggers.toml` format as the Windows client — files are portable
between them.

## 5. The good voice (optional): piper

Fedora doesn't package piper. Two files and it works:

1. Download `piper` (linux x86_64) from
   https://github.com/rhasspy/piper/releases — put the binary at
   `~/.local/bin/piper` (`chmod +x` it).
2. Download a voice (e.g. `en_US-lessac-medium.onnx` **and** its
   `.onnx.json`) from https://huggingface.co/rhasspy/piper-voices
   (samples: https://rhasspy.github.io/piper-samples) — put both files
   in `~/.local/share/piper/voices/`.
3. **Speech tab** — pick engine *piper* and your voice. Pronunciation
   fixes for game names live on the same tab.

## Troubleshooting

- **No meter appears**: it only shows during combat (or while the Meter
  settings tab is open). Check the monitor picker matches the game's
  monitor.
- **No tray icon on GNOME**: install/enable the AppIndicator extension,
  then relaunch.
- **Tray orange with a red banner about rejected batches**: the server
  is older than your client — the server needs updating, not you.
- **Voice silent**: check the Sounds tab's volume/mute, then the Speech
  tab's *Say a phrase* test button.
