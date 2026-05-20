use std::sync::atomic::{AtomicBool, Ordering};
/// froklog-replay — CLI tool for streaming a saved EQ log file to a froklog-server.
///
/// Runs the same tailer → parser → pusher pipeline as the Windows client but
/// accepts full replay controls (start time, end time, speed multiplier) from
/// the command line.  Useful for testing without replaying a live log from a
/// production Windows machine.
///
/// Auto-register example (recommended for local testing):
///   froklog-replay \
///     --log logs/eqlog_Icestorm_test.txt \
///     --server http://localhost:8766 \
///     --admin-token mytoken \
///     --speed 10.0
///
/// Manual registration example:
///   froklog-replay \
///     --log logs/eqlog_Icestorm_test.txt \
///     --server http://localhost:8766 \
///     --stream-id <id> \
///     --stream-token <token> \
///     --from "2024-01-15 19:30:00" \
///     --to   "2024-01-15 20:00:00" \
///     --speed 10.0
use std::sync::Arc;
use std::thread;
use std::time::Duration;

use arc_swap::ArcSwap;
use chrono::NaiveDateTime;
use clap::Parser;
use crossbeam_channel::bounded;
use serde::Deserialize;
use tokio::sync::broadcast;
use tracing::info;

use froklog::state::CombatState;
use froklog::tailer::{TailConfig, TailFrom};
use froklog::{parser, pusher, tailer};

// ── CLI args ──────────────────────────────────────────────────────────────────

#[derive(Parser, Debug)]
#[command(
    name = "froklog-replay",
    about = "Stream a saved EverQuest log file to a froklog-server for testing"
)]
struct Args {
    /// Path to the EverQuest log file (e.g. logs/eqlog_Icestorm_test.txt)
    #[arg(long)]
    log: String,

    /// froklog-server base URL (e.g. http://localhost:8766)
    #[arg(long)]
    server: String,

    /// Admin token for auto-registering a stream (reads player name from --log).
    /// If provided, --stream-id and --stream-token are not required.
    #[arg(long)]
    admin_token: Option<String>,

    /// Game identifier sent to the server (e.g. "eql", "eq2").
    /// Defaults to "eql" when --admin-token is used.
    #[arg(long, default_value = "eql")]
    game: String,

    /// EverQuest server name (e.g. "Teek", "Tormax").
    /// When --admin-token is used this is auto-extracted from the log filename
    /// (eqlog_<Player>_<Server>.txt) if not provided here.
    #[arg(long, value_name = "EQSERVER")]
    eq_server: Option<String>,

    /// Stream ID (from `POST /stream` registration).
    /// Required when --admin-token is not provided.
    #[arg(long)]
    stream_id: Option<String>,

    /// Stream ingest token (from `POST /stream` registration).
    /// Required when --admin-token is not provided.
    #[arg(long)]
    stream_token: Option<String>,

    /// Start replaying from this timestamp (format: "YYYY-MM-DD HH:MM:SS").
    /// Omit to replay from the very beginning of the file.
    #[arg(long, value_name = "DATETIME")]
    from: Option<String>,

    /// Stop replaying at this timestamp (format: "YYYY-MM-DD HH:MM:SS").
    /// Omit to play until EOF.
    #[arg(long, value_name = "DATETIME")]
    to: Option<String>,

    /// Replay speed multiplier (1.0 = real-time, 10.0 = 10× faster).
    /// Defaults to 1.0. Ignored when --dump is set.
    #[arg(long, default_value = "1.0")]
    speed: f64,

    /// Dump mode: send the log file to the server as fast as possible and exit.
    /// Overrides --speed. Replays from the beginning of the file (or --from)
    /// to EOF (ignoring --to).
    #[arg(long, default_value = "false")]
    dump: bool,
}

// ── Registration response ─────────────────────────────────────────────────────

#[derive(Deserialize)]
struct RegisterResponse {
    stream_id: String,
    stream_token: String,
    view_token: String,
    view_path: String,
}

// ── Entry point ───────────────────────────────────────────────────────────────

fn main() {
    tracing_subscriber::fmt()
        .with_env_filter(
            std::env::var("RUST_LOG")
                .unwrap_or_else(|_| "froklog=info,froklog_replay=info".into())
                .as_str(),
        )
        .init();

    let args = Args::parse();

    let player_name = extract_player_name(&args.log);

    // Resolve stream_id + stream_token — either from args or via auto-registration.
    let (stream_id, stream_token) = match &args.admin_token {
        Some(admin_token) => {
            if player_name.is_empty() {
                eprintln!(
                    "error: could not extract player name from log filename \"{}\"",
                    args.log
                );
                eprintln!("       Expected format: eqlog_<PlayerName>_<Server>.txt");
                std::process::exit(1);
            }
            let eq_server = args
                .eq_server
                .clone()
                .unwrap_or_else(|| extract_eq_server(&args.log));
            register_stream(
                &args.server,
                admin_token,
                &args.game,
                &eq_server,
                &player_name,
            )
        }
        None => {
            let id = args.stream_id.as_deref().unwrap_or_else(|| {
                eprintln!("error: --stream-id is required when --admin-token is not provided");
                std::process::exit(1);
            });
            let token = args.stream_token.as_deref().unwrap_or_else(|| {
                eprintln!("error: --stream-token is required when --admin-token is not provided");
                std::process::exit(1);
            });
            (id.to_owned(), token.to_owned())
        }
    };

    let from = args.from.as_deref().map(|s| parse_datetime(s, "--from"));
    let to = args.to.as_deref().map(|s| parse_datetime(s, "--to"));

    let tail_from = match from {
        Some(dt) => TailFrom::Date(dt),
        None => TailFrom::Start,
    };

    let tail_config = TailConfig {
        from: tail_from,
        to: if args.dump { None } else { to },
        speed: if args.dump { None } else { Some(args.speed) },
        dump: args.dump,
    };

    let push_url = {
        let base = args.server.trim_end_matches('/');
        let ws_base = base
            .replacen("https://", "wss://", 1)
            .replacen("http://", "ws://", 1);
        format!("{ws_base}/ingest/{stream_id}")
    };

    info!("Replaying: {}", args.log);
    info!("Push URL:  {push_url}");
    if let Some(ref t) = to {
        info!("Range:     {:?} → {t}", args.from);
    }
    info!("Speed:     {}×", args.speed);
    info!("Player:    {player_name}");

    let done = Arc::new(AtomicBool::new(false));

    let shared = Arc::new(ArcSwap::from_pointee(CombatState::default()));
    let reset_flag = Arc::new(AtomicBool::new(false));
    let (broadcast_tx, _) = broadcast::channel::<Arc<CombatState>>(64);
    let (event_tx, event_rx) = tokio::sync::mpsc::unbounded_channel();
    let (line_tx, line_rx) = bounded::<String>(4096);

    // Tailer — exits naturally when the replay window is exhausted.
    {
        let path = args.log.clone();
        let done2 = Arc::clone(&done);
        thread::Builder::new()
            .name("eq-tailer".into())
            .spawn(move || {
                let rt = tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                    .expect("tailer rt");
                rt.block_on(tailer::tail(path, tail_config, line_tx));
                done2.store(true, Ordering::Relaxed);
            })
            .expect("spawn tailer");
    }

    // Parser
    {
        let shared2 = Arc::clone(&shared);
        let reset2 = Arc::clone(&reset_flag);
        let btx = broadcast_tx.clone();
        let pname = player_name.clone();
        thread::Builder::new()
            .name("eq-parser".into())
            .spawn(move || parser::run(line_rx, shared2, reset2, btx, event_tx, pname))
            .expect("spawn parser");
    }

    // Pusher
    {
        let url = push_url.clone();
        let token = stream_token.clone();
        thread::Builder::new()
            .name("eq-pusher".into())
            .spawn(move || {
                let rt = tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                    .expect("pusher rt");
                rt.block_on(pusher::push_to_server(
                    url,
                    token,
                    event_rx,
                    std::sync::Arc::new(std::sync::atomic::AtomicU64::new(0)),
                    std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
                    std::sync::Arc::new(std::sync::RwLock::new(None)),
                    std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
                    std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
                ));
            })
            .expect("spawn pusher");
    }

    // Wait for the tailer to finish replaying.
    loop {
        thread::sleep(Duration::from_millis(200));
        if done.load(Ordering::Relaxed) {
            // Give the pusher a moment to flush the last batch.
            thread::sleep(Duration::from_secs(2));
            info!("Replay complete.");
            break;
        }
    }
}

// ── Helpers ───────────────────────────────────────────────────────────────────

/// Call `POST /stream` on the server and return `(stream_id, stream_token)`.
/// Prints viewer URL and tokens to stdout so the user can bookmark the stream.
fn register_stream(
    server: &str,
    admin_token: &str,
    game: &str,
    eq_server: &str,
    player: &str,
) -> (String, String) {
    let url = format!("{}/stream", server.trim_end_matches('/'));
    let client = reqwest::blocking::Client::new();
    let resp = client
        .post(&url)
        .bearer_auth(admin_token)
        .json(&serde_json::json!({ "game": game, "server": eq_server, "player": player, "is_replay": true }))
        .send()
        .unwrap_or_else(|e| {
            eprintln!("error: failed to reach server at {url}: {e}");
            std::process::exit(1);
        });

    if !resp.status().is_success() {
        eprintln!("error: POST /stream returned {}", resp.status());
        std::process::exit(1);
    }

    let reg: RegisterResponse = resp.json().unwrap_or_else(|e| {
        eprintln!("error: could not parse registration response: {e}");
        std::process::exit(1);
    });

    let view_url = format!("{}{}", server.trim_end_matches('/'), reg.view_path);

    eprintln!("─────────────────────────────────────────────");
    eprintln!("  Stream registered for player: {player}");
    eprintln!("  stream_id    : {}", reg.stream_id);
    eprintln!("  stream_token : {}", reg.stream_token);
    eprintln!("  view_token   : {}", reg.view_token);
    eprintln!("  Viewer URL   : {view_url}");
    eprintln!("─────────────────────────────────────────────");

    (reg.stream_id, reg.stream_token)
}

fn parse_datetime(s: &str, flag: &str) -> NaiveDateTime {
    NaiveDateTime::parse_from_str(s, "%Y-%m-%d %H:%M:%S").unwrap_or_else(|e| {
        eprintln!("Invalid datetime for {flag} \"{s}\": {e}");
        eprintln!("Expected format: YYYY-MM-DD HH:MM:SS");
        std::process::exit(1);
    })
}

fn extract_player_name(path: &str) -> String {
    let filename = std::path::Path::new(path)
        .file_name()
        .and_then(|f| f.to_str())
        .unwrap_or(path);
    if let Some(rest) = filename.strip_prefix("eqlog_") {
        let without_ext = rest.trim_end_matches(".txt");
        if let Some(idx) = without_ext.rfind('_') {
            let name = &without_ext[..idx];
            if !name.is_empty() {
                return name.to_owned();
            }
        }
    }
    String::new()
}

fn extract_eq_server(path: &str) -> String {
    let filename = std::path::Path::new(path)
        .file_name()
        .and_then(|f| f.to_str())
        .unwrap_or(path);
    if let Some(rest) = filename.strip_prefix("eqlog_") {
        let without_ext = rest.trim_end_matches(".txt");
        if let Some(idx) = without_ext.rfind('_') {
            let server = &without_ext[idx + 1..];
            if !server.is_empty() {
                return server.to_owned();
            }
        }
    }
    String::new()
}
