// froklog — EverQuest log parser
// No visible window; lives in the system tray when the "tray" feature is active.
// `windows_subsystem` is a Windows-only attribute (suppresses the console
// window in release builds) — must stay gated on target_os too, or it errors
// when this is built for any other platform.
#![cfg_attr(
    all(not(debug_assertions), target_os = "windows"),
    windows_subsystem = "windows"
)]

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::thread;
use std::time::Duration;
#[cfg(feature = "tray")]
use std::time::Instant;

use arc_swap::ArcSwap;
use crossbeam_channel::bounded;
use tokio::sync::broadcast;
use tracing::info;

use froklog::config::Config;
#[cfg(feature = "tray")]
use froklog::config::LogWatchMode;
use froklog::state::CombatState;
use froklog::tailer::{TailConfig, TailFrom};
#[cfg(feature = "tray")]
use froklog::triggers::engine::{TriggerConfig, TriggerEngine};
use froklog::{parser, pusher, tailer};

fn main() {
    // Must be the first X11-related thing this process does, on Linux, when
    // the tray UI runs: froklog has multiple OS threads touching GTK/Xlib
    // (the main thread pumps GTK's loop for the tray icon; rfd's file
    // dialogs run GTK on their own thread), and modern GTK3 no longer calls
    // XInitThreads() for you (removed years ago; gdk_threads_init() is now
    // a no-op) — its absence is a documented crash class independent of
    // this codebase (CEF, several other GTK apps hit the identical stack),
    // not specific to any one windowing setup: Xlib lazily creates internal
    // per-object locks the first time something like XGetDefault() runs,
    // and without XInitThreads() first, that lock is left null. Confirmed
    // here with live thread dumps under two different unrelated call paths
    // (cairo's font-options lookup rendering the tray icon's menu; libXcursor's
    // theme lookup mapping rfd's file-chooser dialog) — both are just first
    // GTK-widget-realization codepaths that happen to touch an X resource-
    // manager default.
    #[cfg(all(feature = "tray", target_os = "linux"))]
    {
        #[link(name = "X11")]
        unsafe extern "C" {
            fn XInitThreads() -> std::os::raw::c_int;
        }
        unsafe {
            XInitThreads();
        }
    }

    // Point `ort` (piper-rs's onnxruntime binding, see src/tts.rs) at the
    // bundled shared library instead of trying to download/link its own —
    // must happen before any piper/ort code runs, so this stays as early
    // in main() as possible.
    #[cfg(feature = "tray")]
    {
        let lib_name = if cfg!(target_os = "windows") {
            "onnxruntime.dll"
        } else {
            "libonnxruntime.so"
        };
        std::env::set_var(
            "ORT_DYLIB_PATH",
            froklog::assets::runtime_dir().join(lib_name),
        );
    }

    tracing_subscriber::fmt()
        .with_env_filter(
            std::env::var("RUST_LOG")
                .unwrap_or_else(|_| "froklog=info".into())
                .as_str(),
        )
        .init();

    #[cfg(feature = "tray")]
    {
        use froklog::tray::tray::{enforce_single_instance, run as tray_run, AppHandle};

        let config = Config::load();
        froklog::overlay::overlay::set_sound_enabled(config.sound_enabled);
        froklog::overlay::overlay::set_sound_volume_percent(config.sound_volume);
        froklog::overlay::overlay::set_active_sound_package(&config.sound_package);
        let handle = Arc::new(AppHandle::new(config));

        // Exits immediately (never returns) if another instance is already
        // running, after asking it to raise Settings — must happen before
        // spawning the engine so a duplicate launch doesn't also start a
        // second log tailer/pusher.
        enforce_single_instance(&handle);

        spawn_engine(Arc::clone(&handle));
        spawn_profile_watcher(Arc::clone(&handle));
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
    // The three overlay windows (alert/history/DPS meter) are no longer
    // spawned here on their own OS threads — Slint windows must be created
    // on, and driven by, the thread that owns the event loop, so
    // `tray::run()` creates them itself right before entering
    // `slint::run_event_loop()`. They still read `handle.combat_state` /
    // `handle.overlay_history` / config live on their own timers, same as
    // before, just ticking via `slint::Timer` instead of `WM_TIMER`.
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

/// Coarsely polls, once every `AUTO_DETECT_INTERVAL`, which configured log
/// profile was written to most recently — but only while the user is in
/// Auto mode; in Pinned mode this thread does no filesystem work at all.
/// When the resolved path changes, it requests an engine restart via the
/// same `handle.restart` flag `spawn_engine`'s monitor loop already watches,
/// so no separate teardown/rebuild logic is needed here.
#[cfg(feature = "tray")]
fn spawn_profile_watcher(handle: Arc<froklog::tray::tray::AppHandle>) {
    const AUTO_DETECT_INTERVAL: Duration = Duration::from_secs(8);

    thread::Builder::new()
        .name("eq-profile-watcher".into())
        .spawn(move || {
            let mut last_resolved: Option<String> = None;
            loop {
                if handle.quit.load(Ordering::Relaxed) {
                    break;
                }
                thread::sleep(AUTO_DETECT_INTERVAL);

                let config = handle.config.lock().unwrap().clone();
                if config.log_watch_mode != LogWatchMode::Auto {
                    continue;
                }
                let resolved = config.resolve_active_log_path();
                if resolved != last_resolved {
                    // Don't restart on the very first observation — the
                    // engine monitor already picks up the initial path.
                    if last_resolved.is_some() {
                        info!(
                            "Auto-detected log switch: {:?} -> {:?}",
                            last_resolved, resolved
                        );
                        handle.restart.store(true, Ordering::Relaxed);
                    }
                    last_resolved = resolved;
                }
            }
        })
        .expect("spawn profile watcher");
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
    let Some(active_profile) = config.resolve_active_profile() else {
        return;
    };
    let log_path = active_profile.path.clone();
    #[cfg(feature = "tray")]
    {
        *app_handle.active_log_path.lock().unwrap() = Some(log_path.clone());
    }
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

    let player_name = active_profile.effective_player();
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
        #[cfg(feature = "tray")]
        let last_log_activity = Arc::clone(&app_handle.last_log_activity);
        thread::Builder::new()
            .name("eq-splitter".into())
            .spawn(move || {
                for line in tail_rx {
                    #[cfg(feature = "tray")]
                    {
                        *last_log_activity.lock().unwrap() = Instant::now();
                    }
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
