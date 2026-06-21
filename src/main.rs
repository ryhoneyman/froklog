// froklog — EverQuest log parser (Windows client binary)
// No visible window; lives in the system tray when the "tray" feature is active.
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::thread;
use std::time::Duration;

use arc_swap::ArcSwap;
use crossbeam_channel::bounded;
use tokio::sync::broadcast;
use tracing::info;

use froklog::config::Config;
use froklog::state::CombatState;
use froklog::tailer::{TailConfig, TailFrom};
#[cfg(feature = "tray")]
use froklog::triggers::engine::{TriggerConfig, TriggerEngine};
use froklog::{parser, pusher, tailer};

fn main() {
    tracing_subscriber::fmt()
        .with_env_filter(
            std::env::var("RUST_LOG")
                .unwrap_or_else(|_| "froklog=info".into())
                .as_str(),
        )
        .init();

    #[cfg(feature = "tray")]
    {
        use froklog::tray::tray::{run as tray_run, AppHandle};

        let config = Config::load();
        let handle = Arc::new(AppHandle::new(config));

        spawn_engine(Arc::clone(&handle));
        tray_run(handle);
    }

    #[cfg(not(feature = "tray"))]
    {
        let config = Config::load();
        if !config.is_ready() {
            eprintln!(
                "Config not ready. Edit {:?} and set log_path, server_url, stream_id, stream_token.",
                config_path_display()
            );
            std::process::exit(1);
        }
        let quit = Arc::new(AtomicBool::new(false));
        let restart = Arc::new(AtomicBool::new(false));
        run_engine_once(
            &config,
            Arc::clone(&restart),
            Arc::clone(&quit),
            Arc::new(AtomicU64::new(0)),
            Arc::new(AtomicBool::new(false)),
            Arc::new(std::sync::RwLock::new(None)),
        );
        loop {
            thread::sleep(Duration::from_secs(3600));
        }
    }
}

// ── Engine monitor ────────────────────────────────────────────────────────────

#[cfg(feature = "tray")]
fn spawn_engine(handle: Arc<froklog::tray::tray::AppHandle>) {
    thread::Builder::new()
        .name("eq-engine-monitor".into())
        .spawn(move || {
            loop {
                if handle.quit.load(Ordering::Relaxed) {
                    break;
                }

                // Respect the user's enable/disable toggle.
                if !handle.logging_enabled.load(Ordering::Relaxed) {
                    thread::sleep(Duration::from_secs(1));
                    continue;
                }

                let config = handle.config.lock().unwrap().clone();

                if config.is_ready() {
                    info!("Engine starting");
                    handle.restart.store(false, Ordering::Relaxed);
                    run_engine_once(
                        &config,
                        Arc::clone(&handle.restart),
                        Arc::clone(&handle.quit),
                        Arc::clone(&handle.events_sent),
                        Arc::clone(&handle.connected),
                        Arc::clone(&handle.last_connect_error),
                        Arc::clone(&handle),
                    );
                    info!("Engine stopped");
                } else {
                    thread::sleep(Duration::from_secs(1));
                    continue;
                }

                if handle.quit.load(Ordering::Relaxed) {
                    break;
                }
                thread::sleep(Duration::from_millis(500));
            }
        })
        .expect("spawn engine monitor");
}

// ── Engine ────────────────────────────────────────────────────────────────────

fn run_engine_once(
    config: &Config,
    restart: Arc<AtomicBool>,
    quit: Arc<AtomicBool>,
    events_sent: Arc<AtomicU64>,
    connected: Arc<AtomicBool>,
    last_connect_error: Arc<std::sync::RwLock<Option<String>>>,
    #[cfg(feature = "tray")] app_handle: Arc<froklog::tray::tray::AppHandle>,
) {
    let log_path = match config.log_path.as_ref() {
        Some(p) => p.clone(),
        None => return,
    };
    let push_url = match config.ingest_ws_url() {
        Some(u) => u,
        None => return,
    };
    let push_token = match config.stream_token.as_ref() {
        Some(t) => t.clone(),
        None => return,
    };

    let shared: Arc<ArcSwap<CombatState>> = Arc::new(ArcSwap::from_pointee(CombatState::default()));
    let reset_flag = Arc::new(AtomicBool::new(false));
    let (broadcast_tx, _) = broadcast::channel::<Arc<CombatState>>(64);
    let (event_tx, event_rx) = tokio::sync::mpsc::unbounded_channel();
    // Tailer output → splitter → parser_rx (parser) + trigger_rx (trigger engine).
    let (line_tx, tail_rx) = bounded::<String>(4096);
    let (parser_tx, line_rx) = bounded::<String>(4096);
    let (trigger_tx, _trigger_rx) = bounded::<String>(1024);

    let player_name = config.effective_player_name();
    info!("Watching: {log_path}  player: {player_name}");

    let tail_config = TailConfig {
        from: TailFrom::End,
        to: None,
        speed: None,
        dump: false,
    };

    {
        let path = log_path.clone();
        let restart2 = Arc::clone(&restart);
        let quit2 = Arc::clone(&quit);
        let restart3 = Arc::clone(&restart);
        let quit3 = Arc::clone(&quit);
        thread::Builder::new()
            .name("eq-tailer".into())
            .spawn(move || {
                let rt = tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                    .expect("tailer rt");
                rt.block_on(async move {
                    tokio::select! {
                        _ = tailer::tail(path, tail_config, line_tx) => {}
                        _ = async {
                            loop {
                                tokio::time::sleep(Duration::from_millis(200)).await;
                                if restart2.load(Ordering::Relaxed) || quit2.load(Ordering::Relaxed) {
                                    break;
                                }
                            }
                        } => {}
                    }
                });
                // If the tailer exited on its own (not due to a restart/quit signal),
                // trigger a restart so the engine monitor re-opens the log file.
                if !quit3.load(Ordering::Relaxed) {
                    restart3.store(true, Ordering::Relaxed);
                }
            })
            .expect("spawn tailer");
    }

    // Splitter: fan the tailer output to the parser channel and the trigger channel.
    {
        thread::Builder::new()
            .name("eq-splitter".into())
            .spawn(move || {
                for line in tail_rx {
                    let _ = parser_tx.send(line.clone());
                    // Drop lines if the trigger channel is full rather than blocking the parser.
                    let _ = trigger_tx.try_send(line);
                }
            })
            .expect("spawn splitter");
    }

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

    // Trigger engine + overlay (tray builds only).
    #[cfg(feature = "tray")]
    {
        let trigger_cfg = TriggerConfig::load();
        let overlay_queue = Arc::clone(&app_handle.overlay_queue);
        let engine = TriggerEngine::new(&trigger_cfg, Arc::clone(&overlay_queue));
        *app_handle.trigger_engine.lock().unwrap() = Some(engine.clone());

        // Trigger engine thread — processes log lines and advances timers.
        thread::Builder::new()
            .name("eq-triggers".into())
            .spawn(move || {
                let tick = std::time::Duration::from_millis(100);
                loop {
                    // Drain all pending lines without blocking longer than one tick.
                    let deadline = std::time::Instant::now() + tick;
                    loop {
                        match _trigger_rx.recv_timeout(std::time::Duration::from_millis(5)) {
                            Ok(line) => engine.process_line(&line),
                            Err(crossbeam_channel::RecvTimeoutError::Timeout) => {}
                            Err(crossbeam_channel::RecvTimeoutError::Disconnected) => return,
                        }
                        if std::time::Instant::now() >= deadline {
                            break;
                        }
                    }
                    engine.tick();
                }
            })
            .expect("spawn trigger engine");

        // Spawn overlay window if enabled.
        if config.overlay_enabled {
            froklog::overlay::overlay::spawn_overlay(config.clone(), overlay_queue);
        }
    }

    {
        let restart_p = Arc::clone(&restart);
        let quit_p = Arc::clone(&quit);
        thread::Builder::new()
            .name("eq-pusher".into())
            .spawn(move || {
                let rt = tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                    .expect("pusher rt");
                rt.block_on(pusher::push_to_server(
                    push_url,
                    push_token,
                    event_rx,
                    events_sent,
                    connected,
                    last_connect_error,
                    restart_p,
                    quit_p,
                ));
            })
            .expect("spawn pusher");
        info!("Pushing events to remote server");
    }

    loop {
        thread::sleep(Duration::from_millis(200));
        if restart.load(Ordering::Relaxed) || quit.load(Ordering::Relaxed) {
            break;
        }
    }
}

// ── Helpers ───────────────────────────────────────────────────────────────────

#[cfg(not(feature = "tray"))]
fn config_path_display() -> String {
    #[cfg(target_os = "windows")]
    {
        std::env::var("APPDATA").unwrap_or_else(|_| ".".into()) + r"\froklog\config.toml"
    }
    #[cfg(not(target_os = "windows"))]
    {
        std::env::var("HOME").unwrap_or_else(|_| ".".into()) + "/.config/froklog/config.toml"
    }
}
