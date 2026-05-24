# froklog justfile — cross-compilation from Linux to Windows
# Install: cargo install just
# Run: just <recipe>

target_win  := "x86_64-pc-windows-gnu"
bin_name    := "froklog"

# ── Setup ──────────────────────────────────────────────────────────────────────

# Install Rust (skip if already present)
install-rust:
    curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y
    . "$HOME/.cargo/env"

# Add the Windows cross-compilation target
setup-windows-target:
    rustup target add {{target_win}}
    # Debian/Ubuntu: apt-get install -y gcc-mingw-w64-x86-64
    @echo "If linker errors occur: sudo apt-get install gcc-mingw-w64-x86-64"

# One-shot full setup (new machine / CI)
setup: install-rust setup-windows-target
    @echo "Setup complete."

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

# Windows release binary (cross-compiled from Linux)
build-windows:
    cargo build --release --target {{target_win}}
    @echo "Binary: target/{{target_win}}/release/{{bin_name}}.exe"

# ── Run ───────────────────────────────────────────────────────────────────────

# Run natively (Linux) for testing with a sample log
run *args:
    cargo run -- {{args}}

# Run with debug logging
run-debug *args:
    RUST_LOG=froklog=debug cargo run -- {{args}}

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
