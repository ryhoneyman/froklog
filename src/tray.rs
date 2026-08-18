/// Windows systray integration.
///
/// Runs on the main thread (required by tray-icon/Slint on Windows — Slint
/// windows must be created on, and driven by, the thread that owns the
/// event loop). All heavy work (tailer, parser, pusher) happens on
/// background threads/tasks coordinated through the `AppHandle` passed in
/// from main; UI-touching work (tray clicks, menu clicks, opening
/// windows) is marshaled onto this thread via `slint::invoke_from_event_loop`.
#[cfg(feature = "tray")]
#[allow(clippy::module_inception)]
pub mod tray {
    use std::cell::RefCell;
    use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
    use std::sync::RwLock;
    use std::sync::{Arc, Mutex};
    use std::time::{Duration, Instant};

    use arc_swap::ArcSwap;
    use tracing::info;
    use tray_icon::{
        menu::{Menu, MenuEvent, MenuItem, PredefinedMenuItem},
        TrayIconBuilder,
    };

    use crate::config::Config;
    use crate::state::CombatState;
    use crate::triggers::engine::TriggerEngine;

    // ── Menu item IDs ─────────────────────────────────────────────────────────

    const ID_STATUS: &str = "status";
    const ID_TOGGLE_LOGGING: &str = "toggle_logging";
    const ID_TOGGLE_OVERLAY: &str = "toggle_overlay";
    const ID_TOGGLE_METER: &str = "toggle_meter";
    const ID_SETTINGS: &str = "settings";
    const ID_COPY_VIEWER_URL: &str = "copy_viewer_url";
    const ID_OPEN_VIEWER_URL: &str = "open_viewer_url";
    const ID_QUIT: &str = "quit";

    const TICK_SECS: u64 = 5;

    // tray-icon's types are `!Send` (Rc-based internally) — see `run()`'s
    // comment on why these live in thread-locals instead of being captured
    // directly into `invoke_from_event_loop` closures.
    thread_local! {
        static TRAY: RefCell<Option<tray_icon::TrayIcon>> = const { RefCell::new(None) };
        static TOGGLE_ITEM: RefCell<Option<MenuItem>> = const { RefCell::new(None) };
        static OVERLAY_TOGGLE_ITEM: RefCell<Option<MenuItem>> = const { RefCell::new(None) };
        static METER_TOGGLE_ITEM: RefCell<Option<MenuItem>> = const { RefCell::new(None) };
    }

    // ── AppHandle ─────────────────────────────────────────────────────────────

    /// Shared state between the tray event loop and the background engine.
    pub struct AppHandle {
        pub config: Arc<Mutex<Config>>,
        /// Set to true when the engine should restart (new log / credentials).
        pub restart: Arc<AtomicBool>,
        /// Set to true to request a clean shutdown.
        pub quit: Arc<AtomicBool>,
        /// Mirrors config.logging_enabled; engine monitor polls this.
        pub logging_enabled: Arc<AtomicBool>,
        /// Prevents opening multiple Settings windows simultaneously. The
        /// window itself (a Slint component, `!Send`/`!Sync`) lives in a
        /// thread-local inside `settings_window` instead — everything that
        /// touches it runs on this module's single UI thread via
        /// `invoke_from_event_loop` anyway, so a cross-thread handle here
        /// would just be dead weight (and Slint components aren't Send).
        pub settings_open: Arc<AtomicBool>,
        /// True while the Settings dialog's "Show All Windows" button is
        /// active — forces the alert overlay, history overlay, and DPS meter
        /// to render a draggable placeholder even with no real content, so
        /// the user can reposition them without needing the (now-removed)
        /// in-window pin. Cleared when the Settings dialog closes.
        pub force_show_windows: Arc<AtomicBool>,
        /// Live-aggregated combat state, built locally by the parser. Created once so
        /// it survives engine restarts — the DPS meter overlay reads it on a timer.
        pub combat_state: Arc<ArcSwap<CombatState>>,
        /// Set to true to ask the parser to clear combat totals (keeps
        /// `lines_parsed`/player identity) on the next log line. Consumed by
        /// `parser::run`'s hot loop, then reset back to false. Created once so
        /// it's triggerable from any UI (e.g. the DPS meter's clear button)
        /// regardless of engine restarts.
        pub reset_flag: Arc<AtomicBool>,
        /// Cumulative count of events successfully pushed to the server.
        pub events_sent: Arc<AtomicU64>,
        /// True while the pusher has an active WebSocket connection.
        pub connected: Arc<AtomicBool>,
        /// Last pusher connection error, cleared on successful connect.
        pub last_connect_error: Arc<RwLock<Option<String>>>,
        /// Live trigger engine — replaced on reload.
        pub trigger_engine: Arc<Mutex<Option<TriggerEngine>>>,
        /// Shared queue from trigger engine → overlay window.
        pub overlay_queue: Arc<Mutex<Vec<crate::triggers::engine::OverlayEvent>>>,
        /// Messages that finished flying through the alert overlay, read by
        /// the history overlay window. Created once so it survives engine
        /// restarts, same reasoning as `combat_state`.
        pub overlay_history: Arc<Mutex<Vec<crate::overlay_history::overlay_history::HistoryEntry>>>,
        /// The log path the engine is currently (or was last) watching, as
        /// resolved by `Config::resolve_active_log_path()`. Cached here so
        /// the tray tooltip and Settings status label can display it without
        /// each independently re-stat'ing every log profile — only
        /// `run_engine_once` (on start) and the profile-watcher thread (on a
        /// detected change) write to this.
        pub active_log_path: Arc<Mutex<Option<String>>>,
        /// Wall-clock time the tailer last saw a log line (any line, not
        /// just ones the parser recognizes) — read by every overlay window
        /// to force-hide when the game isn't logging. See `log_inactive`.
        /// Written from the splitter thread in `main.rs`.
        pub last_log_activity: Arc<Mutex<Instant>>,
    }

    impl AppHandle {
        pub fn new(config: Config) -> Self {
            let logging_enabled = config.logging_enabled;
            Self {
                logging_enabled: Arc::new(AtomicBool::new(logging_enabled)),
                config: Arc::new(Mutex::new(config)),
                restart: Arc::new(AtomicBool::new(false)),
                quit: Arc::new(AtomicBool::new(false)),
                settings_open: Arc::new(AtomicBool::new(false)),
                force_show_windows: Arc::new(AtomicBool::new(false)),
                combat_state: Arc::new(ArcSwap::from_pointee(CombatState::default())),
                reset_flag: Arc::new(AtomicBool::new(false)),
                events_sent: Arc::new(AtomicU64::new(0)),
                connected: Arc::new(AtomicBool::new(false)),
                last_connect_error: Arc::new(RwLock::new(None)),
                trigger_engine: Arc::new(Mutex::new(None)),
                overlay_queue: Arc::new(Mutex::new(Vec::new())),
                overlay_history: Arc::new(Mutex::new(Vec::new())),
                active_log_path: Arc::new(Mutex::new(None)),
                last_log_activity: Arc::new(Mutex::new(Instant::now())),
            }
        }

        /// True when no log line has arrived in the last `secs` seconds —
        /// `secs == 0` means the feature is off (`Config::overlay_hide_inactive_secs`'s
        /// "Never" option) and this always returns `false`.
        pub fn log_inactive(&self, secs: u32) -> bool {
            secs > 0
                && self.last_log_activity.lock().unwrap().elapsed()
                    > Duration::from_secs(secs as u64)
        }
    }

    // ── Single instance ───────────────────────────────────────────────────────

    /// Loopback-only TCP port used purely as a single-instance lock + signal
    /// channel: binding it *is* the lock (the OS only lets one process hold
    /// it), and a second launch connects to it to ask the first instance to
    /// raise Settings instead of starting a second copy of everything
    /// (duplicate log tailer, duplicate overlay windows, duplicate pushes to
    /// the server). Works identically on Windows and Linux, so no extra
    /// dependency for this.
    const SINGLE_INSTANCE_PORT: u16 = 47811;

    /// Must be called before any other startup work (engine spawn, tray/
    /// window creation). If another instance is already running, this asks
    /// it to raise Settings and exits the process immediately — it never
    /// returns in that case. Otherwise spawns a background thread that
    /// services raise-requests from later launches and returns normally.
    pub fn enforce_single_instance(handle: &Arc<AppHandle>) {
        use std::io::Write;
        use std::net::{Ipv4Addr, SocketAddr, TcpListener, TcpStream};

        let addr = SocketAddr::from((Ipv4Addr::LOCALHOST, SINGLE_INSTANCE_PORT));

        if let Ok(mut stream) = TcpStream::connect_timeout(&addr, Duration::from_millis(300)) {
            let _ = stream.write_all(b"raise\n");
            info!("froklog is already running — asked it to raise Settings, exiting");
            std::process::exit(0);
        }

        // Nothing answered — either we're first, or the port is stuck in a
        // transient state (e.g. TIME_WAIT from a just-exited instance).
        // Either way, failing to bind isn't worth blocking startup over:
        // just run normally without single-instance protection this time.
        let Ok(listener) = TcpListener::bind(addr) else {
            return;
        };

        let handle = Arc::clone(handle);
        std::thread::Builder::new()
            .name("single-instance-listener".into())
            .spawn(move || {
                for stream in listener.incoming().flatten() {
                    drop(stream);
                    let handle = Arc::clone(&handle);
                    let _ = slint::invoke_from_event_loop(move || {
                        open_or_raise_settings(&handle, 0);
                    });
                }
            })
            .expect("spawn single-instance listener thread");
    }

    // ── Entry point ───────────────────────────────────────────────────────────

    /// Entry point — blocks until the user chooses Quit. Must run on the
    /// process's main thread: it creates the tray icon, creates the three
    /// always-alive overlay windows (alert/history/DPS meter — hidden until
    /// they have something to show), and then owns Slint's event loop for
    /// the rest of the process lifetime. Every Slint window created anywhere
    /// in the app (Settings, trigger editors, overlays) must be created from
    /// a callback that eventually runs on this same thread.
    pub fn run(handle: Arc<AppHandle>) {
        // Must run before any Slint window is created (including the
        // overlays `spawn_all` creates further down) — see
        // `overlay_draw::hide_from_taskbar`'s doc comment for why this hook
        // exists: it tags every window `_NET_WM_WINDOW_TYPE_UTILITY` as an
        // initial creation attribute, before the window is ever mapped, so
        // the window manager excludes it from the taskbar with no
        // post-creation race. The hook fires inside each window's `::new()`
        // (Slint builds the window adapter, and applies this hook, there —
        // not deferred until `.show()`), so Settings opts out via
        // `suppress_utility_window_hint` around its own `::new()` call.
        // Force X11 (via XWayland on an actual Wayland session) instead of
        // letting winit auto-select native Wayland. Three independently
        // confirmed Wayland gaps — softbuffer hardcoding an alpha-less pixel
        // format, winit's Wayland `set_window_level` being a literal no-op,
        // and `xdg_toplevel` having no window-position-query capability at
        // the protocol level at all — none of which exist on X11 with a
        // real compositor running (confirmed live: this dev box's own
        // Xrdp/xfwm4 X11 session already renders transparency correctly
        // with the unmodified renderer, and winit's X11 `set_window_level`
        // uses real `_NET_WM_STATE_ABOVE` EWMH calls, not a stub). XWayland
        // is enabled by default on every mainstream compositor (GNOME, KDE,
        // Sway, Hyprland) for legacy-app compatibility, so this reuses the
        // X11-specific code this app already has and already exercises
        // (`overlay_draw.rs`'s `true_window_position`/`skip_taskbar`)
        // instead of a from-scratch Wayland-native layer-shell rewrite —
        // see memory/project_wayland_overlay_investigation.md for the full
        // investigation this decision came out of.
        #[cfg(target_os = "linux")]
        {
            use slint::winit_030::EventLoopBuilder;
            use winit::platform::x11::{
                EventLoopBuilderExtX11, WindowAttributesExtX11, WindowType,
            };

            let mut x11_event_loop_builder: EventLoopBuilder =
                winit::event_loop::EventLoop::with_user_event();
            x11_event_loop_builder.with_x11();
            let x11_result = slint::BackendSelector::new()
                .with_winit_event_loop_builder(x11_event_loop_builder)
                .with_winit_window_attributes_hook(|attrs| {
                    if crate::overlay_draw::overlay_draw::utility_window_hint_suppressed() {
                        attrs
                    } else {
                        // Override-redirect: the WM never manages these
                        // windows, which is what actually keeps them above a
                        // fullscreen game on compositors that ignore
                        // `_NET_WM_STATE_ABOVE` for managed X11 windows
                        // (confirmed live on COSMIC, where the game is a
                        // native Wayland surface outside the X11 stack
                        // entirely and even wmctrl's ADD_ABOVE is dropped —
                        // an override-redirect window rides the compositor's
                        // unmanaged-surface layer instead, same as menus and
                        // tooltips, and provably stays over the game).
                        // Trade-off: no WM services — interactive move is
                        // reimplemented in overlay_shell::begin_drag, and
                        // stacking upkeep is a direct raise in
                        // reassert_topmost. The Utility type hint stays for
                        // anything that still reads it.
                        attrs
                            .with_x11_window_type(vec![WindowType::Utility])
                            .with_override_redirect(true)
                    }
                })
                .select();

            // No XWayland available (rare on mainstream desktops) — fall
            // back to normal auto-detection (native Wayland) rather than
            // crashing outright. Known-imperfect-but-functional beats not
            // starting at all.
            if let Err(err) = x11_result {
                tracing::warn!(
                    "failed to force X11 backend ({err}), falling back to auto-detected backend"
                );
                slint::BackendSelector::new()
                    .with_winit_window_attributes_hook(|attrs| {
                        if crate::overlay_draw::overlay_draw::utility_window_hint_suppressed() {
                            attrs
                        } else {
                            attrs.with_x11_window_type(vec![WindowType::Utility])
                        }
                    })
                    .select()
                    .expect("select winit backend with window-attributes hook");
            }
        }

        // tray-icon's GTK/appindicator backend needs GTK initialized on
        // whichever thread creates its widgets — must run before `build_tray`
        // below. See the pump timer further down for why GTK's loop also
        // needs periodic servicing on this same thread.
        #[cfg(target_os = "linux")]
        gtk::init().expect("init gtk (needed for the tray icon and native dialogs on Linux)");

        let logging_on = handle.logging_enabled.load(Ordering::Relaxed);
        let overlay_on = handle.config.lock().unwrap().overlay_alert.enabled;
        let meter_on = handle.config.lock().unwrap().overlay_meter.enabled;
        let (
            tray,
            toggle_item,
            overlay_toggle_item,
            meter_toggle_item,
            status_item,
            copy_url_item,
            open_url_item,
            menu,
        ) = build_tray(
            &handle.config.lock().unwrap(),
            logging_on,
            overlay_on,
            meter_on,
        );
        // tray-icon's types (Rc-based internally) aren't Send, so they can't
        // be captured into an `invoke_from_event_loop` closure built on a
        // background thread — even though that closure only ever *runs* on
        // this UI thread, Rust requires the closure itself to be Send at
        // construction time. Thread-locals sidestep this: every access
        // (from the menu-event handler below and the status timer) happens
        // on this same UI thread regardless.
        TRAY.with(|t| *t.borrow_mut() = Some(tray));
        TOGGLE_ITEM.with(|t| *t.borrow_mut() = Some(toggle_item));
        OVERLAY_TOGGLE_ITEM.with(|t| *t.borrow_mut() = Some(overlay_toggle_item));
        METER_TOGGLE_ITEM.with(|t| *t.borrow_mut() = Some(meter_toggle_item));

        // Create the three always-alive overlay windows now, on this thread,
        // before entering the event loop. Each sets up its own Slint Timer
        // for its animation/refresh tick — no dedicated OS thread anymore
        // (Slint windows must live on the thread that drives their event
        // loop), matching how overlay_dps.rs/overlay_history.rs/overlay.rs
        // already read config live on a timer, just relocated from a
        // WM_TIMER callback to a slint::Timer callback.
        crate::overlay_registry::overlay_registry::spawn_all(&handle);

        #[cfg(target_os = "linux")]
        warn_if_no_compositor(&handle);

        // Clicking the tray icon (either button) only pops up its native
        // context menu — no custom left/right-click handling. Opening or
        // raising Settings happens solely from the menu's own "Settings…"
        // item (ID_SETTINGS below); a window-raise triggered directly by
        // the click itself steals focus while the OS is still animating
        // the menu open, which closed it before it was even visible.

        // ── Menu events ───────────────────────────────────────────────────
        {
            let handle = Arc::clone(&handle);
            std::thread::Builder::new()
                .name("tray-menu-events".into())
                .spawn(move || {
                    while let Ok(evt) = MenuEvent::receiver().recv() {
                        let handle = Arc::clone(&handle);
                        let _ = slint::invoke_from_event_loop(move || match evt.id.0.as_str() {
                            ID_TOGGLE_LOGGING => {
                                toggle_logging(&handle);
                            }
                            ID_TOGGLE_OVERLAY => {
                                toggle_overlay(&handle);
                            }
                            ID_TOGGLE_METER => {
                                toggle_meter(&handle);
                            }
                            ID_SETTINGS => {
                                open_or_raise_settings(&handle, 0);
                            }
                            ID_OPEN_VIEWER_URL => {
                                let url = handle.config.lock().unwrap().stream_url();
                                if let Some(url) = url {
                                    open_in_browser(&url);
                                }
                            }
                            ID_COPY_VIEWER_URL => {
                                let url = handle.config.lock().unwrap().stream_url();
                                if let Some(url) = url {
                                    copy_to_clipboard(&url);
                                }
                            }
                            ID_QUIT => {
                                handle.quit.store(true, Ordering::Relaxed);
                                let _ = slint::quit_event_loop();
                            }
                            _ => {}
                        });
                    }
                })
                .expect("spawn tray-menu-events thread");
        }

        // ── Periodic tray status refresh ─────────────────────────────────
        // Replaces the old winit ControlFlow::WaitUntil-driven 1s fast-check
        // + 5s status tick with a single 1s-repeating Slint timer doing both
        // (a plain counter stands in for the old two-deadline tracking).
        let mut prev_count: u64 = 0;
        let mut prev_sample = Instant::now();
        let mut prev_connected: bool = false;
        let mut prev_icon_choice: Option<u8> = None;
        let mut ticks_since_slow = TICK_SECS; // fire the slow tick immediately on first run
        let mut error_items: Vec<MenuItem> = Vec::new();
        let mut prev_error: Option<String> = None;
        let status_timer = slint::Timer::default();
        {
            let handle = Arc::clone(&handle);
            status_timer.start(
                slint::TimerMode::Repeated,
                Duration::from_secs(1),
                move || {
                    let now = Instant::now();

                    // Fast check every tick: repaint the tray icon the moment
                    // the connection state flips.
                    let is_connected = handle.connected.load(Ordering::Relaxed);
                    if is_connected != prev_connected {
                        prev_connected = is_connected;
                        let logging_on = handle.logging_enabled.load(Ordering::Relaxed);
                        let cfg = handle.config.lock().unwrap();
                        let choice = icon_choice(&cfg, logging_on, is_connected);
                        if prev_icon_choice != Some(choice) {
                            prev_icon_choice = Some(choice);
                            TRAY.with(|t| {
                                if let Some(tray) = t.borrow().as_ref() {
                                    let _ = tray.set_icon(Some(make_icon(
                                        &cfg,
                                        logging_on,
                                        is_connected,
                                    )));
                                }
                            });
                        }
                    }

                    ticks_since_slow += 1;
                    if ticks_since_slow < TICK_SECS {
                        return;
                    }
                    ticks_since_slow = 0;

                    let cur = handle.events_sent.load(Ordering::Relaxed);
                    let elapsed = now.duration_since(prev_sample).as_secs_f64().max(0.001);
                    let rate = ((cur.saturating_sub(prev_count)) as f64 / elapsed * 60.0) as u32;
                    prev_count = cur;
                    prev_sample = now;

                    let logging_on = handle.logging_enabled.load(Ordering::Relaxed);
                    let is_connected = handle.connected.load(Ordering::Relaxed);
                    prev_connected = is_connected;
                    let cfg = handle.config.lock().unwrap();
                    let active_log_path = handle.active_log_path.lock().unwrap().clone();
                    let last_err = handle
                        .last_connect_error
                        .read()
                        .ok()
                        .and_then(|g| g.clone());

                    let choice = icon_choice(&cfg, logging_on, is_connected);
                    let icon_stale = prev_icon_choice != Some(choice);
                    prev_icon_choice = Some(choice);
                    TRAY.with(|t| {
                        if let Some(tray) = t.borrow().as_ref() {
                            let _ = tray.set_tooltip(Some(make_tooltip_full(
                                &cfg,
                                active_log_path.as_deref(),
                                logging_on,
                                is_connected,
                                rate,
                            )));
                            // Only on a real state change — see icon_choice's
                            // doc comment for why re-setting churns the icon
                            // file out from under the tray applet.
                            if icon_stale {
                                let _ =
                                    tray.set_icon(Some(make_icon(&cfg, logging_on, is_connected)));
                            }
                        }
                    });
                    status_item.set_text(make_status_text(
                        logging_on,
                        is_connected,
                        &cfg,
                        last_err.as_deref(),
                        rate,
                    ));

                    // Update wrapped error detail lines below the status item.
                    if last_err != prev_error {
                        for item in &error_items {
                            let _ = menu.remove(item);
                        }
                        error_items.clear();
                        if let Some(ref e) = last_err {
                            for (i, line) in word_wrap(e, 32).into_iter().enumerate() {
                                let item = MenuItem::new(line, false, None);
                                let _ = menu.insert(&item, 1 + i);
                                error_items.push(item);
                            }
                        }
                        prev_error = last_err;
                    }

                    let is_reg = cfg.is_registered();
                    copy_url_item.set_enabled(is_reg);
                    open_url_item.set_enabled(is_reg);
                },
            );
        }

        // Winit's event loop (which Slint's backend-winit drives) doesn't know
        // about GTK at all, so nothing services GTK's own GLib main loop —
        // without this, tray-icon's appindicator backend and rfd's GTK
        // dialogs sit dead on Linux (the tray icon never registers over
        // D-Bus, dialogs never paint). Draining it on a fast repeating timer
        // on this same thread stands in for a dedicated `gtk::main()` loop
        // without needing a second thread and the cross-thread marshaling
        // that would come with one.
        #[cfg(target_os = "linux")]
        let _gtk_pump_timer = {
            let t = slint::Timer::default();
            t.start(
                slint::TimerMode::Repeated,
                Duration::from_millis(16),
                || {
                    while gtk::events_pending() {
                        gtk::main_iteration();
                    }
                },
            );
            t
        };

        // Not run_event_loop(): that variant auto-quits once Slint's visible-window
        // keepalive count hits zero, and it has no visibility into our tray icon
        // (the `tray-icon` crate, not Slint's own SystemTrayIcon) or the overlay
        // windows, which stay hidden except while actively rendering. Without
        // this, hiding the last visible window — e.g. Settings' Save/Cancel/close
        // — silently exits the whole app instead of just closing that window.
        // Only the ID_QUIT menu handler's explicit quit_event_loop() should do that.
        slint::run_event_loop_until_quit().expect("run slint event loop");
    }

    // ── Settings open/raise ──────────────────────────────────────────────────

    fn open_or_raise_settings(handle: &Arc<AppHandle>, initial_tab: i32) {
        if !handle.settings_open.swap(true, Ordering::Relaxed) {
            crate::settings_window::settings_window::open_settings(Arc::clone(handle), initial_tab);
        } else {
            crate::settings_window::settings_window::raise_settings(initial_tab);
            bring_windows_forward(handle);
        }
    }

    // ── Overlay toggle ────────────────────────────────────────────────────────

    fn toggle_overlay(handle: &Arc<AppHandle>) {
        let now_on = {
            let mut cfg = handle.config.lock().unwrap();
            cfg.overlay_alert.enabled = !cfg.overlay_alert.enabled;
            let v = cfg.overlay_alert.enabled;
            cfg.save();
            v
        };
        let label = if now_on {
            "Hide Overlay"
        } else {
            "Show Overlay"
        };
        OVERLAY_TOGGLE_ITEM.with(|t| {
            if let Some(item) = t.borrow().as_ref() {
                item.set_text(label);
            }
        });
        // Overlay live-reloads overlay_enabled from config on its timer tick.
    }

    // ── DPS meter toggle ──────────────────────────────────────────────────────

    fn toggle_meter(handle: &Arc<AppHandle>) {
        let now_on = {
            let mut cfg = handle.config.lock().unwrap();
            cfg.overlay_meter.enabled = !cfg.overlay_meter.enabled;
            let v = cfg.overlay_meter.enabled;
            cfg.save();
            v
        };
        let label = if now_on {
            "Hide DPS Meter"
        } else {
            "Show DPS Meter"
        };
        METER_TOGGLE_ITEM.with(|t| {
            if let Some(item) = t.borrow().as_ref() {
                item.set_text(label);
            }
        });
        // Meter live-reloads meter_enabled from config on its timer tick.
    }

    // ── Logging toggle ────────────────────────────────────────────────────────

    fn toggle_logging(handle: &Arc<AppHandle>) {
        let was_on = handle.logging_enabled.fetch_xor(true, Ordering::Relaxed);
        let now_on = !was_on;

        // Persist to config.
        {
            let mut cfg = handle.config.lock().unwrap();
            cfg.logging_enabled = now_on;
            cfg.save();
        }

        // If we just enabled logging, trigger an engine restart.
        if now_on {
            handle.restart.store(true, Ordering::Relaxed);
        }

        let is_connected = handle.connected.load(Ordering::Relaxed);
        TRAY.with(|t| {
            if let Some(tray) = t.borrow().as_ref() {
                let _ = tray.set_icon(Some(make_icon(
                    &handle.config.lock().unwrap(),
                    now_on,
                    is_connected,
                )));
                let _ = tray.set_tooltip(Some(make_tooltip(
                    &handle.config.lock().unwrap(),
                    handle.active_log_path.lock().unwrap().as_deref(),
                    now_on,
                )));
            }
        });

        let label = if now_on {
            "Disable Logging"
        } else {
            "Enable Logging"
        };
        TOGGLE_ITEM.with(|t| {
            if let Some(item) = t.borrow().as_ref() {
                item.set_text(label);
            }
        });
    }

    // ── Bring windows forward ────────────────────────────────────────────────

    /// Brings the Settings window (if open) to the foreground on a tray
    /// right-click. The overlay windows no longer need an explicit z-order
    /// reassertion here now that they're plain Slint `always-on-top`
    /// windows managed on the same UI thread rather than raw HWNDs poked
    /// from outside — they stay topmost on their own.
    fn bring_windows_forward(_handle: &Arc<AppHandle>) {
        crate::settings_window::settings_window::raise_settings_no_tab_change();
    }

    // ── Compositor check (Linux) ─────────────────────────────────────────────

    /// X11 has no protocol-level alpha blending for a top-level window —
    /// something has to actually composite it against the desktop, which is
    /// what a compositor (picom, or a DE's built-in one like Mutter/KWin)
    /// does. Without one, the alert overlay's "transparent" background
    /// renders as an opaque box instead (see overlay_alert.slint's
    /// `panel-fallback` doc comment for the deeper root cause). Wayland
    /// compositors always composite, so this never fires there.
    /// `gdk_screen_is_composited()` is the standard toolkit-level way to
    /// check this — same mechanism GTK/Qt apps rely on — and comes for free
    /// via the `gtk` crate already initialized above for the tray icon, so
    /// no new dependency is needed. Also used by overlay.rs to decide
    /// whether the alert window needs its opaque `panel-fallback` backing —
    /// that used to be forced on for all of Linux regardless of whether a
    /// compositor was actually running, which drew an unwanted black
    /// rounded-rect behind the alert text on setups (Mutter/KWin/picom)
    /// that composite fine.
    #[cfg(target_os = "linux")]
    pub(crate) fn is_composited() -> bool {
        gtk::gdk::Screen::default()
            .map(|screen| screen.is_composited())
            .unwrap_or(true)
    }

    #[cfg(target_os = "linux")]
    fn warn_if_no_compositor(handle: &Arc<AppHandle>) {
        let alert_style_uses_overlay = {
            let config = handle.config.lock().unwrap();
            match config.alert_style {
                crate::config::AlertStyle::Separate => config.overlay_alert.enabled,
                crate::config::AlertStyle::Merged => config.overlay_merged.enabled,
            }
        };
        if !alert_style_uses_overlay {
            return;
        }
        if is_composited() {
            return;
        }
        std::thread::Builder::new()
            .name("compositor-warning".into())
            .spawn(|| {
                rfd::MessageDialog::new()
                    .set_title("Alert Overlay")
                    .set_description(
                        "No compositor is running, so the alert overlay can't be made \
                         transparent and will show as an opaque box instead.\n\n\
                         Install and run a lightweight compositor (e.g. picom) to fix this.",
                    )
                    .set_level(rfd::MessageLevel::Warning)
                    .set_buttons(rfd::MessageButtons::Ok)
                    .show();
            })
            .expect("spawn compositor warning dialog thread");
    }

    // ── Clipboard ─────────────────────────────────────────────────────────────

    pub(crate) fn copy_to_clipboard(text: &str) {
        if let Ok(mut clipboard) = arboard::Clipboard::new() {
            let _ = clipboard.set_text(text);
        }
    }

    // ── Browser launch ────────────────────────────────────────────────────────

    fn open_in_browser(url: &str) {
        let _ = open::that(url);
    }

    // ── Tray construction ─────────────────────────────────────────────────────

    fn build_tray(
        cfg: &Config,
        logging_on: bool,
        overlay_on: bool,
        meter_on: bool,
    ) -> (
        tray_icon::TrayIcon,
        MenuItem,
        MenuItem,
        MenuItem,
        MenuItem,
        MenuItem,
        MenuItem,
        Menu,
    ) {
        let menu = Menu::new();

        let status_item = MenuItem::with_id(
            ID_STATUS,
            make_status_text(logging_on, false, cfg, None, 0),
            false,
            None,
        );
        let sep_status = PredefinedMenuItem::separator();
        let toggle_label = if logging_on {
            "Disable Logging"
        } else {
            "Enable Logging"
        };
        let toggle_item = MenuItem::with_id(ID_TOGGLE_LOGGING, toggle_label, true, None);

        let overlay_toggle_label = if overlay_on {
            "Hide Overlay"
        } else {
            "Show Overlay"
        };
        let overlay_toggle_item =
            MenuItem::with_id(ID_TOGGLE_OVERLAY, overlay_toggle_label, true, None);

        let meter_toggle_label = if meter_on {
            "Hide DPS Meter"
        } else {
            "Show DPS Meter"
        };
        let meter_toggle_item = MenuItem::with_id(ID_TOGGLE_METER, meter_toggle_label, true, None);

        let settings_item = MenuItem::with_id(ID_SETTINGS, "Settings…", true, None);
        let is_reg = cfg.is_registered();
        let open_url_item = MenuItem::with_id(ID_OPEN_VIEWER_URL, "Open Viewer URL", is_reg, None);
        let copy_url_item = MenuItem::with_id(ID_COPY_VIEWER_URL, "Copy Viewer URL", is_reg, None);
        let sep = PredefinedMenuItem::separator();
        let quit_item = MenuItem::with_id(ID_QUIT, "Quit", true, None);

        menu.append(&status_item).unwrap();
        menu.append(&sep_status).unwrap();
        menu.append(&toggle_item).unwrap();
        menu.append(&PredefinedMenuItem::separator()).unwrap();
        menu.append(&overlay_toggle_item).unwrap();
        menu.append(&meter_toggle_item).unwrap();
        menu.append(&PredefinedMenuItem::separator()).unwrap();
        menu.append(&settings_item).unwrap();
        menu.append(&PredefinedMenuItem::separator()).unwrap();
        menu.append(&open_url_item).unwrap();
        menu.append(&copy_url_item).unwrap();
        menu.append(&sep).unwrap();
        menu.append(&quit_item).unwrap();

        let menu_handle = menu.clone();
        let tray = TrayIconBuilder::new()
            .with_menu(Box::new(menu))
            // Left click pops the same native menu as right click — no
            // custom per-click handling (see the doc comment above the
            // (removed) TrayIconEvent listener in `run()`).
            .with_menu_on_left_click(true)
            // No log resolved yet — the engine hasn't started; the status timer
            // corrects this on its first tick.
            .with_tooltip(make_tooltip(cfg, None, logging_on))
            .with_icon(make_icon(cfg, logging_on, false))
            .build()
            .expect("tray icon");

        (
            tray,
            toggle_item,
            overlay_toggle_item,
            meter_toggle_item,
            status_item,
            copy_url_item,
            open_url_item,
            menu_handle,
        )
    }

    fn word_wrap(text: &str, max_chars: usize) -> Vec<String> {
        let mut lines: Vec<String> = Vec::new();
        let mut current = String::new();
        for word in text.split_whitespace() {
            if current.is_empty() {
                current = word.to_string();
            } else if current.chars().count() + 1 + word.chars().count() <= max_chars {
                current.push(' ');
                current.push_str(word);
            } else {
                lines.push(std::mem::take(&mut current));
                current = word.to_string();
            }
        }
        if !current.is_empty() {
            lines.push(current);
        }
        lines
    }

    fn make_status_text(
        logging_on: bool,
        connected: bool,
        cfg: &crate::config::Config,
        _last_err: Option<&str>,
        rate: u32,
    ) -> String {
        if !logging_on {
            return "Logging disabled".into();
        }
        if !cfg.local_ready() {
            return "Not configured (missing log file)".into();
        }
        if !cfg.remote_logging_enabled {
            return "Local only (remote logging off)".into();
        }
        if !cfg.is_ready() {
            if !cfg.is_registered() {
                return "Local only (not registered)".into();
            }
            return "Local only (missing server URL)".into();
        }
        if !connected {
            return "Reconnecting…".into();
        }
        let rate_str = if rate == 0 {
            "idle".into()
        } else {
            format!("{rate} ev/min")
        };
        format!("Connected ({rate_str})")
    }

    fn make_tooltip(cfg: &Config, active_log_path: Option<&str>, logging_on: bool) -> String {
        make_tooltip_full(cfg, active_log_path, logging_on, false, 0)
    }

    fn make_tooltip_full(
        cfg: &Config,
        active_log_path: Option<&str>,
        logging_on: bool,
        connected: bool,
        rate: u32,
    ) -> String {
        if !logging_on {
            return "froklog — logging disabled".into();
        }
        let log = active_log_path
            .and_then(|p| std::path::Path::new(p).file_name()?.to_str())
            .unwrap_or("no log");
        if !cfg.local_ready() {
            return "froklog — not configured".into();
        }
        if !cfg.remote_logging_enabled {
            return format!("froklog ● {log} — local only (remote off)");
        }
        if !cfg.is_registered() {
            return format!("froklog ○ {log} — not registered");
        }
        if !connected {
            return format!("froklog ○ {log} — reconnecting");
        }
        let activity = if rate == 0 {
            "idle".into()
        } else {
            format!("{rate} ev/min")
        };
        format!("froklog ● {log} — {activity}")
    }

    /// Which of the four status icons applies — a comparable key so callers
    /// can skip `set_icon` when nothing changed. That matters on Linux:
    /// every `set_icon` writes a NEW counter-named PNG, deletes the previous
    /// one, and re-points the StatusNotifierItem at the new path — so
    /// re-setting an unchanged icon every status tick gives the tray applet
    /// a moving target, and any read that loses the race lands on a deleted
    /// file and renders no icon at all (observed live on COSMIC: the item's
    /// IconName can wedge one generation behind the file on disk).
    pub(crate) fn icon_choice(cfg: &Config, logging_on: bool, connected: bool) -> u8 {
        if !logging_on {
            3 // red
        } else if !cfg.local_ready() {
            1 // gray
        } else if !cfg.remote_logging_enabled {
            // Remote push intentionally off — local engine is fully up, so this
            // isn't a warning state.
            0 // green
        } else if !cfg.is_registered() || !connected {
            // Wants remote but not registered / WS link down — reconnecting.
            2 // orange
        } else {
            0 // green
        }
    }

    pub(crate) fn make_icon(cfg: &Config, logging_on: bool, connected: bool) -> tray_icon::Icon {
        const GREEN: &[u8] = include_bytes!("../assets/froklog-green.png");
        const GRAY: &[u8] = include_bytes!("../assets/froklog-gray.png");
        const ORANGE: &[u8] = include_bytes!("../assets/froklog-orange.png");
        const RED: &[u8] = include_bytes!("../assets/froklog-red.png");

        let bytes = match icon_choice(cfg, logging_on, connected) {
            3 => RED,
            1 => GRAY,
            2 => ORANGE,
            _ => GREEN,
        };

        let img = image::load_from_memory(bytes)
            .expect("embedded icon PNG")
            .into_rgba8();
        let (w, h) = img.dimensions();
        tray_icon::Icon::from_rgba(img.into_raw(), w, h).expect("icon")
    }
}
