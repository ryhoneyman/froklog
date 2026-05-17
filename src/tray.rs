/// Windows systray integration.
///
/// Runs on the main thread (required by winit/tray-icon on Windows).
/// All heavy work (tailer, parser, pusher) happens on background threads/tasks
/// co-ordinated through the `AppHandle` passed in from main.
#[cfg(feature = "tray")]
pub mod tray {
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::{Arc, Mutex};

    use tray_icon::{
        menu::{Menu, MenuEvent, MenuItem, PredefinedMenuItem},
        TrayIconBuilder, TrayIconEvent,
    };
    use winit::event_loop::{ControlFlow, EventLoop};

    use crate::config::Config;

    // ── Menu item IDs ─────────────────────────────────────────────────────────

    const ID_TOGGLE_LOGGING: &str = "toggle_logging";
    const ID_SETTINGS: &str = "settings";
    const ID_QUIT: &str = "quit";

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
            }
        }
    }

    // ── Entry point ───────────────────────────────────────────────────────────

    /// Entry point — blocks until the user chooses Quit.
    pub fn run(handle: Arc<AppHandle>) {
        let event_loop: EventLoop<()> = EventLoop::builder().build().expect("event loop");

        let logging_on = handle.logging_enabled.load(Ordering::Relaxed);
        let (tray, toggle_item) = build_tray(&handle.config.lock().unwrap(), logging_on);
        let tray = Arc::new(Mutex::new(tray));
        let toggle_item = Arc::new(Mutex::new(toggle_item));

        let handle_clone = Arc::clone(&handle);

        #[allow(deprecated)]
        event_loop
            .run(move |event, elwt| {
                elwt.set_control_flow(ControlFlow::Wait);

                // ── Left-click: toggle logging ────────────────────────────────
                if let Ok(evt) = TrayIconEvent::receiver().try_recv() {
                    if matches!(evt, tray_icon::TrayIconEvent::Click { .. }) {
                        toggle_logging(&handle_clone, &tray, &toggle_item);
                    }
                }

                // ── Menu events ───────────────────────────────────────────────
                if let Ok(evt) = MenuEvent::receiver().try_recv() {
                    match evt.id.0.as_str() {
                        ID_TOGGLE_LOGGING => {
                            toggle_logging(&handle_clone, &tray, &toggle_item);
                        }
                        ID_SETTINGS => {
                            if !handle_clone.settings_open.swap(true, Ordering::Relaxed) {
                                crate::settings_win::open_settings(Arc::clone(&handle_clone));
                            }
                        }
                        ID_QUIT => {
                            handle_clone.quit.store(true, Ordering::Relaxed);
                            elwt.exit();
                        }
                        _ => {}
                    }
                }

                let _ = event;
            })
            .expect("event loop");
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

        let tray_guard = tray.lock().unwrap();
        let _ = tray_guard.set_icon(Some(make_icon(&handle.config.lock().unwrap(), now_on)));
        let _ = tray_guard.set_tooltip(Some(make_tooltip(&handle.config.lock().unwrap(), now_on)));

        let label = if now_on {
            "Disable Logging"
        } else {
            "Enable Logging"
        };
        let _ = toggle_item.lock().unwrap().set_text(label);
    }

    // ── Tray construction ─────────────────────────────────────────────────────

    fn build_tray(cfg: &Config, logging_on: bool) -> (tray_icon::TrayIcon, MenuItem) {
        let menu = Menu::new();

        let toggle_label = if logging_on {
            "Disable Logging"
        } else {
            "Enable Logging"
        };
        let toggle_item = MenuItem::with_id(ID_TOGGLE_LOGGING, toggle_label, true, None);
        let settings_item = MenuItem::with_id(ID_SETTINGS, "Settings…", true, None);
        let sep = PredefinedMenuItem::separator();
        let quit_item = MenuItem::with_id(ID_QUIT, "Quit", true, None);

        menu.append(&toggle_item).unwrap();
        menu.append(&settings_item).unwrap();
        menu.append(&sep).unwrap();
        menu.append(&quit_item).unwrap();

        let tray = TrayIconBuilder::new()
            .with_menu(Box::new(menu))
            .with_tooltip(make_tooltip(cfg, logging_on))
            .with_icon(make_icon(cfg, logging_on))
            .build()
            .expect("tray icon");

        (tray, toggle_item)
    }

    fn make_tooltip(cfg: &Config, logging_on: bool) -> String {
        if !logging_on {
            return "froklog — logging disabled".into();
        }
        let log = cfg
            .log_path
            .as_deref()
            .and_then(|p| std::path::Path::new(p).file_name()?.to_str())
            .unwrap_or("no log");
        if cfg.is_registered() {
            format!("froklog ● {log}")
        } else if cfg.log_path.is_some() {
            format!("froklog ○ {log} — not registered")
        } else {
            "froklog — not configured".into()
        }
    }

    fn make_icon(cfg: &Config, logging_on: bool) -> tray_icon::Icon {
        const GREEN: &[u8] = include_bytes!("../assets/froklog-green.png");
        const GRAY: &[u8] = include_bytes!("../assets/froklog-gray.png");
        const ORANGE: &[u8] = include_bytes!("../assets/froklog-orange.png");
        const RED: &[u8] = include_bytes!("../assets/froklog-red.png");

        let bytes = if !logging_on {
            RED
        } else if cfg.is_registered() {
            GREEN
        } else if cfg.log_path.is_some() {
            ORANGE
        } else {
            GRAY
        };

        let img = image::load_from_memory(bytes)
            .expect("embedded icon PNG")
            .into_rgba8();
        let (w, h) = img.dimensions();
        tray_icon::Icon::from_rgba(img.into_raw(), w, h).expect("icon")
    }
}
