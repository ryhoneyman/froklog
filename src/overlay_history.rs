/// Previous-messages history overlay window.
///
/// A second Win32 `WS_EX_LAYERED` popup, independent of the fly-in alert
/// window (`overlay.rs`), showing a flat list of messages that have finished
/// their alert lifecycle: icon + message + "Ns" elapsed-time label. Reads
/// `AppHandle.overlay_history`, which `overlay.rs` appends to once a message
/// shrinks away.
#[cfg(feature = "tray")]
#[allow(clippy::module_inception)]
pub mod overlay_history {
    use std::ffi::c_void;
    use std::mem;
    use std::sync::atomic::Ordering;
    use std::sync::Arc;
    use std::time::Instant;

    use crate::config::Config;
    #[cfg(target_os = "windows")]
    use crate::overlay_draw::overlay_draw::{
        apply_lock_style, composite_text, composite_text_stroked, make_font, premult,
    };
    use crate::overlay_draw::overlay_draw::{fill_rrect, parse_hex_color};
    use crate::tray::tray::AppHandle;

    // ── Win32 imports ─────────────────────────────────────────────────────────

    #[cfg(target_os = "windows")]
    use windows::Win32::Foundation::{COLORREF, HWND, LPARAM, LRESULT, POINT, SIZE, WPARAM};

    #[cfg(target_os = "windows")]
    use windows::Win32::Graphics::Gdi::{
        BeginPaint, CreateCompatibleDC, CreateDIBSection, DeleteDC, DeleteObject, EndPaint, GetDC,
        ReleaseDC, SelectObject, BITMAPINFO, BITMAPINFOHEADER, BLENDFUNCTION, DIB_RGB_COLORS,
        DT_END_ELLIPSIS, DT_LEFT, DT_NOCLIP, DT_RIGHT, HBRUSH, HFONT, HGDIOBJ, PAINTSTRUCT,
    };

    #[cfg(target_os = "windows")]
    use windows::Win32::System::LibraryLoader::GetModuleHandleW;

    #[cfg(target_os = "windows")]
    use windows::Win32::UI::WindowsAndMessaging::{
        CreateWindowExW, DefWindowProcW, DestroyWindow, DispatchMessageW, GetMessageW,
        GetSystemMetrics, GetWindowLongPtrW, GetWindowRect, LoadCursorW, PostQuitMessage,
        RegisterClassExW, SetWindowLongPtrW, ShowWindow, TranslateMessage, UpdateLayeredWindow,
        CREATESTRUCTW, CS_HREDRAW, CS_VREDRAW, GWLP_USERDATA, IDC_ARROW, MSG, SM_CXSCREEN,
        SM_CYSCREEN, SW_HIDE, SW_SHOWNOACTIVATE, ULW_ALPHA, WM_APP, WM_CREATE, WM_DESTROY,
        WM_LBUTTONDOWN, WM_NCLBUTTONDOWN, WM_PAINT, WM_TIMER, WNDCLASSEXW, WS_EX_LAYERED,
        WS_EX_NOACTIVATE, WS_EX_TOOLWINDOW, WS_EX_TOPMOST, WS_POPUP,
    };

    #[cfg(target_os = "windows")]
    use windows::core::PCWSTR;

    // ── Constants ─────────────────────────────────────────────────────────────

    const WM_APP_EVENT: u32 = WM_APP + 10;
    const WM_EXITSIZEMOVE: u32 = 0x0232;
    const TIMER_HISTORY: usize = 1;
    const TIMER_INTERVAL_MS: u32 = 200;

    const ROW_H: i32 = 22;
    const PAD_X: i32 = 10;
    const PAD_Y: i32 = 8;
    const ICON_SZ: i32 = 15;
    const ICON_TEXT_GAP: i32 = 8;
    const CORNER_R: i32 = 6;
    const TIME_COL_W: i32 = 46;
    const FADE_IN_SECS: f32 = 0.25;

    const PANEL_BG: (u8, u8, u8, u8) = (12, 12, 17, 200);
    const ROW_BG_ALT: (u8, u8, u8, u8) = (255, 255, 255, 8);

    /// Default text fill/stroke when a trigger doesn't override them.
    const DEFAULT_TEXT_RGB: (u8, u8, u8) = (255, 255, 255);
    const DEFAULT_BORDER_RGB: (u8, u8, u8) = (0, 0, 0);

    const CLASS_NAME: &str = "FroklogOverlayHistory\0";

    // ── History entry ─────────────────────────────────────────────────────────

    /// One message that finished flying through the alert overlay.
    pub struct HistoryEntry {
        pub icon: String,
        pub color: String,
        pub message: String,
        pub message_color: String,
        pub border_color: String,
        pub arrived: Instant,
    }

    impl HistoryEntry {
        pub fn new(
            icon: String,
            color: String,
            message: String,
            message_color: String,
            border_color: String,
        ) -> Self {
            Self {
                icon,
                color,
                message,
                message_color,
                border_color,
                arrived: Instant::now(),
            }
        }
    }

    // ── State ─────────────────────────────────────────────────────────────────

    struct HistoryState {
        handle: Arc<AppHandle>,
        history_enabled: bool,
        max_entries: usize,
        idle_secs: u32,
        font_size: i32,
        window_x: i32,
        window_y: i32,
        window_w: i32,
        visible: bool,
        locked: bool,
        /// Mirrors `AppHandle.force_show_windows` — forces this window
        /// visible (with a placeholder row) so it can be dragged into
        /// position from the Settings dialog even with no real history.
        force_show: bool,
        #[cfg(target_os = "windows")]
        hfont: Option<HFONT>,
        icon_cache: std::collections::HashMap<String, Option<Vec<u32>>>,
    }

    #[cfg(target_os = "windows")]
    impl HistoryState {
        fn new(handle: &Arc<AppHandle>, wx: i32, wy: i32) -> Self {
            let cfg = handle.config.lock().unwrap();
            Self {
                handle: Arc::clone(handle),
                history_enabled: cfg.overlay_history_enabled,
                max_entries: cfg.overlay_history_max_entries.max(1),
                idle_secs: cfg.overlay_history_idle_secs,
                font_size: cfg.overlay_history_font_size.max(8) as i32,
                window_x: wx,
                window_y: wy,
                window_w: cfg.overlay_history_width.max(160),
                visible: false,
                locked: cfg.overlay_history_locked,
                force_show: handle.force_show_windows.load(Ordering::Relaxed),
                hfont: None,
                icon_cache: std::collections::HashMap::new(),
            }
        }

        unsafe fn ensure_font(&mut self) -> HFONT {
            if self.hfont.is_none() {
                self.hfont = Some(make_font("Segoe UI", self.font_size, false));
            }
            self.hfont.unwrap()
        }

        unsafe fn drop_font(&mut self) {
            if let Some(f) = self.hfont.take() {
                let _ = DeleteObject(HGDIOBJ(f.0));
            }
        }

        /// Reload live-tunable settings from config. Returns true if the
        /// click-through lock state changed and needs to be re-applied to the
        /// window style (covers both the in-window pin click and the settings
        /// dialog's checkbox — the window can't receive clicks while locked,
        /// so this poll is the only way an unlock via the dialog takes effect).
        fn sync_config(&mut self) -> bool {
            let (new_size, history_enabled, max_entries, idle_secs, window_w, locked) = {
                let cfg = self.handle.config.lock().unwrap();
                (
                    cfg.overlay_history_font_size.max(8) as i32,
                    cfg.overlay_history_enabled,
                    cfg.overlay_history_max_entries.max(1),
                    cfg.overlay_history_idle_secs,
                    cfg.overlay_history_width.max(160),
                    cfg.overlay_history_locked,
                )
            };
            if new_size != self.font_size {
                self.font_size = new_size;
                unsafe { self.drop_font() };
            }
            self.history_enabled = history_enabled;
            self.max_entries = max_entries;
            self.idle_secs = idle_secs;
            self.window_w = window_w;
            let lock_changed = locked != self.locked;
            self.locked = locked;
            self.force_show = self.handle.force_show_windows.load(Ordering::Relaxed);
            lock_changed
        }
    }

    // ── Icon loading & compositing ────────────────────────────────────────────

    fn load_icon_pixels_premult(filename: &str, size: i32) -> Option<Vec<u32>> {
        let exe = std::env::current_exe().ok()?;
        let path = exe.parent()?.join("icons").join(filename);
        let img = image::open(&path).ok()?;
        let img = img
            .resize_exact(
                size as u32,
                size as u32,
                image::imageops::FilterType::Nearest,
            )
            .into_rgba8();
        let raw = img.into_raw();
        let mut out = Vec::with_capacity((size * size) as usize);
        for chunk in raw.chunks_exact(4) {
            let (r, g, b, a) = (chunk[0], chunk[1], chunk[2], chunk[3]);
            #[cfg(target_os = "windows")]
            {
                out.push(premult(r, g, b, a));
            }
            #[cfg(not(target_os = "windows"))]
            {
                let _ = (r, g, b, a);
            }
        }
        Some(out)
    }

    #[cfg(target_os = "windows")]
    #[allow(clippy::too_many_arguments)]
    fn composite_icon(
        pix: &mut [u32],
        dw: i32,
        dh: i32,
        icon_pix: &[u32],
        icon_sz: i32,
        dx: i32,
        dy: i32,
        alpha: f32,
    ) {
        use crate::overlay_draw::overlay_draw::blend_over;
        let ka = (alpha * 255.0) as u32;
        for iy in 0..icon_sz {
            for ix in 0..icon_sz {
                let src = icon_pix[(iy * icon_sz + ix) as usize];
                let scaled = ((((src >> 24) * ka) >> 8) << 24)
                    | ((((src >> 16 & 0xFF) * ka) >> 8) << 16)
                    | ((((src >> 8 & 0xFF) * ka) >> 8) << 8)
                    | (((src & 0xFF) * ka) >> 8);
                let sx = dx + ix;
                let sy = dy + iy;
                if sx < 0 || sy < 0 || sx >= dw || sy >= dh {
                    continue;
                }
                let di = (sy * dw + sx) as usize;
                pix[di] = blend_over(scaled, pix[di]);
            }
        }
    }

    // ── Row snapshot ──────────────────────────────────────────────────────────

    /// One row's worth of display data, snapshotted from `HistoryEntry` each
    /// tick so rendering doesn't need the mutex held.
    struct HistoryRow {
        icon: String,
        color: String,
        message: String,
        message_color: String,
        border_color: String,
        secs_ago: f32,
    }

    // ── Frame rendering ───────────────────────────────────────────────────────

    #[cfg(target_os = "windows")]
    unsafe fn render_frame(hwnd: HWND, state: &mut HistoryState, entries: &[HistoryRow]) {
        let font = state.ensure_font();
        let w = state.window_w;
        let h = ROW_H * entries.len().max(1) as i32 + PAD_Y * 2;

        let bmi = BITMAPINFO {
            bmiHeader: BITMAPINFOHEADER {
                biSize: mem::size_of::<BITMAPINFOHEADER>() as u32,
                biWidth: w,
                biHeight: -h,
                biPlanes: 1,
                biBitCount: 32,
                ..Default::default()
            },
            ..Default::default()
        };

        let hdc_screen = GetDC(None);
        let hdc_mem = CreateCompatibleDC(hdc_screen);
        let mut bits: *mut c_void = std::ptr::null_mut();
        let Ok(hbm) = CreateDIBSection(hdc_screen, &bmi, DIB_RGB_COLORS, &mut bits, None, 0) else {
            let _ = DeleteDC(hdc_mem);
            ReleaseDC(None, hdc_screen);
            return;
        };
        let old_bm = SelectObject(hdc_mem, HGDIOBJ(hbm.0));

        let pix = std::slice::from_raw_parts_mut(bits as *mut u32, (w * h) as usize);
        pix.fill(0);

        let (pr, pg, pb, pa) = PANEL_BG;
        fill_rrect(pix, w, h, 0, 0, w, h, CORNER_R, premult(pr, pg, pb, pa));

        // Newest entry (last in the Vec) renders at the top row; older
        // entries are pushed further down as new ones arrive.
        for (i, row) in entries.iter().rev().enumerate() {
            let ry = PAD_Y + i as i32 * ROW_H;
            // Ramp 0 -> 1 over the first FADE_IN_SECS after arrival.
            let row_alpha = (row.secs_ago / FADE_IN_SECS).clamp(0.0, 1.0);

            if i % 2 == 1 {
                let (ar, ag, ab, aa) = ROW_BG_ALT;
                fill_rrect(pix, w, h, 0, ry, w, ry + ROW_H, 0, premult(ar, ag, ab, aa));
            }

            let icon_x = PAD_X;
            let icon_y = ry + (ROW_H - ICON_SZ) / 2;
            let drew_png = if row.icon.ends_with(".png") || row.icon.ends_with(".jpg") {
                state
                    .icon_cache
                    .entry(format!("{}@{}", row.icon, ICON_SZ))
                    .or_insert_with(|| load_icon_pixels_premult(&row.icon, ICON_SZ))
                    .as_ref()
                    .map(|icon_pix| {
                        composite_icon(pix, w, h, icon_pix, ICON_SZ, icon_x, icon_y, row_alpha)
                    })
                    .is_some()
            } else {
                false
            };
            if !drew_png {
                let (ir, ig, ib) = parse_hex_color(&row.color)
                    .map(|c| {
                        (
                            (c >> 16 & 0xFF) as u8,
                            (c >> 8 & 0xFF) as u8,
                            (c & 0xFF) as u8,
                        )
                    })
                    .unwrap_or((150, 150, 160));
                fill_rrect(
                    pix,
                    w,
                    h,
                    icon_x,
                    icon_y,
                    icon_x + ICON_SZ,
                    icon_y + ICON_SZ,
                    3,
                    premult(ir, ig, ib, (200.0 * row_alpha) as u8),
                );
            }

            let text_rgb = parse_hex_color(&row.message_color)
                .map(|c| {
                    (
                        (c >> 16 & 0xFF) as u8,
                        (c >> 8 & 0xFF) as u8,
                        (c & 0xFF) as u8,
                    )
                })
                .unwrap_or(DEFAULT_TEXT_RGB);
            let border_rgb = parse_hex_color(&row.border_color)
                .map(|c| {
                    (
                        (c >> 16 & 0xFF) as u8,
                        (c >> 8 & 0xFF) as u8,
                        (c & 0xFF) as u8,
                    )
                })
                .unwrap_or(DEFAULT_BORDER_RGB);
            let text_x = PAD_X + ICON_SZ + ICON_TEXT_GAP;
            composite_text_stroked(
                pix,
                w,
                h,
                &row.message,
                font,
                text_rgb,
                border_rgb,
                1,
                row_alpha,
                text_x,
                ry,
                w - text_x - TIME_COL_W - PAD_X,
                ROW_H,
                DT_LEFT | DT_END_ELLIPSIS,
            );
            composite_text(
                pix,
                w,
                h,
                &format!("{}s", row.secs_ago as u64),
                font,
                (140, 140, 150),
                row_alpha,
                w - TIME_COL_W - PAD_X,
                ry,
                TIME_COL_W,
                ROW_H,
                DT_RIGHT | DT_NOCLIP,
            );
        }

        let dst_pt = POINT {
            x: state.window_x,
            y: state.window_y,
        };
        let win_size = SIZE { cx: w, cy: h };
        let src_pt = POINT { x: 0, y: 0 };
        let blend = BLENDFUNCTION {
            BlendOp: 0,
            BlendFlags: 0,
            SourceConstantAlpha: 255,
            AlphaFormat: 1,
        };
        let _ = UpdateLayeredWindow(
            hwnd,
            hdc_screen,
            Some(&dst_pt),
            Some(&win_size),
            hdc_mem,
            Some(&src_pt),
            COLORREF(0),
            Some(&blend),
            ULW_ALPHA,
        );

        SelectObject(hdc_mem, old_bm);
        let _ = DeleteObject(HGDIOBJ(hbm.0));
        let _ = DeleteDC(hdc_mem);
        ReleaseDC(None, hdc_screen);
    }

    // ── Entry point ───────────────────────────────────────────────────────────

    pub fn spawn_overlay_history(handle: Arc<AppHandle>) {
        std::thread::Builder::new()
            .name("froklog-overlay-history".into())
            .spawn(move || run_history_thread(handle))
            .expect("spawn overlay history thread");
    }

    #[cfg(target_os = "windows")]
    fn run_history_thread(handle: Arc<AppHandle>) {
        unsafe {
            let hmodule = GetModuleHandleW(PCWSTR::null()).unwrap_or_default();
            let hinstance = windows::Win32::Foundation::HINSTANCE(hmodule.0);
            let class_w: Vec<u16> = CLASS_NAME.encode_utf16().collect();

            let wc = WNDCLASSEXW {
                cbSize: mem::size_of::<WNDCLASSEXW>() as u32,
                style: CS_HREDRAW | CS_VREDRAW,
                lpfnWndProc: Some(history_wnd_proc),
                hInstance: hinstance,
                lpszClassName: PCWSTR(class_w.as_ptr()),
                hCursor: LoadCursorW(None, IDC_ARROW).unwrap_or_default(),
                hbrBackground: HBRUSH(std::ptr::null_mut()),
                ..Default::default()
            };
            let _ = RegisterClassExW(&wc);

            let (wx, wy) = initial_position(&handle.config.lock().unwrap());
            let state = Box::new(HistoryState::new(&handle, wx, wy));
            let initial_w = state.window_w;
            let state_ptr = Box::into_raw(state);

            let hwnd = CreateWindowExW(
                WS_EX_TOPMOST | WS_EX_LAYERED | WS_EX_TOOLWINDOW | WS_EX_NOACTIVATE,
                PCWSTR(class_w.as_ptr()),
                PCWSTR::null(),
                WS_POPUP,
                wx,
                wy,
                initial_w,
                1,
                None,
                None,
                hinstance,
                Some(state_ptr as *const c_void),
            )
            .expect("CreateWindowExW overlay history");

            handle
                .overlay_history_hwnd
                .store(hwnd.0 as isize, Ordering::Relaxed);

            windows::Win32::UI::WindowsAndMessaging::SetTimer(
                hwnd,
                TIMER_HISTORY,
                TIMER_INTERVAL_MS,
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
    fn run_history_thread(_handle: Arc<AppHandle>) {}

    // ── Window procedure ──────────────────────────────────────────────────────

    #[cfg(target_os = "windows")]
    unsafe extern "system" fn history_wnd_proc(
        hwnd: HWND,
        msg: u32,
        wparam: WPARAM,
        lparam: LPARAM,
    ) -> LRESULT {
        if msg == WM_CREATE {
            let cs = &*(lparam.0 as *const CREATESTRUCTW);
            SetWindowLongPtrW(hwnd, GWLP_USERDATA, cs.lpCreateParams as isize);
            return LRESULT(0);
        }

        let ptr = GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *mut HistoryState;
        if ptr.is_null() {
            return DefWindowProcW(hwnd, msg, wparam, lparam);
        }
        let state = &mut *ptr;

        match msg {
            WM_TIMER => {
                if state.handle.quit.load(Ordering::Relaxed) {
                    let _ = DestroyWindow(hwnd);
                    return LRESULT(0);
                }
                let lock_changed = state.sync_config();
                if lock_changed {
                    apply_lock_style(hwnd, state.locked);
                }

                let entries: Vec<HistoryRow> = {
                    let mut hist = state.handle.overlay_history.lock().unwrap();
                    let max = state.max_entries;
                    if hist.len() > max {
                        let excess = hist.len() - max;
                        hist.drain(0..excess);
                    }
                    hist.iter()
                        .map(|e| HistoryRow {
                            icon: e.icon.clone(),
                            color: e.color.clone(),
                            message: e.message.clone(),
                            message_color: e.message_color.clone(),
                            border_color: e.border_color.clone(),
                            secs_ago: e.arrived.elapsed().as_secs_f32(),
                        })
                        .collect()
                };

                let last_arrived_secs = entries.last().map(|e| e.secs_ago);
                let idle_timed_out = state.idle_secs > 0
                    && last_arrived_secs.is_some_and(|s| s > state.idle_secs as f32);
                let show = state.force_show
                    || (state.history_enabled && !entries.is_empty() && !idle_timed_out);
                if !show {
                    if state.visible {
                        let _ = ShowWindow(hwnd, SW_HIDE);
                        state.visible = false;
                    }
                    return LRESULT(0);
                }
                if !state.visible {
                    let _ = ShowWindow(hwnd, SW_SHOWNOACTIVATE);
                    state.visible = true;
                }
                if entries.is_empty() {
                    // force_show with nothing real to display yet — draw a
                    // placeholder row so there's something to grab and drag.
                    let placeholder = [HistoryRow {
                        icon: String::new(),
                        color: String::new(),
                        message: "Drag to position — History Overlay".to_string(),
                        message_color: String::new(),
                        border_color: String::new(),
                        secs_ago: 0.0,
                    }];
                    render_frame(hwnd, state, &placeholder);
                } else {
                    render_frame(hwnd, state, &entries);
                }
                LRESULT(0)
            }

            WM_PAINT => {
                let mut ps = PAINTSTRUCT::default();
                let _ = BeginPaint(hwnd, &mut ps);
                let _ = EndPaint(hwnd, &ps);
                LRESULT(0)
            }

            WM_APP_EVENT => LRESULT(0),

            WM_LBUTTONDOWN => {
                let _ = windows::Win32::UI::Input::KeyboardAndMouse::ReleaseCapture();
                let _ = windows::Win32::UI::WindowsAndMessaging::SendMessageW(
                    hwnd,
                    WM_NCLBUTTONDOWN,
                    WPARAM(windows::Win32::UI::WindowsAndMessaging::HTCAPTION as usize),
                    LPARAM(0),
                );
                LRESULT(0)
            }

            WM_EXITSIZEMOVE => {
                let mut rect = windows::Win32::Foundation::RECT::default();
                if GetWindowRect(hwnd, &mut rect).is_ok() {
                    state.window_x = rect.left;
                    state.window_y = rect.top;
                    let mut cfg = state.handle.config.lock().unwrap();
                    cfg.overlay_history_x = rect.left;
                    cfg.overlay_history_y = rect.top;
                    cfg.save();
                }
                LRESULT(0)
            }

            WM_DESTROY => {
                state.drop_font();
                state.icon_cache.clear();
                drop(Box::from_raw(ptr));
                SetWindowLongPtrW(hwnd, GWLP_USERDATA, 0);
                PostQuitMessage(0);
                LRESULT(0)
            }

            _ => DefWindowProcW(hwnd, msg, wparam, lparam),
        }
    }

    /// Returns `(x, y)` — top-left position. Defaults to just below the alert
    /// overlay's default anchor if never moved.
    fn initial_position(cfg: &Config) -> (i32, i32) {
        #[cfg(target_os = "windows")]
        {
            if cfg.overlay_history_x >= 0 && cfg.overlay_history_y >= 0 {
                return (cfg.overlay_history_x, cfg.overlay_history_y);
            }
            unsafe {
                let sw = GetSystemMetrics(SM_CXSCREEN);
                let sh = GetSystemMetrics(SM_CYSCREEN);
                let w = cfg.overlay_history_width.max(160);
                ((sw - w) / 2, (sh as f32 * 0.55) as i32)
            }
        }
        #[cfg(not(target_os = "windows"))]
        (cfg.overlay_history_x.max(0), cfg.overlay_history_y.max(0))
    }
}
