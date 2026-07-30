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
        froklog::overlay::overlay::set_sound_enabled(config.sound_enabled);
        froklog::overlay::overlay::set_sound_volume_percent(config.sound_volume);
        froklog::overlay::overlay::set_active_sound_package(&config.sound_package);
        let handle = Arc::new(AppHandle::new(config));

        spawn_engine(Arc::clone(&handle));
        tray_run(handle);
    }

    #[cfg(not(feature = "tray"))]
    {
        let config = Config::load();
        if !config.local_ready() {
            eprintln!(
                "Config not ready. Edit {:?} and set log_path (server_url/stream_id/stream_token are only needed for remote push).",
                config_path_display()
            );
            std::process::exit(1);
        }
        let quit = Arc::new(AtomicBool::new(false));
        let restart = Arc::new(AtomicBool::new(false));
        // Mirror the tray build's monitor loop: run_engine_once returns when
        // the engine stops (e.g. the tailer exits or a restart is requested).
        // The old code ran it once and slept forever — a dead tailer left a
        // zombie process that never recovered.
        loop {
            restart.store(false, Ordering::Relaxed);
            run_engine_once(
                &config,
                Arc::clone(&restart),
                Arc::clone(&quit),
                Arc::new(AtomicU64::new(0)),
                Arc::new(AtomicBool::new(false)),
                Arc::new(std::sync::RwLock::new(None)),
            );
            if quit.load(Ordering::Relaxed) {
                break;
            }
            thread::sleep(Duration::from_millis(500));
        }
    }
}

// ── Engine monitor ────────────────────────────────────────────────────────────

#[cfg(feature = "tray")]
fn spawn_engine(handle: Arc<froklog::tray::tray::AppHandle>) {
    // Spawn the overlay once here so it survives engine restarts and live-reloads
    // its settings (font, alpha, enabled) from the shared config on each tick.
    froklog::overlay::overlay::spawn_overlay(Arc::clone(&handle));

    // Spawn the history overlay too — reads handle.overlay_history, which the
    // alert overlay appends to once a message finishes flying through it.
    froklog::overlay_history::overlay_history::spawn_overlay_history(Arc::clone(&handle));

    // Spawn the DPS meter once here too — reads handle.combat_state, which stays
    // valid across engine restarts (see run_engine_once).
    froklog::overlay_dps::overlay_dps::spawn_dps_meter(Arc::clone(&handle));

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

                if config.local_ready() {
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
    // Remote push is optional: local DPS meter / triggers / overlays keep running
    // from the tailer + parser even when no server is configured or the user has
    // switched remote logging off via the toggle.
    let remote_target: Option<(String, String)> = if config.remote_ready() {
        config
            .ingest_ws_url()
            .zip(config.stream_token.as_ref().cloned())
    } else {
        None
    };

    #[cfg(feature = "tray")]
    let shared: Arc<ArcSwap<CombatState>> = Arc::clone(&app_handle.combat_state);
    #[cfg(not(feature = "tray"))]
    let shared: Arc<ArcSwap<CombatState>> = Arc::new(ArcSwap::from_pointee(CombatState::default()));
    // Always publish a blank snapshot on (re)start — the tray build reuses a stable
    // ArcSwap across restarts (so the DPS meter overlay keeps a valid handle), so it
    // no longer gets a clean slate for free from a freshly-allocated ArcSwap.
    shared.store(Arc::new(CombatState::default()));
    #[cfg(feature = "tray")]
    let reset_flag: Arc<AtomicBool> = Arc::clone(&app_handle.reset_flag);
    #[cfg(not(feature = "tray"))]
    let reset_flag: Arc<AtomicBool> = Arc::new(AtomicBool::new(false));
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
    }

    if let Some((push_url, push_token)) = remote_target {
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
    } else {
        connected.store(false, Ordering::Relaxed);
        // Nobody is consuming parser events; drain and discard them so the
        // unbounded channel doesn't grow for the life of the session.
        thread::Builder::new()
            .name("eq-event-sink".into())
            .spawn(move || {
                let mut event_rx = event_rx;
                while event_rx.blocking_recv().is_some() {}
            })
            .expect("spawn event sink");
        info!("Remote logging disabled or not configured; running locally only");
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
