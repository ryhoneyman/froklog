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

build-all:
    cargo build --release --no-default-features --features neural --bin froklog-loggen
    cargo build --release --no-default-features --bin froklog-server --bin froklog-replay --bin froklog-debug --bin froklog-migrate

# ── Neural training pipeline ───────────────────────────────────────────────────

nn_data    := "data/chat/build"
nn_scripts := "scripts/chat"
nn_map     := "data/chat/build/archetype_map.json"

# 1. Extract corpus JSONL from all EQ log files
nn-corpus:
    python3 {{nn_scripts}}/extract_corpus.py \
        --logs logs/ \
        --archetype-map {{nn_map}} \
        --output {{nn_data}}/corpus.jsonl

# 2. Train SentencePiece BPE vocabulary on the corpus
nn-vocab:
    python3 {{nn_scripts}}/build_vocab.py \
        --corpus {{nn_data}}/corpus.jsonl \
        --output {{nn_data}}/vocab

# 3. Tokenise corpus into padded training arrays
nn-dataset:
    python3 {{nn_scripts}}/build_dataset.py \
        --corpus {{nn_data}}/corpus.jsonl \
        --vocab  {{nn_data}}/vocab.model \
        --output {{nn_data}}/dataset

# 4. Train the model (CPU; use --device cuda if available)
nn-train *args:
    python3 {{nn_scripts}}/train.py \
        --dataset {{nn_data}}/dataset \
        --output  {{nn_data}}/checkpoints \
        {{args}}

# 5. Export best checkpoint to ONNX for Rust inference
nn-export:
    python3 {{nn_scripts}}/export_onnx.py \
        --checkpoint {{nn_data}}/checkpoints/best.pt \
        --output     data/chat/models/model.onnx

# Full pipeline: corpus → vocab → dataset → train → export
nn-all: nn-corpus nn-vocab nn-dataset nn-train nn-export

# Print per-archetype corpus statistics without writing files
nn-stats:
    python3 {{nn_scripts}}/extract_corpus.py \
        --logs logs/ \
        --archetype-map {{nn_map}} \
        --stats

# Infer archetype suggestions for unknown speakers from corpus features
nn-analyze *args:
    python3 {{nn_scripts}}/analyze_speakers.py \
        --corpus {{nn_data}}/corpus.jsonl \
        --archetype-map {{nn_map}} \
        {{args}}

# Write suggested archetype labels for unknown speakers into the map file
nn-update-map:
    python3 {{nn_scripts}}/analyze_speakers.py \
        --corpus {{nn_data}}/corpus.jsonl \
        --archetype-map {{nn_map}} \
        --update-map

# Correlate/annotate chat from a single EQ log file (replaces froklog-chatanalyze binary)
# Usage: just nn-chatanalyze --input eqlog_Name.txt [--mode correlated|corpus|stats] [--speaker Name]
nn-chatanalyze *args:
    python3 {{nn_scripts}}/chatanalyze.py {{args}}
