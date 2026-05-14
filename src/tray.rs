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

    const ID_PICK_LOG: &str = "pick_log";
    const ID_REGISTER: &str = "register";
    const ID_RESET_TOKEN: &str = "reset_token";
    const ID_COPY_VIEWER: &str = "copy_viewer";
    const ID_STATUS: &str = "status";
    const ID_QUIT: &str = "quit";

    /// Shared state between the tray event loop and the background engine.
    pub struct AppHandle {
        pub config: Arc<Mutex<Config>>,
        /// Set to true when the engine should restart (new log file / token).
        pub restart: Arc<AtomicBool>,
        /// Set to true to request a clean shutdown.
        pub quit: Arc<AtomicBool>,
    }

    impl AppHandle {
        pub fn new(config: Config) -> Self {
            Self {
                config: Arc::new(Mutex::new(config)),
                restart: Arc::new(AtomicBool::new(false)),
                quit: Arc::new(AtomicBool::new(false)),
            }
        }
    }

    /// Entry point — blocks until the user chooses Quit.
    pub fn run(handle: Arc<AppHandle>) {
        let event_loop: EventLoop<()> = EventLoop::builder().build().expect("event loop");

        let (tray, _menu) = build_tray(&handle.config.lock().unwrap());
        let tray = Arc::new(Mutex::new(tray));

        // On startup, if a log file is set but registration is missing, nudge the user.
        {
            let cfg = handle.config.lock().unwrap();
            if cfg.log_path.is_some() && !cfg.is_registered() {
                show_info(
                    "froklog — action required",
                    "A log file is set but this client is not registered with a server.\n\
                     Right-click the tray icon and choose \"Register with server…\".",
                );
            }
        }

        let handle_clone = Arc::clone(&handle);

        // winit doesn't expose a simple run-with-closure on older APIs; we use
        // the deprecated `run` which is still the right approach for tray-only apps.
        #[allow(deprecated)]
        event_loop.run(move |event, elwt| {
            elwt.set_control_flow(ControlFlow::Wait);

            // ── Tray icon clicks ──────────────────────────────────────────────
            if let Ok(_evt) = TrayIconEvent::receiver().try_recv() {
                // Left-click: show status balloon (no-op on platforms without it)
            }

            // ── Menu events ───────────────────────────────────────────────────
            if let Ok(evt) = MenuEvent::receiver().try_recv() {
                let id = evt.id.0.as_str();
                match id {
                    ID_PICK_LOG => {
                        if let Some(path) = pick_log_file() {
                            let mut cfg = handle_clone.config.lock().unwrap();
                            cfg.log_path = Some(path);
                            cfg.save();
                            update_tray(&tray.lock().unwrap(), &cfg);
                            handle_clone.restart.store(true, Ordering::Relaxed);
                        }
                    }
                    ID_REGISTER => {
                        // First-time registration.  Server URL is pre-filled from config
                        // if previously set — the user only needs to re-enter it if changed.
                        match do_register(&handle_clone.config, /*reset=*/false) {
                            Ok(()) => {
                                let cfg = handle_clone.config.lock().unwrap();
                                let url = cfg.viewer_url().unwrap_or_default();
                                show_info("froklog — registered",
                                    &format!("Stream registered!\n\nViewer URL (share this):\n{url}"));
                                update_tray(&tray.lock().unwrap(), &cfg);
                                handle_clone.restart.store(true, Ordering::Relaxed);
                            }
                            Err(e) => show_error("Registration failed", &e),
                        }
                    }
                    ID_RESET_TOKEN => {
                        // Re-registration: warn that the old viewer URL will stop working.
                        let confirmed = confirm_dialog(
                            "froklog — reset token",
                            "This will create a new stream and invalidate the current viewer URL.\n\
                             Anyone watching your stream will be disconnected.\n\n\
                             Continue?",
                        );
                        if confirmed {
                            match do_register(&handle_clone.config, /*reset=*/true) {
                                Ok(()) => {
                                    let cfg = handle_clone.config.lock().unwrap();
                                    let url = cfg.viewer_url().unwrap_or_default();
                                    show_info("froklog — re-registered",
                                        &format!("New stream created.\n\nNew viewer URL:\n{url}"));
                                    update_tray(&tray.lock().unwrap(), &cfg);
                                    handle_clone.restart.store(true, Ordering::Relaxed);
                                }
                                Err(e) => show_error("Re-registration failed", &e),
                            }
                        }
                    }
                    ID_COPY_VIEWER => {
                        let cfg = handle_clone.config.lock().unwrap();
                        match cfg.viewer_url() {
                            Some(url) => {
                                copy_to_clipboard(&url);
                                show_info("froklog — viewer URL copied", &url);
                            }
                            None => show_error("Not registered",
                                "Register with a server first (right-click → Register with server…)."),
                        }
                    }
                    ID_STATUS => {
                        let cfg = handle_clone.config.lock().unwrap();
                        let log    = cfg.log_path.as_deref().unwrap_or("(none)");
                        let server = cfg.server_url.as_deref().unwrap_or("(none)");
                        let status = if cfg.is_registered() {
                            format!("Registered — stream: {}", cfg.stream_id.as_deref().unwrap_or("?"))
                        } else {
                            "Not registered".into()
                        };
                        let viewer = cfg.viewer_url().unwrap_or_else(|| "(not registered)".into());
                        show_info("froklog status", &format!(
                            "Log file:   {log}\nServer:     {server}\nStatus:     {status}\nViewer URL: {viewer}"
                        ));
                    }
                    ID_QUIT => {
                        handle_clone.quit.store(true, Ordering::Relaxed);
                        elwt.exit();
                    }
                    _ => {}
                }
            }

            // Suppress unused variable warning on non-Windows builds where
            // winit's event type differs.
            let _ = event;
        }).expect("event loop");
    }

    // ── Tray construction ─────────────────────────────────────────────────────

    fn build_tray(cfg: &Config) -> (tray_icon::TrayIcon, Menu) {
        let menu = Menu::new();

        let status_item = MenuItem::with_id(ID_STATUS, "Status…", true, None);
        let pick_item = MenuItem::with_id(ID_PICK_LOG, "Pick log file…", true, None);
        let register_item = MenuItem::with_id(ID_REGISTER, "Register with server…", true, None);
        let reset_item =
            MenuItem::with_id(ID_RESET_TOKEN, "Re-register / Reset token…", true, None);
        let copy_item = MenuItem::with_id(ID_COPY_VIEWER, "Copy viewer URL", true, None);
        let sep = PredefinedMenuItem::separator();
        let quit_item = MenuItem::with_id(ID_QUIT, "Quit", true, None);

        menu.append(&status_item).unwrap();
        menu.append(&pick_item).unwrap();
        menu.append(&register_item).unwrap();
        menu.append(&reset_item).unwrap();
        menu.append(&copy_item).unwrap();
        menu.append(&sep).unwrap();
        menu.append(&quit_item).unwrap();

        let icon = make_icon(cfg);
        let tooltip = make_tooltip(cfg);

        let tray = TrayIconBuilder::new()
            .with_menu(Box::new(menu.clone()))
            .with_tooltip(&tooltip)
            .with_icon(icon)
            .build()
            .expect("tray icon");

        (tray, menu)
    }

    fn make_tooltip(cfg: &Config) -> String {
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

    /// Update both the icon and tooltip to reflect the current config state.
    fn update_tray(tray: &tray_icon::TrayIcon, cfg: &Config) {
        let _ = tray.set_icon(Some(make_icon(cfg)));
        let _ = tray.set_tooltip(Some(make_tooltip(cfg)));
    }

    // ── Icon ──────────────────────────────────────────────────────────────────

    /// Build a 16×16 RGBA circle icon whose colour reflects the current state:
    ///   green  = fully operational (log + registered)
    ///   amber  = log set but not registered
    ///   gray   = not configured
    fn make_icon(cfg: &Config) -> tray_icon::Icon {
        let (r, g, b): (u8, u8, u8) = if cfg.is_registered() {
            (60, 200, 80) // green
        } else if cfg.log_path.is_some() {
            (220, 160, 30) // amber
        } else {
            (110, 110, 120) // gray
        };

        const SIZE: usize = 16;
        let mut rgba = vec![0u8; SIZE * SIZE * 4];
        let cx = (SIZE / 2) as f32;
        let cy = (SIZE / 2) as f32;
        let radius = (SIZE / 2 - 1) as f32;
        for y in 0..SIZE {
            for x in 0..SIZE {
                let dx = x as f32 - cx;
                let dy = y as f32 - cy;
                if dx * dx + dy * dy <= radius * radius {
                    let idx = (y * SIZE + x) * 4;
                    rgba[idx] = r;
                    rgba[idx + 1] = g;
                    rgba[idx + 2] = b;
                    rgba[idx + 3] = 255;
                }
            }
        }
        tray_icon::Icon::from_rgba(rgba, SIZE as u32, SIZE as u32).expect("icon")
    }

    // ── Platform helpers ──────────────────────────────────────────────────────

    /// Open a Windows file-open dialog filtered to *.txt.
    fn pick_log_file() -> Option<String> {
        #[cfg(target_os = "windows")]
        {
            use std::ffi::OsStr;
            use std::os::windows::ffi::OsStrExt;
            use windows::core::PCWSTR;
            use windows::Win32::UI::Controls::Dialogs::{
                GetOpenFileNameW, OPENFILENAMEW, OFN_FILEMUSTEXIST, OFN_PATHMUSTEXIST,
            };

            // Build a wide-char filter string: "Log files\0*.txt\0\0"
            let filter: Vec<u16> = OsStr::new("Log files\0*.txt\0\0").encode_wide().collect();

            let mut buf = vec![0u16; 1024];

            let mut ofn = OPENFILENAMEW {
                lStructSize: std::mem::size_of::<OPENFILENAMEW>() as u32,
                lpstrFilter: PCWSTR(filter.as_ptr()),
                lpstrFile: windows::core::PWSTR(buf.as_mut_ptr()),
                nMaxFile: buf.len() as u32,
                Flags: OFN_FILEMUSTEXIST | OFN_PATHMUSTEXIST,
                ..Default::default()
            };

            let ok = unsafe { GetOpenFileNameW(&mut ofn) };
            if ok.as_bool() {
                let end = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
                Some(String::from_utf16_lossy(&buf[..end]))
            } else {
                None
            }
        }
        #[cfg(not(target_os = "windows"))]
        {
            // Fallback: read from stdin (useful for testing on Linux).
            eprintln!("Enter log file path:");
            let mut s = String::new();
            std::io::stdin().read_line(&mut s).ok();
            let s = s.trim().to_string();
            if s.is_empty() {
                None
            } else {
                Some(s)
            }
        }
    }

    // ── Registration ──────────────────────────────────────────────────────────

    /// Drive the full registration flow.
    ///
    /// - If `server_url` is already stored in config, only the admin token is
    ///   prompted (the URL is reused).  Otherwise both are requested.
    /// - When `reset = true` the existing stream_id/stream_token/view_token are
    ///   cleared before the POST so a brand-new stream is created.
    fn do_register(config: &Arc<Mutex<Config>>, reset: bool) -> Result<(), String> {
        // Determine whether we already have a saved server URL.
        let saved_url = config.lock().unwrap().server_url.clone();

        let server_url = if let Some(ref u) = saved_url {
            // Pre-filled; let the user confirm or change it.
            let entered = input_dialog(
                "froklog — server URL",
                &format!("Server URL (press Enter to keep current):\n{u}"),
            );
            match entered {
                Some(ref s) if !s.is_empty() => s.clone(),
                _ => u.clone(),
            }
        } else {
            input_dialog(
                "froklog — server URL",
                "Enter the froklog-server URL (e.g. http://myserver:8766):",
            )
            .ok_or_else(|| "Cancelled".to_string())?
        };

        if server_url.is_empty() {
            return Err("Server URL cannot be empty.".into());
        }

        // Admin token is never stored — always prompted.
        let admin_token = input_dialog(
            "froklog — admin token",
            "Enter the server admin token (FROKLOG_ADMIN_TOKEN):\n\
             This is only used to create/reset your stream and is never saved.",
        )
        .ok_or_else(|| "Cancelled".to_string())?;

        if admin_token.is_empty() {
            return Err("Admin token cannot be empty.".into());
        }

        // Determine player name from the currently configured log file.
        let player_name = {
            let cfg = config.lock().unwrap();
            cfg.log_path
                .as_deref()
                .and_then(|p| {
                    let stem = std::path::Path::new(p).file_stem()?.to_str()?;
                    let parts: Vec<&str> = stem.split('_').collect();
                    if parts.len() >= 3 {
                        Some(parts[1].to_string())
                    } else {
                        None
                    }
                })
                .unwrap_or_else(|| "Unknown".into())
        };

        // Clear old stream credentials if this is a reset.
        if reset {
            let mut cfg = config.lock().unwrap();
            cfg.stream_id = None;
            cfg.stream_token = None;
            cfg.view_token = None;
        }

        let url = format!("{server_url}/stream");
        let body = format!(r#"{{"player":"{player_name}"}}"#);
        let resp = blocking_post_json(&url, &admin_token, &body)?;

        let v: serde_json::Value = serde_json::from_str(&resp)
            .map_err(|e| format!("Invalid server response: {e}\nRaw: {resp}"))?;

        // Surface a clear error if the server returned an error body.
        if let Some(err) = v.get("error").and_then(|e| e.as_str()) {
            return Err(format!("Server error: {err}"));
        }

        let stream_id = v["stream_id"]
            .as_str()
            .ok_or("missing stream_id")?
            .to_string();
        let stream_token = v["stream_token"]
            .as_str()
            .ok_or("missing stream_token")?
            .to_string();
        let view_token = v["view_token"]
            .as_str()
            .ok_or("missing view_token")?
            .to_string();

        let mut cfg = config.lock().unwrap();
        cfg.server_url = Some(server_url);
        cfg.stream_id = Some(stream_id);
        cfg.stream_token = Some(stream_token);
        cfg.view_token = Some(view_token);
        cfg.save();

        Ok(())
    }

    /// Minimal blocking HTTP POST using std::net::TcpStream.
    /// Avoids adding a full HTTP client crate just for one call.
    fn blocking_post_json(url: &str, bearer: &str, body: &str) -> Result<String, String> {
        use std::io::{Read, Write};
        use std::net::TcpStream;

        // Parse URL manually (we only need host:port and path).
        let without_scheme = url
            .strip_prefix("https://")
            .or_else(|| url.strip_prefix("http://"))
            .ok_or_else(|| format!("URL must start with http:// or https://: {url}"))?;
        let (hostport, path) = without_scheme
            .split_once('/')
            .map(|(h, p)| (h, format!("/{p}")))
            .unwrap_or((without_scheme, "/".to_string()));

        let default_port = if url.starts_with("https") {
            "443"
        } else {
            "80"
        };
        let addr = if hostport.contains(':') {
            hostport.to_string()
        } else {
            format!("{hostport}:{default_port}")
        };

        let host = hostport.split(':').next().unwrap_or(hostport);

        let request = format!(
            "POST {path} HTTP/1.0\r\n\
             Host: {host}\r\n\
             Authorization: Bearer {bearer}\r\n\
             Content-Type: application/json\r\n\
             Content-Length: {len}\r\n\
             Connection: close\r\n\
             \r\n\
             {body}",
            len = body.len()
        );

        let mut stream =
            TcpStream::connect(&addr).map_err(|e| format!("Cannot connect to {addr}: {e}"))?;
        stream
            .write_all(request.as_bytes())
            .map_err(|e| format!("Write error: {e}"))?;

        let mut raw = String::new();
        stream
            .read_to_string(&mut raw)
            .map_err(|e| format!("Read error: {e}"))?;

        // Split off the HTTP headers from the body.
        let body = raw
            .split("\r\n\r\n")
            .nth(1)
            .ok_or_else(|| "Malformed HTTP response".to_string())?;

        Ok(body.to_string())
    }

    /// Show a simple Windows input dialog using a message-box workaround.
    /// On Windows we use TaskDialog for nicer UX; on other platforms stdin.
    fn input_dialog(title: &str, prompt: &str) -> Option<String> {
        #[cfg(target_os = "windows")]
        {
            // InputBox isn't native Win32 without a full dialog resource.
            // We use a console-based fallback via a hidden console window.
            // For production you'd embed a proper Win32 dialog; this gets the job done.
            show_info(title, prompt);
            let mut s = String::new();
            // Temporarily re-enable the console for input.
            use std::io::BufRead;
            let stdin = std::io::stdin();
            stdin
                .lock()
                .lines()
                .next()
                .and_then(|l| l.ok())
                .filter(|s| !s.is_empty())
        }
        #[cfg(not(target_os = "windows"))]
        {
            eprintln!("{title}: {prompt}");
            let mut s = String::new();
            std::io::stdin().read_line(&mut s).ok();
            let s = s.trim().to_string();
            if s.is_empty() {
                None
            } else {
                Some(s)
            }
        }
    }

    /// Show a Yes/No confirmation dialog.  Returns `true` if the user chose Yes.
    fn confirm_dialog(title: &str, msg: &str) -> bool {
        #[cfg(target_os = "windows")]
        {
            use windows::core::PCSTR;
            use windows::Win32::UI::WindowsAndMessaging::{
                MessageBoxA, HWND_DESKTOP, IDYES, MB_ICONWARNING, MB_YESNO,
            };
            let title_c = std::ffi::CString::new(title).unwrap_or_default();
            let msg_c = std::ffi::CString::new(msg).unwrap_or_default();
            let result = unsafe {
                MessageBoxA(
                    HWND_DESKTOP,
                    PCSTR(msg_c.as_ptr() as *const u8),
                    PCSTR(title_c.as_ptr() as *const u8),
                    MB_YESNO | MB_ICONWARNING,
                )
            };
            result == IDYES
        }
        #[cfg(not(target_os = "windows"))]
        {
            eprintln!("{title}: {msg}");
            eprintln!("Confirm? [y/N]");
            let mut s = String::new();
            std::io::stdin().read_line(&mut s).ok();
            matches!(s.trim().to_lowercase().as_str(), "y" | "yes")
        }
    }

    fn show_info(title: &str, msg: &str) {
        #[cfg(target_os = "windows")]
        {
            use windows::core::PCSTR;
            use windows::Win32::UI::WindowsAndMessaging::{
                MessageBoxA, HWND_DESKTOP, MB_ICONINFORMATION, MB_OK,
            };
            let title_c = std::ffi::CString::new(title).unwrap_or_default();
            let msg_c = std::ffi::CString::new(msg).unwrap_or_default();
            unsafe {
                MessageBoxA(
                    HWND_DESKTOP,
                    PCSTR(msg_c.as_ptr() as *const u8),
                    PCSTR(title_c.as_ptr() as *const u8),
                    MB_OK | MB_ICONINFORMATION,
                );
            }
        }
        #[cfg(not(target_os = "windows"))]
        {
            eprintln!("[{title}] {msg}");
        }
    }

    fn show_error(title: &str, msg: &str) {
        #[cfg(target_os = "windows")]
        {
            use windows::core::PCSTR;
            use windows::Win32::UI::WindowsAndMessaging::{
                MessageBoxA, HWND_DESKTOP, MB_ICONERROR, MB_OK,
            };
            let title_c = std::ffi::CString::new(title).unwrap_or_default();
            let msg_c = std::ffi::CString::new(msg).unwrap_or_default();
            unsafe {
                MessageBoxA(
                    HWND_DESKTOP,
                    PCSTR(msg_c.as_ptr() as *const u8),
                    PCSTR(title_c.as_ptr() as *const u8),
                    MB_OK | MB_ICONERROR,
                );
            }
        }
        #[cfg(not(target_os = "windows"))]
        {
            eprintln!("[ERROR][{title}] {msg}");
        }
    }

    fn copy_to_clipboard(text: &str) {
        #[cfg(target_os = "windows")]
        {
            // Write to clipboard via OpenClipboard / SetClipboardData.
            use windows::Win32::System::DataExchange::{
                CloseClipboard, EmptyClipboard, OpenClipboard, SetClipboardData,
            };
            use windows::Win32::System::Memory::{
                GlobalAlloc, GlobalLock, GlobalUnlock, GMEM_MOVEABLE,
            };
            use windows::Win32::System::Ole::CF_TEXT;
            use windows::Win32::UI::WindowsAndMessaging::HWND_DESKTOP;

            unsafe {
                if OpenClipboard(HWND_DESKTOP).is_ok() {
                    let _ = EmptyClipboard();
                    let bytes = text.as_bytes();
                    let hglob = GlobalAlloc(GMEM_MOVEABLE, bytes.len() + 1);
                    if let Ok(hglob) = hglob {
                        let ptr = GlobalLock(hglob) as *mut u8;
                        if !ptr.is_null() {
                            std::ptr::copy_nonoverlapping(bytes.as_ptr(), ptr, bytes.len());
                            *ptr.add(bytes.len()) = 0;
                            GlobalUnlock(hglob);
                        }
                        let _ = SetClipboardData(
                            CF_TEXT.0 as u32,
                            windows::Win32::Foundation::HANDLE(hglob.0),
                        );
                    }
                    let _ = CloseClipboard();
                }
            }
        }
        #[cfg(not(target_os = "windows"))]
        {
            // On Linux, try xclip/xsel as a best-effort.
            let _ = std::process::Command::new("xclip")
                .args(["-selection", "clipboard"])
                .stdin(std::process::Stdio::piped())
                .spawn()
                .and_then(|mut c| {
                    use std::io::Write;
                    c.stdin.as_mut().unwrap().write_all(text.as_bytes())?;
                    c.wait()
                });
        }
    }
}
