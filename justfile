# froklog justfile — native Linux + Windows cross-compilation from Linux
# Install: cargo install just
# Run: just <recipe>

target_win       := "x86_64-pc-windows-gnu"
target_win_msvc  := "x86_64-pc-windows-msvc"
bin_name         := "froklog"

# Shared by every `cargo xwin build` recipe below (build-windows-tray,
# dev-windows-tray). cargo-xwin resolves "clang-cl" once against whatever's
# first on PATH and caches that resolution as a symlink at
# ~/.cache/cargo-xwin/clang-cl forever after — real MSVC STL headers reject
# anything older than Clang 19 (see setup-clang19's doc comment), so if this
# ever runs with a system Clang <19 already on PATH (Ubuntu 22.04's default
# is 14) ahead of ~/tools/llvm19-bin (setup-clang19's no-sudo install), the
# cache gets poisoned with the wrong compiler and every future xwin build
# silently keeps using it — even after PATH is fixed, since cargo-xwin only
# creates the symlink if it's missing. This bit us for real: a `just
# iterate` run appeared to succeed with only cosmetic-looking "compiler
# family detection failed" warnings, because it reused already-built .obj
# files from a prior correct build instead of recompiling anything with the
# now-wrong cached compiler — a genuinely clean rebuild failed outright with
# `error STL1000: Unexpected compiler version, expected Clang 19.0.0 or
# newer.` Removing the cache file here each time (harmless — cargo-xwin just
# recreates it) makes every xwin build self-correct against the PATH set
# right above it, instead of trusting whatever ran here last.
xwin_path_setup := '''
    if [ -d "$HOME/tools/llvm19-bin" ]; then
        export PATH="$HOME/tools/llvm19-bin:$PATH"
        # setup-clang19's extracted .deb tree — clang-cl itself dynamically
        # links libLLVM.so.19.1 from here, not just needing to be found on
        # PATH. Without this it fails at *load* time ("error while loading
        # shared libraries"), a different failure from the PATH issue above
        # but the same "only ever documented as a manual export" trap.
        export LD_LIBRARY_PATH="$HOME/tools/llvm19/usr/lib/x86_64-linux-gnu:${LD_LIBRARY_PATH:-}"
    fi
    rm -f "$HOME/.cache/cargo-xwin/clang-cl"
'''

# ── Setup ──────────────────────────────────────────────────────────────────────

# Install Rust (skip if already present)
install-rust:
    curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y
    . "$HOME/.cargo/env"

# Add the Windows cross-compilation target used by the headless binaries
# (froklog-server/-replay/-debug/-migrate/-loggen) — NOT the tray client,
# which cross-compiles to windows-msvc instead (see setup-windows-msvc-target).
# Those binaries never link onnxruntime/piper, so mingw is fine for them.
setup-windows-target:
    rustup target add {{target_win}}
    # Debian/Ubuntu: apt-get install -y gcc-mingw-w64-x86-64
    @echo "If linker errors occur: sudo apt-get install gcc-mingw-w64-x86-64"

# Windows cross-compilation for the tray client specifically, via
# cargo-xwin + clang-cl instead of mingw — required because it links
# onnxruntime (via piper-rs, see src/tts.rs), whose Windows binaries target
# the MSVC ABI, not mingw's. Needs Clang >=19 already on PATH (real MSVC
# STL headers reject anything older) — see setup-clang19 if the system
# Clang is too old, and llvm-lib (from that same Clang install, or any
# system LLVM's "-tools" package) on PATH too; rustup's own llvm-tools
# component doesn't ship it.
setup-windows-msvc-target:
    rustup target add {{target_win_msvc}}
    rustup component add llvm-tools
    cargo install cargo-xwin
    @echo "Also need Clang >=19 + llvm-lib on PATH — see setup-clang19 if the system Clang is older."

# Installs Clang 19 without needing sudo/root, by extracting apt.llvm.org's
# .deb packages directly (dpkg-deb -x) into ~/tools/llvm19 — for a machine
# without sudo access. On a machine *with* sudo (most CI runners), the far
# simpler path is apt.llvm.org's own install script instead:
#   wget https://apt.llvm.org/llvm.sh && chmod +x llvm.sh && sudo ./llvm.sh 19
# After running this, put ~/tools/llvm19-bin (symlinked clang/clang-cl) and
# your system LLVM's -tools package (for llvm-lib) on PATH before building.
setup-clang19:
    #!/usr/bin/env bash
    set -e
    VER="19.1.7~++20250114103320+cd708029e0b2-1~exp1~20250114103432.75"
    DEST=$(mktemp -d)
    PREFIX="$HOME/tools/llvm19"
    BASE="https://apt.llvm.org/jammy/pool/main/l/llvm-toolchain-19"
    mkdir -p "$PREFIX" "$HOME/tools/llvm19-bin"
    for pkg in clang-19 libclang-cpp19 libllvm19 libclang-common-19-dev libclang1-19 llvm-19-linker-tools; do
        f="${pkg}_${VER}_amd64.deb"
        echo "downloading $f"
        curl -sSL -o "$DEST/$f" "$BASE/$f"
        dpkg-deb -x "$DEST/$f" "$PREFIX"
    done
    ln -sf "$PREFIX/usr/lib/llvm-19/bin/clang" "$HOME/tools/llvm19-bin/clang"
    ln -sf "$PREFIX/usr/lib/llvm-19/bin/clang" "$HOME/tools/llvm19-bin/clang-cl"
    ln -sf "$PREFIX/usr/lib/llvm-19/bin/clang++" "$HOME/tools/llvm19-bin/clang++"
    rm -rf "$DEST"
    echo "Installed. Add to PATH before building:"
    echo "  export PATH=\"$HOME/tools/llvm19-bin:\$PATH\""
    echo "  export LD_LIBRARY_PATH=\"$PREFIX/usr/lib/x86_64-linux-gnu:\$LD_LIBRARY_PATH\""

# System dev libraries the native Linux tray build needs (GTK/appindicator
# for the tray icon, X11/Wayland for winit, ALSA for sound, cmake for
# building espeak-ng from source — piper-rs's phonemization backend, see
# src/tts.rs). Debian/Ubuntu only — adjust package names for other distros.
# Keep this package list in sync with .github/workflows/ci.yml's
# "Install tray system dependencies" step.
setup-linux-tray-deps:
    sudo apt-get install -y \
        libgtk-3-dev libayatana-appindicator3-dev libxdo-dev \
        libxkbcommon-dev libxkbcommon-x11-0 libx11-dev libxcursor-dev libxi-dev libxrandr-dev libwayland-dev \
        libasound2-dev \
        cmake

# Downloads the onnxruntime shared libraries the tray client bundles (see
# src/assets.rs's runtime_dir() and src/main.rs's ORT_DYLIB_PATH wiring)
# into gitignored assets/runtime/ — Microsoft's official release, not
# ort's own downloaded prebuilt binary, because this one only needs glibc
# >=2.28 on Linux (ort's needs >=2.38, too new for some real deployment
# targets — see memory/project_piper_tts_spike.md for how that was found).
onnxruntime_version := "1.29.0"
fetch-onnxruntime:
    #!/usr/bin/env bash
    set -e
    mkdir -p assets/runtime/linux assets/runtime/windows
    curl -sSL "https://github.com/microsoft/onnxruntime/releases/download/v{{onnxruntime_version}}/onnxruntime-linux-x64-{{onnxruntime_version}}.tgz" \
        | tar -xz -C assets/runtime/linux --strip-components=1
    curl -sSL -o /tmp/onnxruntime-win.zip \
        "https://github.com/microsoft/onnxruntime/releases/download/v{{onnxruntime_version}}/onnxruntime-win-x64-{{onnxruntime_version}}.zip"
    unzip -oq /tmp/onnxruntime-win.zip -d assets/runtime/windows
    rm /tmp/onnxruntime-win.zip
    echo "onnxruntime {{onnxruntime_version}} fetched into assets/runtime/{linux,windows}/"

# Downloads the one voice bundled with every install (see
# config.rs's default_tts_voice()) into gitignored assets/voices/ — the
# small extra catalog in Settings' voice manager downloads on demand
# instead, straight to the user's data dir, not through this recipe.
fetch-default-voice:
    #!/usr/bin/env bash
    set -e
    mkdir -p assets/voices
    curl -sSL -o assets/voices/en_US-amy-low.onnx \
        "https://huggingface.co/rhasspy/piper-voices/resolve/main/en/en_US/amy/low/en_US-amy-low.onnx"
    curl -sSL -o assets/voices/en_US-amy-low.onnx.json \
        "https://huggingface.co/rhasspy/piper-voices/resolve/main/en/en_US/amy/low/en_US-amy-low.onnx.json"
    echo "Bundled voice fetched into assets/voices/"

# Fetches espeak-rs-sys 0.2.0 from crates.io into gitignored vendor/ and
# applies froklog's one-line-per-file Windows cross-compile fix on top (see
# vendor-patches/ and Cargo.toml's [patch.crates-io]) — cargo-xwin's clang-cl
# doesn't transitively pull in <io.h>/advapi32 the way real MSVC's cl.exe
# does, so espeak-ng's vendored CLI tool (built as a data-compiler step even
# though froklog never runs it) fails to compile/link without this.
espeak_rs_sys_version := "0.2.0"
fetch-espeak-rs-sys:
    #!/usr/bin/env bash
    set -e
    rm -rf vendor/espeak-rs-sys
    mkdir -p vendor
    curl -sSL "https://crates.io/api/v1/crates/espeak-rs-sys/{{espeak_rs_sys_version}}/download" \
        | tar -xz -C vendor
    mv "vendor/espeak-rs-sys-{{espeak_rs_sys_version}}" vendor/espeak-rs-sys
    patch -p1 -d vendor/espeak-rs-sys < vendor-patches/espeak-rs-sys-windows-cross-compile.patch
    echo "espeak-rs-sys {{espeak_rs_sys_version}} fetched and patched into vendor/espeak-rs-sys/"

# One-shot full setup (new machine / CI)
setup: install-rust setup-windows-target
    @echo "Setup complete. Run 'just setup-linux-tray-deps' too if building the native Linux tray client."
    @echo "For the tray client specifically, also run 'just setup-windows-msvc-target', 'just fetch-onnxruntime', 'just fetch-default-voice', and 'just fetch-espeak-rs-sys'."

# ── Build ──────────────────────────────────────────────────────────────────────

# Native Linux debug build (fast iteration); sweeps incremental cache if target/debug > 1 GB
dev:
    #!/usr/bin/env bash
    set -e
    cargo build
    size=$(du -sb target/debug | cut -f1)
    if [ "$size" -gt 1073741824 ]; then
        echo "target/debug is $((size/1024/1024)) MB — sweeping stale artifacts..."
        cargo sweep -t 7
    fi

# Native Linux release build
build:
    cargo build --release

# Native Linux tray client debug build (fast iteration) — requires
# 'just setup-linux-tray-deps' once per machine
dev-tray:
    cargo build --features tray --bin {{bin_name}}

# Native Linux tray client release build
build-tray:
    cargo build --release --features tray --bin {{bin_name}}
    @echo "Binary: target/release/{{bin_name}}"

# Windows release build of the headless binaries (froklog-server/-replay/
# -debug/-migrate/-loggen), cross-compiled from Linux via mingw. NOT the
# tray client — see build-windows-tray for that (different toolchain,
# see setup-windows-msvc-target's doc comment for why).
build-windows:
    cargo build --release --target {{target_win}} \
        --bin froklog-server --bin froklog-replay --bin froklog-debug --bin froklog-migrate --bin froklog-loggen
    @echo "Binaries: target/{{target_win}}/release/"

# Same as build-windows but dev-profile (skips release's thin-LTO/
# codegen-units=1/strip, much faster) — for handing a build to a remote
# machine to test, not for shipping.
dev-windows:
    cargo build --target {{target_win}} \
        --bin froklog-server --bin froklog-replay --bin froklog-debug --bin froklog-migrate --bin froklog-loggen
    @echo "Binaries: target/{{target_win}}/debug/"

# Windows release build of the tray client, cross-compiled from Linux via
# cargo-xwin/msvc (see setup-windows-msvc-target). Needs
# assets/runtime/windows/onnxruntime.dll and assets/voices/ alongside the
# built .exe at runtime — see fetch-onnxruntime/fetch-default-voice.
build-windows-tray:
    #!/usr/bin/env bash
    set -euo pipefail
    {{xwin_path_setup}}
    cargo xwin build --release --features tray --bin {{bin_name}} --target {{target_win_msvc}}
    echo "Binary: target/{{target_win_msvc}}/release/{{bin_name}}.exe"

# Same as build-windows-tray but dev-profile — for handing a build to a
# remote machine to test, not for shipping.
dev-windows-tray:
    #!/usr/bin/env bash
    set -euo pipefail
    {{xwin_path_setup}}
    cargo xwin build --features tray --bin {{bin_name}} --target {{target_win_msvc}}
    echo "Binary: target/{{target_win_msvc}}/debug/{{bin_name}}.exe"

# ── Release ───────────────────────────────────────────────────────────────────

# Bumps froklog's own version (Cargo.toml + Cargo.lock, via `cargo check`),
# commits that as its own dedicated commit, then creates a matching
# annotated tag — the "bump-then-tag" convention release.yml's check-version
# job enforces (it fails the whole release pipeline if a pushed tag doesn't
# match Cargo.toml's version). Requires a clean working tree first: this
# should be the last, deliberate step marking "this is the new release", not
# a commit that also happens to carry unrelated pending work — commit that
# separately first.
#
# Does NOT push. Review with `git show HEAD` / `git show vX.Y.Z`, then push
# both in one action when ready to actually trigger release.yml:
#   git push origin main --follow-tags
# (or VSCode's "Git: Push (Follow Tags)" — works here since this creates an
# annotated tag, unlike some of this project's older lightweight tags).
bump-version version:
    #!/usr/bin/env bash
    set -euo pipefail
    VERSION="{{version}}"
    if ! [[ "$VERSION" =~ ^[0-9]+\.[0-9]+\.[0-9]+$ ]]; then
        echo "error: version must be full semver X.Y.Z (Cargo.toml requires it), got '$VERSION'" >&2
        exit 1
    fi
    if [ -n "$(git status --porcelain)" ]; then
        echo "error: working tree not clean — commit or stash pending changes first," >&2
        echo "so the version-bump commit doesn't carry unrelated work" >&2
        exit 1
    fi
    sed -i "s/^version = \".*\"/version = \"$VERSION\"/" Cargo.toml
    cargo check --quiet
    git add Cargo.toml Cargo.lock
    git commit -m "Bump version to $VERSION"
    git tag -a "v$VERSION" -m "v$VERSION"
    echo "Bumped to $VERSION, committed, and tagged v$VERSION locally (not pushed)."
    echo "Review, then: git push origin main --follow-tags"

# ── Packaging ─────────────────────────────────────────────────────────────────

# Where release archives get staged before archiving. Gitignored.
dist_dir := "dist"

# Stages a portable native-Linux tray release (binary + the three runtime
# assets it needs beside it: onnxruntime, the default voice, and
# espeak-ng-data — see assets.rs and memory/project_piper_tts_spike.md) into
# dist/froklog-linux-x86_64/, then tar.gz's it. Requires
# fetch-onnxruntime/fetch-default-voice to have populated assets/runtime and
# assets/voices already (not re-run here — they hit the network every time).
# The Windows release build is NOT packaged from here — see
# .github/workflows/release.yml, which builds it on a native windows-latest
# runner instead of this box's cargo-xwin cross-compile toolchain.
package-tray-linux: build-tray
    #!/usr/bin/env bash
    set -euo pipefail
    STAGE="{{dist_dir}}/froklog-linux-x86_64"
    rm -rf "$STAGE" && mkdir -p "$STAGE"
    cp target/release/{{bin_name}} "$STAGE/"
    # runtime_dir() (assets.rs) expects the .so directly beside the exe, not
    # nested under assets/runtime/linux's own lib/ subfolder — flatten it.
    # -a preserves the libonnxruntime.so -> .so.1 -> .so.1.29.0 symlink
    # chain instead of three full copies of a 28MB file.
    mkdir -p "$STAGE/runtime"
    cp -a assets/runtime/linux/lib/. "$STAGE/runtime/"
    cp -r assets/voices "$STAGE/voices"
    ESPEAK_DATA=$(find target -path '*/build/espeak-rs-sys-*/out/share/espeak-ng-data' -type d \
        -printf '%T@ %p\n' | sort -rn | head -1 | cut -d' ' -f2-)
    if [ -z "$ESPEAK_DATA" ]; then
        echo "error: no espeak-ng-data found under target/ — run 'just build-tray' first" >&2
        exit 1
    fi
    cp -r "$ESPEAK_DATA" "$STAGE/espeak-ng-data"
    printf '%s\n' \
        'froklog (Linux tray client)' \
        '============================' \
        '' \
        'Run: ./froklog' \
        '' \
        'This is a portable install: nothing gets written outside this folder' \
        'except your own data under ~/.local/share/froklog (icons, sound packages,' \
        'downloaded voices) and config under $XDG_CONFIG_HOME (or ~/.config).' \
        '' \
        'Runtime system packages required (install via apt/dnf/pacman/etc if' \
        'froklog fails to start -- Debian/Ubuntu names shown):' \
        '  libgtk-3-0 libayatana-appindicator3-1 libxdo3' \
        '  libxkbcommon0 libxkbcommon-x11-0 libx11-6 libxcursor1 libxi6 libxrandr2' \
        '  libwayland-client0' \
        '  libasound2' \
        > "$STAGE/README.txt"
    mkdir -p "{{dist_dir}}"
    tar -czf "{{dist_dir}}/froklog-linux-x86_64.tar.gz" -C "{{dist_dir}}" froklog-linux-x86_64
    echo "Packaged: {{dist_dir}}/froklog-linux-x86_64.tar.gz"

# ── Fast iteration loop ──────────────────────────────────────────────────────
# No fmt/clippy/test, no --release — just the two binaries you need while
# actively changing code: a Linux dev build to smoke-test locally (e.g. under
# Xvfb) and a Windows dev build to hand to a remote machine. Run `just ci`
# once you're done iterating and ready to actually commit/push — nothing here
# skips real GitHub Actions CI (.github/workflows/ci.yml only runs *after* a
# push and isn't blocking anything locally), this just skips the same local
# checks it runs so each round-trip is faster.
iterate: dev-tray dev-windows-tray
    @echo "Linux:   target/debug/{{bin_name}}"
    @echo "Windows: target/{{target_win_msvc}}/debug/{{bin_name}}.exe"

# ── Run ───────────────────────────────────────────────────────────────────────

# Run natively (Linux) for testing with a sample log
run *args:
    cargo run -- {{args}}

# Run with debug logging
run-debug *args:
    RUST_LOG=froklog=debug cargo run -- {{args}}

# Run the native Linux tray client for testing with a sample log
run-tray *args:
    cargo run --features tray --bin {{bin_name}} -- {{args}}

# ── Test / Lint ───────────────────────────────────────────────────────────────

test:
    cargo test

lint:
    cargo clippy -- -D warnings

fmt:
    cargo fmt --check

ci: fmt lint test

# ── Utilities ─────────────────────────────────────────────────────────────────

# Generate a fake EQ log line to test the parser without a live game
fake-log:
    @echo "[Tue Jan 01 00:00:01 2000] Soandso hit Fippy Darkpaw for 1234 points of cold damage." >> /tmp/eq_test.log
    @echo "[Tue Jan 01 00:00:02 2000] Healer healed Soandso for 500 (800) hit points." >> /tmp/eq_test.log
    @echo "Appended test lines to /tmp/eq_test.log"

# Tail /tmp/eq_test.log from EOF (handy for testing without EQ running)
run-test:
    cargo run -- /tmp/eq_test.log

# Headless web-only mode — no GUI required, open http://localhost:8765 in browser.
# Workflow: just run-headless  (terminal 1)
#           just fake-log      (terminal 2, repeat to feed lines)
run-headless:
    touch /tmp/eq_test.log
    cargo run --no-default-features -- /tmp/eq_test.log --headless

# Replay the whole log file from the beginning in headless mode.
run-from-start:
    cargo run --no-default-features -- /tmp/eq_test.log --headless --from-start

# Replay a specific time range (edit dates as needed).
# Example: just run-range "2024-01-02 20:00:00" "2024-01-02 21:00:00"
run-range from to:
    cargo run --no-default-features -- /tmp/eq_test.log --headless --from "{{from}}" --to "{{to}}"

# Replay 30 minutes starting from a given time.
# Example: just run-window "2024-01-02 20:00:00"
run-window from:
    cargo run --no-default-features -- /tmp/eq_test.log --headless --from "{{from}}" --duration 30m

clean:
    cargo clean

# Manually prune stale incremental artifacts older than 7 days
sweep:
    cargo sweep -t 7

# Wipe only debug artifacts, keep release
clean-debug:
    rm -rf target/debug

build-all:
    cargo build --release --no-default-features --features neural --bin froklog-loggen
    cargo build --release --no-default-features --bin froklog-server --bin froklog-replay --bin froklog-debug --bin froklog-migrate

# ── Neural training pipeline ───────────────────────────────────────────────────

chat_data    := "data/chat/build"
chat_scripts := "scripts/chat"
chat_map     := "data/chat/build/archetype_map.json"

# 1. Extract corpus JSONL from all EQ log files
# Usage: just chat-corpus [--include-unknown] [--stats]
chat-corpus *args:
    python3 {{chat_scripts}}/extract_corpus.py \
        --logs logs/ \
        --archetype-map {{chat_map}} \
        --output {{chat_data}}/corpus.jsonl \
        {{args}}

# 2. Train SentencePiece BPE vocabulary on the corpus
chat-vocab:
    python3 {{chat_scripts}}/build_vocab.py \
        --corpus {{chat_data}}/corpus.jsonl \
        --output {{chat_data}}/vocab

# 3. Tokenise corpus into padded training arrays
chat-dataset:
    python3 {{chat_scripts}}/build_dataset.py \
        --corpus {{chat_data}}/corpus.jsonl \
        --vocab  {{chat_data}}/vocab.model \
        --output {{chat_data}}/dataset

# 4. Train the model (CPU; use --device cuda if available)
chat-train *args:
    python3 {{chat_scripts}}/train.py \
        --dataset {{chat_data}}/dataset \
        --output  {{chat_data}}/checkpoints \
        {{args}}

# 5. Export best checkpoint to ONNX for Rust inference
chat-export:
    python3 {{chat_scripts}}/export_onnx.py \
        --checkpoint {{chat_data}}/checkpoints/best.pt \
        --output     data/chat/models/model.onnx

# Full pipeline: corpus → vocab → dataset → train → export
chat-all: chat-corpus chat-vocab chat-dataset chat-train chat-export

# Print per-archetype corpus statistics without writing files
chat-stats:
    python3 {{chat_scripts}}/extract_corpus.py \
        --logs logs/ \
        --archetype-map {{chat_map}} \
        --stats

# Infer archetype suggestions for unknown speakers from corpus features
chat-analyze *args:
    python3 {{chat_scripts}}/analyze_speakers.py \
        --corpus {{chat_data}}/corpus.jsonl \
        --archetype-map {{chat_map}} \
        {{args}}

# Write suggested archetype labels for unknown speakers into the map file
chat-update-map:
    python3 {{chat_scripts}}/analyze_speakers.py \
        --corpus {{chat_data}}/corpus.jsonl \
        --archetype-map {{chat_map}} \
        --update-map

# Correlate/annotate chat from a single EQ log file (replaces froklog-chatanalyze binary)
# Usage: just chat-chatanalyze --input eqlog_Name.txt [--mode correlated|corpus|stats] [--speaker Name]
chat-chatanalyze *args:
    python3 {{chat_scripts}}/chatanalyze.py {{args}}
