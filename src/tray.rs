/// Windows systray integration.
///
/// Runs on the main thread (required by winit/tray-icon on Windows).
/// All heavy work (tailer, parser, pusher) happens on background threads/tasks
/// co-ordinated through the `AppHandle` passed in from main.
#[cfg(feature = "tray")]
#[allow(clippy::module_inception)]
pub mod tray {
    use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
    use std::sync::RwLock;
    use std::sync::{Arc, Mutex};
    use std::time::{Duration, Instant};

    use tray_icon::{
        menu::{Menu, MenuEvent, MenuItem, PredefinedMenuItem},
        TrayIconBuilder, TrayIconEvent,
    };
    use winit::event_loop::{ControlFlow, EventLoop};

    use crate::config::Config;
    use crate::triggers::engine::TriggerEngine;

    // ── Menu item IDs ─────────────────────────────────────────────────────────

    const ID_STATUS: &str = "status";
    const ID_TOGGLE_LOGGING: &str = "toggle_logging";
    const ID_TOGGLE_OVERLAY: &str = "toggle_overlay";
    const ID_OVERLAY_SETTINGS: &str = "overlay_settings";
    const ID_SETTINGS: &str = "settings";
    const ID_COPY_VIEWER_URL: &str = "copy_viewer_url";
    const ID_OPEN_VIEWER_URL: &str = "open_viewer_url";
    const ID_QUIT: &str = "quit";

    const TICK_SECS: u64 = 5;

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
        /// Prevents opening multiple settings windows simultaneously.
        pub settings_open: Arc<AtomicBool>,
        /// Prevents opening multiple overlay-config windows simultaneously.
        pub overlay_config_open: Arc<AtomicBool>,
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
                overlay_config_open: Arc::new(AtomicBool::new(false)),
                events_sent: Arc::new(AtomicU64::new(0)),
                connected: Arc::new(AtomicBool::new(false)),
                last_connect_error: Arc::new(RwLock::new(None)),
                trigger_engine: Arc::new(Mutex::new(None)),
                overlay_queue: Arc::new(Mutex::new(Vec::new())),
            }
        }
    }

    // ── Entry point ───────────────────────────────────────────────────────────

    /// Entry point — blocks until the user chooses Quit.
    pub fn run(handle: Arc<AppHandle>) {
        let event_loop: EventLoop<()> = EventLoop::builder().build().expect("event loop");

        let logging_on = handle.logging_enabled.load(Ordering::Relaxed);
        let overlay_on = handle.config.lock().unwrap().overlay_enabled;
        let (tray, toggle_item, overlay_toggle_item, status_item, copy_url_item, open_url_item) =
            build_tray(&handle.config.lock().unwrap(), logging_on, overlay_on);
        #[allow(clippy::arc_with_non_send_sync)]
        let tray = Arc::new(Mutex::new(tray));
        #[allow(clippy::arc_with_non_send_sync)]
        let toggle_item = Arc::new(Mutex::new(toggle_item));
        #[allow(clippy::arc_with_non_send_sync)]
        let overlay_toggle_item = Arc::new(Mutex::new(overlay_toggle_item));

        let handle_clone = Arc::clone(&handle);

        // Rate-tracking and connection state.
        let mut prev_count: u64 = 0;
        let mut prev_sample = Instant::now();
        let mut prev_connected: bool = false;
        let mut next_tick = Instant::now() + Duration::from_secs(TICK_SECS);
        let mut next_fast_check = Instant::now() + Duration::from_secs(1);

        #[allow(deprecated)]
        event_loop
            .run(move |event, elwt| {
                // Wake at 1-second boundary for fast connection-state checks.
                elwt.set_control_flow(ControlFlow::WaitUntil(next_fast_check));

                // ── Left-click: open Settings ─────────────────────────────────
                // Match only Left+Up to avoid double-firing. Opening Settings is
                // safer than toggling logging, which stops capture and is easy to
                // trigger accidentally.
                if let Ok(evt) = TrayIconEvent::receiver().try_recv() {
                    if matches!(
                        evt,
                        tray_icon::TrayIconEvent::Click {
                            button: tray_icon::MouseButton::Left,
                            button_state: tray_icon::MouseButtonState::Up,
                            ..
                        }
                    ) {
                        if !handle_clone.settings_open.swap(true, Ordering::Relaxed) {
                            crate::settings_win::open_settings(Arc::clone(&handle_clone));
                        }
                    }
                }

                // ── Menu events ───────────────────────────────────────────────
                if let Ok(evt) = MenuEvent::receiver().try_recv() {
                    match evt.id.0.as_str() {
                        ID_TOGGLE_LOGGING => {
                            toggle_logging(&handle_clone, &tray, &toggle_item);
                        }
                        ID_TOGGLE_OVERLAY => {
                            toggle_overlay(&handle_clone, &overlay_toggle_item);
                        }
                        ID_OVERLAY_SETTINGS
                            if !handle_clone
                                .overlay_config_open
                                .swap(true, Ordering::Relaxed) =>
                        {
                            crate::overlay_config_win::open_overlay_config(Arc::clone(
                                &handle_clone,
                            ));
                        }
                        ID_SETTINGS
                            if !handle_clone.settings_open.swap(true, Ordering::Relaxed) =>
                        {
                            crate::settings_win::open_settings(Arc::clone(&handle_clone));
                        }
                        ID_OPEN_VIEWER_URL => {
                            let url = handle_clone.config.lock().unwrap().stream_url();
                            if let Some(url) = url {
                                open_in_browser(&url);
                            }
                        }
                        ID_COPY_VIEWER_URL => {
                            let url = handle_clone.config.lock().unwrap().stream_url();
                            if let Some(url) = url {
                                copy_to_clipboard(&url);
                            }
                        }
                        ID_QUIT => {
                            handle_clone.quit.store(true, Ordering::Relaxed);
                            elwt.exit();
                        }
                        _ => {}
                    }
                }

                // ── Connection fast-check (1 s) ───────────────────────────────
                let now = Instant::now();
                if now >= next_fast_check {
                    next_fast_check = now + Duration::from_secs(1);
                    let is_connected = handle_clone.connected.load(Ordering::Relaxed);
                    if is_connected != prev_connected {
                        prev_connected = is_connected;
                        let logging_on = handle_clone.logging_enabled.load(Ordering::Relaxed);
                        let cfg = handle_clone.config.lock().unwrap();
                        let _ = tray.lock().unwrap().set_icon(Some(make_icon(
                            &cfg,
                            logging_on,
                            is_connected,
                        )));
                    }
                }

                // ── Periodic status tick (5 s) ────────────────────────────────
                if now >= next_tick {
                    let cur = handle_clone.events_sent.load(Ordering::Relaxed);
                    let elapsed = now.duration_since(prev_sample).as_secs_f64().max(0.001);
                    let rate = ((cur.saturating_sub(prev_count)) as f64 / elapsed * 60.0) as u32;
                    prev_count = cur;
                    prev_sample = now;
                    next_tick = now + Duration::from_secs(TICK_SECS);

                    let logging_on = handle_clone.logging_enabled.load(Ordering::Relaxed);
                    let is_connected = handle_clone.connected.load(Ordering::Relaxed);
                    prev_connected = is_connected;
                    let cfg = handle_clone.config.lock().unwrap();
                    let last_err = handle_clone
                        .last_connect_error
                        .read()
                        .ok()
                        .and_then(|g| g.clone());

                    let _ = tray.lock().unwrap().set_tooltip(Some(make_tooltip_full(
                        &cfg,
                        logging_on,
                        is_connected,
                        rate,
                    )));
                    let _ = tray.lock().unwrap().set_icon(Some(make_icon(
                        &cfg,
                        logging_on,
                        is_connected,
                    )));
                    status_item.set_text(make_status_text(
                        logging_on,
                        is_connected,
                        &cfg,
                        last_err.as_deref(),
                        rate,
                    ));
                    let is_reg = cfg.is_registered();
                    copy_url_item.set_enabled(is_reg);
                    open_url_item.set_enabled(is_reg);
                }

                let _ = event;
            })
            .expect("event loop");
    }

    // ── Overlay toggle ────────────────────────────────────────────────────────

    fn toggle_overlay(handle: &Arc<AppHandle>, overlay_toggle_item: &Arc<Mutex<MenuItem>>) {
        let now_on = {
            let mut cfg = handle.config.lock().unwrap();
            cfg.overlay_enabled = !cfg.overlay_enabled;
            let v = cfg.overlay_enabled;
            cfg.save();
            v
        };
        let label = if now_on {
            "Hide Overlay"
        } else {
            "Show Overlay"
        };
        overlay_toggle_item.lock().unwrap().set_text(label);
        // A restart picks up the new overlay_enabled flag.
        handle.restart.store(true, Ordering::Relaxed);
    }

    // ── Logging toggle ────────────────────────────────────────────────────────

    fn toggle_logging(
        handle: &Arc<AppHandle>,
        tray: &Arc<Mutex<tray_icon::TrayIcon>>,
        toggle_item: &Arc<Mutex<MenuItem>>,
    ) {
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
        let tray_guard = tray.lock().unwrap();
        let _ = tray_guard.set_icon(Some(make_icon(
            &handle.config.lock().unwrap(),
            now_on,
            is_connected,
        )));
        let _ = tray_guard.set_tooltip(Some(make_tooltip(&handle.config.lock().unwrap(), now_on)));

        let label = if now_on {
            "Disable Logging"
        } else {
            "Enable Logging"
        };
        toggle_item.lock().unwrap().set_text(label);
    }

    // ── Clipboard ─────────────────────────────────────────────────────────────

    #[cfg(target_os = "windows")]
    fn copy_to_clipboard(text: &str) {
        use windows::Win32::Foundation::{HANDLE, HWND};
        use windows::Win32::System::DataExchange::{
            CloseClipboard, EmptyClipboard, OpenClipboard, SetClipboardData,
        };
        use windows::Win32::System::Memory::{
            GlobalAlloc, GlobalLock, GlobalUnlock, GMEM_MOVEABLE,
        };

        const CF_UNICODETEXT: u32 = 13;

        let wide: Vec<u16> = text.encode_utf16().chain(std::iter::once(0u16)).collect();
        let byte_count = wide.len() * 2;

        unsafe {
            let Ok(hglob) = GlobalAlloc(GMEM_MOVEABLE, byte_count) else {
                return;
            };
            let ptr = GlobalLock(hglob) as *mut u16;
            if ptr.is_null() {
                return;
            }
            std::ptr::copy_nonoverlapping(wide.as_ptr(), ptr, wide.len());
            let _ = GlobalUnlock(hglob);

            if OpenClipboard(HWND::default()).is_err() {
                return;
            }
            let _ = EmptyClipboard();
            // Ownership of hglob transfers to the clipboard; do not free it.
            let _ = SetClipboardData(CF_UNICODETEXT, HANDLE(hglob.0));
            let _ = CloseClipboard();
        }
    }

    #[cfg(not(target_os = "windows"))]
    fn copy_to_clipboard(_text: &str) {}

    // ── Browser launch ────────────────────────────────────────────────────────

    #[cfg(target_os = "windows")]
    fn open_in_browser(url: &str) {
        use windows::Win32::UI::Shell::ShellExecuteW;
        use windows::Win32::UI::WindowsAndMessaging::SW_SHOWNORMAL;
        let url_w: Vec<u16> = url.encode_utf16().chain(std::iter::once(0)).collect();
        let op_w: Vec<u16> = "open".encode_utf16().chain(std::iter::once(0)).collect();
        unsafe {
            ShellExecuteW(
                None,
                windows::core::PCWSTR(op_w.as_ptr()),
                windows::core::PCWSTR(url_w.as_ptr()),
                windows::core::PCWSTR::null(),
                windows::core::PCWSTR::null(),
                SW_SHOWNORMAL,
            );
        }
    }

    #[cfg(not(target_os = "windows"))]
    fn open_in_browser(_url: &str) {}

    // ── Tray construction ─────────────────────────────────────────────────────

    fn build_tray(
        cfg: &Config,
        logging_on: bool,
        overlay_on: bool,
    ) -> (
        tray_icon::TrayIcon,
        MenuItem,
        MenuItem,
        MenuItem,
        MenuItem,
        MenuItem,
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
        let overlay_settings_item =
            MenuItem::with_id(ID_OVERLAY_SETTINGS, "Overlay Settings…", true, None);

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
        menu.append(&overlay_settings_item).unwrap();
        menu.append(&PredefinedMenuItem::separator()).unwrap();
        menu.append(&settings_item).unwrap();
        menu.append(&open_url_item).unwrap();
        menu.append(&copy_url_item).unwrap();
        menu.append(&sep).unwrap();
        menu.append(&quit_item).unwrap();

        let tray = TrayIconBuilder::new()
            .with_menu(Box::new(menu))
            .with_tooltip(make_tooltip(cfg, logging_on))
            .with_icon(make_icon(cfg, logging_on, false))
            .build()
            .expect("tray icon");

        (
            tray,
            toggle_item,
            overlay_toggle_item,
            status_item,
            copy_url_item,
            open_url_item,
        )
    }

    fn make_status_text(
        logging_on: bool,
        connected: bool,
        cfg: &crate::config::Config,
        last_err: Option<&str>,
        rate: u32,
    ) -> String {
        if !logging_on {
            return "Logging disabled".into();
        }
        if !cfg.is_ready() {
            if !cfg.is_registered() {
                return "Not registered".into();
            }
            return "Not configured (missing log file or server URL)".into();
        }
        if !connected {
            return match last_err {
                Some(e) => format!("Reconnecting… ({e})"),
                None => "Reconnecting…".into(),
            };
        }
        let rate_str = if rate == 0 {
            "idle".into()
        } else {
            format!("{rate} ev/min")
        };
        format!("Connected ({rate_str})")
    }

    fn make_tooltip(cfg: &Config, logging_on: bool) -> String {
        make_tooltip_full(cfg, logging_on, false, 0)
    }

    fn make_tooltip_full(cfg: &Config, logging_on: bool, connected: bool, rate: u32) -> String {
        if !logging_on {
            return "froklog — logging disabled".into();
        }
        let log = cfg
            .log_path
            .as_deref()
            .and_then(|p| std::path::Path::new(p).file_name()?.to_str())
            .unwrap_or("no log");
        if !cfg.is_registered() {
            return if cfg.log_path.is_some() {
                format!("froklog ○ {log} — not registered")
            } else {
                "froklog — not configured".into()
            };
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

    fn make_icon(cfg: &Config, logging_on: bool, connected: bool) -> tray_icon::Icon {
        const GREEN: &[u8] = include_bytes!("../assets/froklog-green.png");
        const GRAY: &[u8] = include_bytes!("../assets/froklog-gray.png");
        const ORANGE: &[u8] = include_bytes!("../assets/froklog-orange.png");
        const RED: &[u8] = include_bytes!("../assets/froklog-red.png");

        let bytes = if !logging_on {
            RED
        } else if !cfg.is_registered() {
            // Partially configured: orange if log chosen, gray if nothing set.
            if cfg.log_path.is_some() {
                ORANGE
            } else {
                GRAY
            }
        } else if !connected {
            // Registered but WS link is down — show orange to signal reconnecting.
            ORANGE
        } else {
            GREEN
        };

        let img = image::load_from_memory(bytes)
            .expect("embedded icon PNG")
            .into_rgba8();
        let (w, h) = img.dimensions();
        tray_icon::Icon::from_rgba(img.into_raw(), w, h).expect("icon")
    }
}
