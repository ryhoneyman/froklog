/// Always-on-top transparent overlay window.
///
/// Rendered as a layered Win32 popup (`WS_EX_TOPMOST | WS_EX_LAYERED`).
/// Entries arrive via `WM_APP_EVENT` messages posted from the trigger engine
/// thread.  A 100 ms `WM_TIMER` drives the animation / idle-hide logic.
///
/// Layout (top → bottom):
///   older entries — small text, fading opacity
///   newest entry  — 2× font size, full opacity
///
/// The window is draggable via left-button-down anywhere on it.
#[cfg(feature = "tray")]
#[allow(clippy::module_inception)]
pub mod overlay {
    use std::ffi::c_void;
    use std::sync::{Arc, Mutex};
    use std::time::{Duration, Instant};

    use crate::config::Config;
    use crate::triggers::engine::OverlayEvent;

    // ── Win32 imports ─────────────────────────────────────────────────────────

    #[cfg(target_os = "windows")]
    use windows::Win32::Foundation::{HWND, LPARAM, LRESULT, WPARAM};

    #[cfg(target_os = "windows")]
    use windows::Win32::Graphics::Gdi::{
        BeginPaint, BitBlt, CreateCompatibleBitmap, CreateCompatibleDC, CreateFontW,
        CreateSolidBrush, DeleteDC, DeleteObject, EndPaint, FillRect, GetDC, GetStockObject,
        ReleaseDC, SelectObject, SetBkMode, SetTextColor, TextOutW, BLACK_BRUSH,
        CLIP_DEFAULT_PRECIS, DEFAULT_CHARSET, DEFAULT_PITCH, DEFAULT_QUALITY, FF_DONTCARE, FW_BOLD,
        FW_NORMAL, HBITMAP, HBRUSH, HDC, HFONT, HGDIOBJ, OUT_DEFAULT_PRECIS, PAINTSTRUCT, SRCCOPY,
        TRANSPARENT,
    };

    #[cfg(target_os = "windows")]
    use windows::Win32::System::LibraryLoader::GetModuleHandleW;

    #[cfg(target_os = "windows")]
    use windows::Win32::UI::WindowsAndMessaging::{
        CreateWindowExW, DefWindowProcW, DestroyWindow, DispatchMessageW, GetMessageW,
        GetSystemMetrics, GetWindowLongPtrW, GetWindowRect, LoadCursorW, PostQuitMessage,
        RegisterClassExW, SetLayeredWindowAttributes, SetWindowLongPtrW, SetWindowPos,
        TranslateMessage, CREATESTRUCTW, CS_HREDRAW, CS_VREDRAW, GWLP_USERDATA, HMENU, IDC_ARROW,
        LWA_ALPHA, MSG, SM_CXSCREEN, SM_CYSCREEN, SWP_NOMOVE, SWP_NOOWNERZORDER, SWP_NOZORDER,
        WINDOW_EX_STYLE, WINDOW_STYLE, WM_APP, WM_CREATE, WM_DESTROY, WM_LBUTTONDOWN,
        WM_NCLBUTTONDOWN, WM_PAINT, WM_TIMER, WNDCLASSEXW, WS_EX_LAYERED, WS_EX_NOACTIVATE,
        WS_EX_TOOLWINDOW, WS_EX_TOPMOST, WS_POPUP, WS_VISIBLE,
    };

    #[cfg(target_os = "windows")]
    use windows::core::PCWSTR;

    // ── Constants ─────────────────────────────────────────────────────────────

    const WM_APP_EVENT: u32 = WM_APP + 10;
    const TIMER_ANIM: usize = 1;
    const ANIM_INTERVAL_MS: u32 = 100;

    const PAD_X: i32 = 8;
    const PAD_Y: i32 = 6;
    const LINE_GAP: i32 = 4;
    const ICON_TEXT_GAP: i32 = 6;

    const WINDOW_WIDTH: i32 = 480;

    // Colour constants (COLORREF = 0x00BBGGRR).
    const COL_TEXT: u32 = 0x00FFFFFF; // white
    const COL_FEATURED: u32 = 0x0022FFFF; // bright yellow
    const COL_BG: u32 = 0x00101010; // near-black background

    const CLASS_NAME: &str = "FroklogOverlay\0";

    // ── Entry ─────────────────────────────────────────────────────────────────

    struct OverlayEntry {
        icon: String,
        message: String,
        sound: Option<String>,
        arrived: Instant,
    }

    // ── State ─────────────────────────────────────────────────────────────────

    struct OverlayState {
        queue: Arc<Mutex<Vec<OverlayEvent>>>,
        entries: Vec<OverlayEntry>,
        max_entries: usize,
        idle_secs: u32,
        font_size: i32,
        font_name: String,
        alpha: u8,
        /// Cached HFONT for normal entries (freed on drop / reload).
        hfont_normal: Option<HFONT>,
        hfont_featured: Option<HFONT>,
        window_w: i32,
    }

    #[cfg(target_os = "windows")]
    impl OverlayState {
        fn new(cfg: &Config, queue: Arc<Mutex<Vec<OverlayEvent>>>) -> Self {
            Self {
                queue,
                entries: Vec::new(),
                max_entries: cfg.overlay_max_entries.max(1),
                idle_secs: cfg.overlay_idle_secs,
                font_size: cfg.overlay_font_size.max(8) as i32,
                font_name: if cfg.overlay_font.is_empty() {
                    "Segoe UI".to_string()
                } else {
                    cfg.overlay_font.clone()
                },
                alpha: cfg.overlay_alpha,
                hfont_normal: None,
                hfont_featured: None,
                window_w: WINDOW_WIDTH,
            }
        }

        unsafe fn ensure_fonts(&mut self) {
            if self.hfont_normal.is_none() {
                self.hfont_normal = Some(make_font(&self.font_name, self.font_size, false));
            }
            if self.hfont_featured.is_none() {
                self.hfont_featured = Some(make_font(&self.font_name, self.font_size * 2, true));
            }
        }

        unsafe fn drop_fonts(&mut self) {
            if let Some(f) = self.hfont_normal.take() {
                let _ = DeleteObject(HGDIOBJ(f.0));
            }
            if let Some(f) = self.hfont_featured.take() {
                let _ = DeleteObject(HGDIOBJ(f.0));
            }
        }

        fn drain_queue(&mut self) {
            let new_events: Vec<OverlayEvent> = {
                let mut q = self.queue.lock().unwrap();
                q.drain(..).collect()
            };
            for ev in new_events {
                if let Some(sound) = &ev.sound {
                    if !sound.is_empty() {
                        play_sound(sound);
                    }
                }
                self.entries.push(OverlayEntry {
                    icon: ev.icon,
                    message: ev.message,
                    sound: ev.sound,
                    arrived: Instant::now(),
                });
                // Trim oldest if over capacity.
                while self.entries.len() > self.max_entries {
                    self.entries.remove(0);
                }
            }
        }

        /// Returns true if the overlay should be visible (has recent entries).
        fn is_active(&self) -> bool {
            if self.entries.is_empty() {
                return false;
            }
            let newest = self.entries.last().unwrap().arrived;
            newest.elapsed() < Duration::from_secs(self.idle_secs as u64)
        }

        /// Compute required window height for current entries.
        unsafe fn required_height(&mut self) -> i32 {
            self.ensure_fonts();
            let normal_h = self.font_size + LINE_GAP;
            let featured_h = self.font_size * 2 + LINE_GAP;
            let n = self.entries.len();
            if n == 0 {
                return 0;
            }
            PAD_Y + (n - 1) as i32 * normal_h + featured_h + PAD_Y
        }
    }

    // ── Entry point ───────────────────────────────────────────────────────────

    /// Spawn the overlay window on a dedicated thread.  Returns immediately.
    /// The overlay reads from `queue`; the trigger engine pushes into it.
    pub fn spawn_overlay(cfg: Config, queue: Arc<Mutex<Vec<OverlayEvent>>>) {
        std::thread::Builder::new()
            .name("froklog-overlay".into())
            .spawn(move || run_overlay_thread(cfg, queue))
            .expect("spawn overlay thread");
    }

    #[cfg(target_os = "windows")]
    fn run_overlay_thread(cfg: Config, queue: Arc<Mutex<Vec<OverlayEvent>>>) {
        unsafe {
            let hmodule = GetModuleHandleW(PCWSTR::null()).unwrap_or_default();
            let hinstance = windows::Win32::Foundation::HINSTANCE(hmodule.0);
            let class_w: Vec<u16> = CLASS_NAME.encode_utf16().collect();

            let wc = WNDCLASSEXW {
                cbSize: std::mem::size_of::<WNDCLASSEXW>() as u32,
                style: CS_HREDRAW | CS_VREDRAW,
                lpfnWndProc: Some(overlay_wnd_proc),
                hInstance: hinstance,
                lpszClassName: PCWSTR(class_w.as_ptr()),
                hCursor: LoadCursorW(None, IDC_ARROW).unwrap_or_default(),
                hbrBackground: HBRUSH(std::ptr::null_mut()),
                ..Default::default()
            };
            let _ = RegisterClassExW(&wc);

            let (wx, wy) = initial_position(&cfg);

            let state = Box::new(OverlayState::new(&cfg, queue));
            let alpha = state.alpha;
            let state_ptr = Box::into_raw(state);

            let hwnd = CreateWindowExW(
                WS_EX_TOPMOST | WS_EX_LAYERED | WS_EX_TOOLWINDOW | WS_EX_NOACTIVATE,
                PCWSTR(class_w.as_ptr()),
                PCWSTR::null(),
                WS_POPUP | WS_VISIBLE,
                wx,
                wy,
                WINDOW_WIDTH,
                1, // height set on first paint
                None,
                None,
                hinstance,
                Some(state_ptr as *const c_void),
            )
            .expect("CreateWindowExW overlay");

            let _ = SetLayeredWindowAttributes(
                hwnd,
                windows::Win32::Foundation::COLORREF(0),
                alpha,
                LWA_ALPHA,
            );

            // Start the animation timer.
            windows::Win32::UI::WindowsAndMessaging::SetTimer(
                hwnd,
                TIMER_ANIM,
                ANIM_INTERVAL_MS,
                None,
            );

            let mut msg = MSG::default();
            while GetMessageW(&mut msg, None, 0, 0).as_bool() {
                let _ = TranslateMessage(&msg);
                DispatchMessageW(&msg);
            }
        }
    }

    #[cfg(not(target_os = "windows"))]
    fn run_overlay_thread(_cfg: Config, _queue: Arc<Mutex<Vec<OverlayEvent>>>) {}

    // ── Window procedure ──────────────────────────────────────────────────────

    #[cfg(target_os = "windows")]
    unsafe extern "system" fn overlay_wnd_proc(
        hwnd: HWND,
        msg: u32,
        wparam: WPARAM,
        lparam: LPARAM,
    ) -> LRESULT {
        if msg == WM_CREATE {
            let cs = &*(lparam.0 as *const CREATESTRUCTW);
            let ptr = cs.lpCreateParams as *mut OverlayState;
            SetWindowLongPtrW(hwnd, GWLP_USERDATA, ptr as isize);
            return LRESULT(0);
        }

        let ptr = GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *mut OverlayState;
        if ptr.is_null() {
            return DefWindowProcW(hwnd, msg, wparam, lparam);
        }
        let state = &mut *ptr;

        match msg {
            WM_TIMER => {
                // Drain new events from the shared queue.
                state.drain_queue();

                // Resize and repaint.
                if state.is_active() {
                    let h = state.required_height();
                    let mut rect = windows::Win32::Foundation::RECT::default();
                    let _ = GetWindowRect(hwnd, &mut rect);
                    let _ = SetWindowPos(
                        hwnd,
                        None,
                        0,
                        0,
                        state.window_w,
                        h,
                        SWP_NOMOVE | SWP_NOZORDER | SWP_NOOWNERZORDER,
                    );
                    windows::Win32::Graphics::Gdi::InvalidateRect(hwnd, None, false);
                } else {
                    // Collapse to 0-height rather than hiding so timer keeps firing.
                    let _ = SetWindowPos(
                        hwnd,
                        None,
                        0,
                        0,
                        state.window_w,
                        1,
                        SWP_NOMOVE | SWP_NOZORDER | SWP_NOOWNERZORDER,
                    );
                }

                LRESULT(0)
            }

            WM_APP_EVENT => {
                // Wakeup nudge — actual data is in the queue; timer will drain it.
                LRESULT(0)
            }

            WM_PAINT => {
                paint_overlay(hwnd, state);
                LRESULT(0)
            }

            WM_LBUTTONDOWN => {
                // Make the entire window draggable.
                let _ = windows::Win32::UI::Input::KeyboardAndMouse::ReleaseCapture();
                let _ = windows::Win32::UI::WindowsAndMessaging::SendMessageW(
                    hwnd,
                    WM_NCLBUTTONDOWN,
                    WPARAM(windows::Win32::UI::WindowsAndMessaging::HTCAPTION as usize),
                    LPARAM(0),
                );
                LRESULT(0)
            }

            WM_DESTROY => {
                drop(Box::from_raw(ptr));
                SetWindowLongPtrW(hwnd, GWLP_USERDATA, 0);
                PostQuitMessage(0);
                LRESULT(0)
            }

            _ => DefWindowProcW(hwnd, msg, wparam, lparam),
        }
    }

    // ── Painting ──────────────────────────────────────────────────────────────

    #[cfg(target_os = "windows")]
    unsafe fn paint_overlay(hwnd: HWND, state: &mut OverlayState) {
        let mut ps = PAINTSTRUCT::default();
        let hdc = BeginPaint(hwnd, &mut ps);

        // Get client rect.
        let mut client = windows::Win32::Foundation::RECT::default();
        windows::Win32::UI::WindowsAndMessaging::GetClientRect(hwnd, &mut client).ok();
        let w = client.right - client.left;
        let h = client.bottom - client.top;

        // Paint into a back-buffer to avoid flicker.
        let hdc_mem = CreateCompatibleDC(hdc);
        let hbm: HBITMAP = CreateCompatibleBitmap(hdc, w, h);
        let hbm_old = SelectObject(hdc_mem, HGDIOBJ(hbm.0));

        // Fill background.
        let bg_brush = CreateSolidBrush(windows::Win32::Foundation::COLORREF(COL_BG));
        let mut fill_rect = windows::Win32::Foundation::RECT {
            left: 0,
            top: 0,
            right: w,
            bottom: h,
        };
        FillRect(hdc_mem, &fill_rect, bg_brush);
        let _ = DeleteObject(HGDIOBJ(bg_brush.0));

        SetBkMode(hdc_mem, TRANSPARENT);

        state.ensure_fonts();

        let normal_h = state.font_size + LINE_GAP;
        let featured_h = state.font_size * 2 + LINE_GAP;
        let n = state.entries.len();

        let mut y = PAD_Y;

        for (i, entry) in state.entries.iter().enumerate() {
            let is_featured = i + 1 == n;
            let line_h = if is_featured { featured_h } else { normal_h };
            let font = if is_featured {
                state.hfont_featured.unwrap()
            } else {
                state.hfont_normal.unwrap()
            };

            // Compute alpha for old entries (fade after half the idle time).
            let age = entry.arrived.elapsed().as_secs_f32();
            let fade_start = state.idle_secs as f32 * 0.5;
            let alpha_factor = if is_featured {
                1.0f32
            } else {
                (1.0 - ((age - fade_start) / (state.idle_secs as f32 * 0.5)).clamp(0.0, 1.0))
                    .max(0.1)
            };

            let col = if is_featured { COL_FEATURED } else { COL_TEXT };
            // Blend the colour alpha with fade_factor by modulating each channel.
            let blended = blend_color(col, alpha_factor);
            SetTextColor(hdc_mem, windows::Win32::Foundation::COLORREF(blended));

            let old_font = SelectObject(hdc_mem, HGDIOBJ(font.0));

            // Icon column (draw a small coloured square or the first letter).
            let icon_size = line_h - LINE_GAP;
            draw_icon(hdc_mem, &entry.icon, PAD_X, y, icon_size, blended);

            // Message text.
            let tx = PAD_X + icon_size + ICON_TEXT_GAP;
            let text_w: Vec<u16> = entry.message.encode_utf16().collect();
            TextOutW(hdc_mem, tx, y, &text_w);

            SelectObject(hdc_mem, old_font);
            y += line_h;
        }

        // Blit back-buffer to screen.
        BitBlt(hdc, 0, 0, w, h, hdc_mem, 0, 0, SRCCOPY).ok();

        SelectObject(hdc_mem, hbm_old);
        let _ = DeleteObject(HGDIOBJ(hbm.0));
        let _ = DeleteDC(hdc_mem);

        EndPaint(hwnd, &ps);
    }

    // ── Icon rendering ────────────────────────────────────────────────────────

    /// Draw a simple coloured square with the first letter of the icon name,
    /// or load a PNG from disk if the path exists.
    #[cfg(target_os = "windows")]
    unsafe fn draw_icon(hdc: HDC, icon: &str, x: i32, y: i32, size: i32, text_color: u32) {
        let icon_color = icon_color_for(icon);
        let brush = CreateSolidBrush(windows::Win32::Foundation::COLORREF(icon_color));
        let rect = windows::Win32::Foundation::RECT {
            left: x,
            top: y,
            right: x + size,
            bottom: y + size,
        };
        FillRect(hdc, &rect, brush);
        let _ = DeleteObject(HGDIOBJ(brush.0));

        // Draw the first character of the icon name centred in the square.
        if let Some(ch) = icon.chars().next() {
            let s: String = ch.to_uppercase().collect();
            let sw: Vec<u16> = s.encode_utf16().collect();
            SetTextColor(hdc, windows::Win32::Foundation::COLORREF(0x00FFFFFF));
            TextOutW(hdc, x + size / 4, y + size / 6, &sw);
        }
    }

    fn icon_color_for(icon: &str) -> u32 {
        let lower = icon.to_lowercase();
        if lower.contains("heal") || lower.contains("cure") {
            0x0033CC33 // green
        } else if lower.contains("damage") || lower.contains("dmg") || lower.contains("death") {
            0x003333CC // red
        } else if lower.contains("warn") || lower.contains("alert") {
            0x0000AAFF // orange
        } else if lower.contains("spell") || lower.contains("cast") {
            0x00CC6600 // blue
        } else {
            0x00666666 // neutral grey
        }
    }

    fn blend_color(colorref: u32, factor: f32) -> u32 {
        let r = ((colorref & 0xFF) as f32 * factor) as u32;
        let g = (((colorref >> 8) & 0xFF) as f32 * factor) as u32;
        let b = (((colorref >> 16) & 0xFF) as f32 * factor) as u32;
        r | (g << 8) | (b << 16)
    }

    // ── Font creation ─────────────────────────────────────────────────────────

    #[cfg(target_os = "windows")]
    unsafe fn make_font(name: &str, pt_size: i32, bold: bool) -> HFONT {
        // Convert point size to logical units (pixels). 96 DPI assumed; good enough
        // for a game overlay that doesn't need DPI-awareness.
        let height = -pt_size * 96 / 72;
        let weight = if bold {
            FW_BOLD.0 as i32
        } else {
            FW_NORMAL.0 as i32
        };
        let name_w: Vec<u16> = name.encode_utf16().chain(std::iter::once(0u16)).collect();
        let mut face = [0u16; 32];
        let copy_len = name_w.len().min(31);
        face[..copy_len].copy_from_slice(&name_w[..copy_len]);

        CreateFontW(
            height,
            0,
            0,
            0,
            weight,
            0,
            0,
            0,
            DEFAULT_CHARSET.0 as u32,
            OUT_DEFAULT_PRECIS.0 as u32,
            CLIP_DEFAULT_PRECIS.0 as u32,
            DEFAULT_QUALITY.0 as u32,
            (DEFAULT_PITCH.0 | FF_DONTCARE.0) as u32,
            PCWSTR(face.as_ptr()),
        )
    }

    // ── Sound ─────────────────────────────────────────────────────────────────

    fn play_sound(path: &str) {
        #[cfg(target_os = "windows")]
        {
            let path = path.to_string();
            std::thread::spawn(move || unsafe {
                let path_w: Vec<u16> = path.encode_utf16().chain(std::iter::once(0u16)).collect();
                windows::Win32::Media::Audio::PlaySoundW(
                    PCWSTR(path_w.as_ptr()),
                    None,
                    windows::Win32::Media::Audio::SND_FILENAME
                        | windows::Win32::Media::Audio::SND_ASYNC,
                );
            });
        }
    }

    // ── Helpers ───────────────────────────────────────────────────────────────

    fn initial_position(cfg: &Config) -> (i32, i32) {
        #[cfg(target_os = "windows")]
        {
            if cfg.overlay_x >= 0 && cfg.overlay_y >= 0 {
                return (cfg.overlay_x, cfg.overlay_y);
            }
            unsafe {
                let sw = GetSystemMetrics(SM_CXSCREEN);
                let sh = GetSystemMetrics(SM_CYSCREEN);
                // Default: centred horizontally, bottom quarter of screen.
                let x = (sw - WINDOW_WIDTH) / 2;
                let y = sh * 3 / 4;
                (x, y)
            }
        }
        #[cfg(not(target_os = "windows"))]
        {
            (cfg.overlay_x.max(0), cfg.overlay_y.max(0))
        }
    }

    // ── Public helper ─────────────────────────────────────────────────────────

    /// Save the current window position back into the config and persist it.
    #[cfg(target_os = "windows")]
    pub fn save_position(hwnd_raw: usize, cfg: &mut Config) {
        unsafe {
            let hwnd = HWND(hwnd_raw as *mut c_void);
            let mut rect = windows::Win32::Foundation::RECT::default();
            if GetWindowRect(hwnd, &mut rect).is_ok() {
                cfg.overlay_x = rect.left;
                cfg.overlay_y = rect.top;
                cfg.save();
            }
        }
    }
}
