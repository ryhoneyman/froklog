/// Staggered 2.5D Stacked Deck overlay window.
///
/// Per-pixel-alpha `WS_EX_LAYERED` Win32 popup rendered into a 32-bpp
/// premultiplied-BGRA DIB and pushed to the screen via `UpdateLayeredWindow`.
/// Each log event becomes a `DeckCard` with independently interpolated
/// `cur_y`, `cur_alpha`, and `cur_scale`; a 16 ms `WM_TIMER` drives the
/// ease-out-cubic spring step every frame (~60 fps).
///
/// Deck layout (top → bottom, oldest → newest):
///   history[k]  — compressed-height cards, decaying opacity & scale
///   spotlight   — full-height card, 100 % opacity, left accent bar, bold text
#[cfg(feature = "tray")]
#[allow(clippy::module_inception)]
pub mod overlay {
    use std::ffi::c_void;
    use std::mem;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::{Arc, Mutex};
    use std::time::{Duration, Instant};

    use crate::config::Config;
    use crate::triggers::engine::OverlayEvent;

    // ── Win32 imports ─────────────────────────────────────────────────────────

    #[cfg(target_os = "windows")]
    use windows::Win32::Foundation::{COLORREF, HWND, LPARAM, LRESULT, POINT, SIZE, WPARAM};

    #[cfg(target_os = "windows")]
    use windows::Win32::Graphics::Gdi::{
        BeginPaint, CreateCompatibleDC, CreateDIBSection, CreateFontW, DeleteDC, DeleteObject,
        DrawTextW, EndPaint, GetDC, ReleaseDC, SelectObject, SetBkMode, SetTextColor, BITMAPINFO,
        BITMAPINFOHEADER, BLENDFUNCTION, CLIP_DEFAULT_PRECIS, DEFAULT_CHARSET, DEFAULT_PITCH,
        DEFAULT_QUALITY, DIB_RGB_COLORS, DT_LEFT, DT_NOCLIP, DT_SINGLELINE, DT_VCENTER,
        FF_DONTCARE, FW_BOLD, FW_NORMAL, HBRUSH, HFONT, HGDIOBJ, OUT_DEFAULT_PRECIS, PAINTSTRUCT,
        TRANSPARENT,
    };

    #[cfg(target_os = "windows")]
    use windows::Win32::System::LibraryLoader::GetModuleHandleW;

    #[cfg(target_os = "windows")]
    use windows::Win32::UI::WindowsAndMessaging::{
        CreateWindowExW, DefWindowProcW, DestroyWindow, DispatchMessageW, GetMessageW,
        GetSystemMetrics, GetWindowLongPtrW, GetWindowRect, LoadCursorW, PostQuitMessage,
        RegisterClassExW, SetWindowLongPtrW, TranslateMessage, UpdateLayeredWindow, CREATESTRUCTW,
        CS_HREDRAW, CS_VREDRAW, GWLP_USERDATA, IDC_ARROW, MSG, SM_CXSCREEN, SM_CYSCREEN, ULW_ALPHA,
        WM_APP, WM_CREATE, WM_DESTROY, WM_LBUTTONDOWN, WM_NCLBUTTONDOWN, WM_PAINT, WM_TIMER,
        WNDCLASSEXW, WS_EX_LAYERED, WS_EX_NOACTIVATE, WS_EX_TOOLWINDOW, WS_EX_TOPMOST, WS_POPUP,
        WS_VISIBLE,
    };

    #[cfg(target_os = "windows")]
    use windows::core::PCWSTR;

    // ── Constants ─────────────────────────────────────────────────────────────

    const WM_APP_EVENT: u32 = WM_APP + 10;
    const WM_EXITSIZEMOVE: u32 = 0x0232;
    const TIMER_ANIM: usize = 1;
    const ANIM_INTERVAL_MS: u32 = 16;

    const WINDOW_WIDTH: i32 = 500;
    const MAX_DECK: usize = 20;

    const SPOTLIGHT_H: i32 = 52;
    const HISTORY_H: i32 = 28;
    const CARD_GAP: i32 = 3;
    const CARD_PAD_V: i32 = 8;
    const ACCENT_W: i32 = 4;
    const ICON_SZ_SPOT: i32 = 30;
    const ICON_SZ_HIST: i32 = 16;
    const CORNER_R: i32 = 5;
    const ICON_TEXT_GAP: i32 = 8;
    const CARD_PAD_X: i32 = 10;

    const SPRING: f32 = 14.0;

    const HIST_ALPHA_TOP: f32 = 0.90;
    const HIST_ALPHA_MIN: f32 = 0.20;
    const HIST_SCALE_TOP: f32 = 0.95;
    const HIST_SCALE_MIN: f32 = 0.70;

    const CARD_BG: (u8, u8, u8, u8) = (14, 14, 20, 210);

    const CLASS_NAME: &str = "FroklogOverlay\0";

    // ── Event category ────────────────────────────────────────────────────────

    #[derive(Clone, Copy, PartialEq)]
    enum EventCategory {
        Combat,
        Loot,
        System,
    }

    impl EventCategory {
        fn from_icon(icon: &str) -> Self {
            match icon {
                "damage" | "warn" | "skull.png" | "sword.png" | "alert.png" => Self::Combat,
                "heal" | "spell" | "heart.png" | "star.png" | "lightning.png" => Self::Loot,
                _ => Self::System,
            }
        }

        fn accent(&self) -> (u8, u8, u8) {
            match self {
                Self::Combat => (218, 48, 58),
                Self::Loot => (220, 180, 28),
                Self::System => (68, 120, 192),
            }
        }

        fn text_rgb(&self) -> (u8, u8, u8) {
            match self {
                Self::Combat => (255, 180, 180),
                Self::Loot => (255, 240, 160),
                Self::System => (190, 210, 255),
            }
        }
    }

    // ── Deck card ─────────────────────────────────────────────────────────────

    struct DeckCard {
        event: OverlayEvent,
        category: EventCategory,
        arrived: Instant,
        cur_y: f32,
        target_y: f32,
        cur_alpha: f32,
        target_alpha: f32,
        cur_scale: f32,
        target_scale: f32,
    }

    impl DeckCard {
        fn new(event: OverlayEvent, spawn_y: f32) -> Self {
            let category = EventCategory::from_icon(&event.icon);
            Self {
                category,
                event,
                arrived: Instant::now(),
                cur_y: spawn_y,
                target_y: spawn_y,
                cur_alpha: 0.0,
                target_alpha: 1.0,
                cur_scale: 0.8,
                target_scale: 1.0,
            }
        }
    }

    // ── State ─────────────────────────────────────────────────────────────────

    struct OverlayState {
        config: Arc<Mutex<Config>>,
        quit: Arc<AtomicBool>,
        queue: Arc<Mutex<Vec<OverlayEvent>>>,
        deck: Vec<DeckCard>,
        last_tick: Instant,
        overlay_enabled: bool,
        max_entries: usize,
        idle_secs: u32,
        font_size: i32,
        font_name: String,
        window_w: i32,
        window_x: i32,
        anchor_bottom_y: i32,
        #[cfg(target_os = "windows")]
        hfont_normal: Option<HFONT>,
        #[cfg(target_os = "windows")]
        hfont_bold: Option<HFONT>,
        /// Premultiplied BGRA pixel cache keyed by `"filename@size"`.
        /// `None` value means the load was attempted and failed.
        icon_cache: std::collections::HashMap<String, Option<Vec<u32>>>,
        /// GDI font cache for history cards keyed by rounded pt size.
        scaled_font_cache: std::collections::HashMap<i32, HFONT>,
        /// SAPI ISpVoice — created lazily on first TTS event.
        #[cfg(target_os = "windows")]
        tts_voice: Option<windows::Win32::Media::Speech::ISpVoice>,
        /// Cached SAPI rate value so we only call SetRate when it changes.
        #[cfg(target_os = "windows")]
        tts_rate: i32,
    }

    #[cfg(target_os = "windows")]
    impl OverlayState {
        fn new(
            cfg_arc: &Arc<Mutex<Config>>,
            queue: Arc<Mutex<Vec<OverlayEvent>>>,
            quit: Arc<AtomicBool>,
            wx: i32,
            anchor_bottom_y: i32,
        ) -> Self {
            let cfg = cfg_arc.lock().unwrap();
            Self {
                config: Arc::clone(cfg_arc),
                quit,
                queue,
                deck: Vec::new(),
                last_tick: Instant::now(),
                overlay_enabled: cfg.overlay_enabled,
                max_entries: cfg.overlay_max_entries.max(1).min(MAX_DECK),
                idle_secs: cfg.overlay_idle_secs,
                font_size: cfg.overlay_font_size.max(8) as i32,
                font_name: if cfg.overlay_font.is_empty() {
                    "Segoe UI".to_string()
                } else {
                    cfg.overlay_font.clone()
                },
                window_w: WINDOW_WIDTH,
                window_x: wx,
                anchor_bottom_y,
                hfont_normal: None,
                hfont_bold: None,
                icon_cache: std::collections::HashMap::new(),
                scaled_font_cache: std::collections::HashMap::new(),
                tts_voice: None,
                tts_rate: 0,
            }
        }

        unsafe fn ensure_fonts(&mut self) {
            if self.hfont_normal.is_none() {
                self.hfont_normal = Some(make_font(&self.font_name, self.font_size, false));
            }
            if self.hfont_bold.is_none() {
                self.hfont_bold = Some(make_font(&self.font_name, self.font_size, true));
            }
        }

        unsafe fn drop_fonts(&mut self) {
            for f in [self.hfont_normal.take(), self.hfont_bold.take()]
                .into_iter()
                .flatten()
            {
                let _ = DeleteObject(HGDIOBJ(f.0));
            }
            for f in self.scaled_font_cache.drain().map(|(_, f)| f) {
                let _ = DeleteObject(HGDIOBJ(f.0));
            }
        }

        fn drop_icon_cache(&mut self) {
            self.icon_cache.clear();
        }

        unsafe fn sync_config(&mut self) {
            let cfg = self.config.lock().unwrap();
            let new_font = if cfg.overlay_font.is_empty() {
                "Segoe UI".to_string()
            } else {
                cfg.overlay_font.clone()
            };
            let new_size = cfg.overlay_font_size.max(8) as i32;
            let new_idle = cfg.overlay_idle_secs;
            let new_max = cfg.overlay_max_entries.max(1).min(MAX_DECK);
            let new_enabled = cfg.overlay_enabled;
            let new_voice_id = cfg.tts_voice.clone();
            drop(cfg);

            // If the selected voice changed, drop the cached ISpVoice so it is
            // recreated with the new voice on the next TTS event.
            let cur_voice_id = if let Some(ref v) = self.tts_voice {
                if let Ok(tok) = v.GetVoice() {
                    if let Ok(id_ptr) = tok.GetId() {
                        let full = id_ptr.to_string().unwrap_or_default();
                        windows::Win32::System::Com::CoTaskMemFree(Some(id_ptr.0 as *mut _));
                        full.rsplit('\\').next().unwrap_or("").to_string()
                    } else {
                        String::new()
                    }
                } else {
                    String::new()
                }
            } else {
                String::new()
            };
            if new_voice_id != cur_voice_id {
                self.tts_voice = None;
            }

            if new_font != self.font_name || new_size != self.font_size {
                self.font_name = new_font;
                self.font_size = new_size;
                self.drop_fonts();
            }
            self.idle_secs = new_idle;
            self.overlay_enabled = new_enabled;
            if new_max != self.max_entries {
                self.max_entries = new_max;
                while self.deck.len() > self.max_entries {
                    self.deck.remove(0);
                }
                assign_deck_targets(&mut self.deck, self.max_entries);
            }
        }

        fn drain_queue(&mut self) {
            let new_events: Vec<OverlayEvent> = {
                let mut q = self.queue.lock().unwrap();
                q.drain(..).collect()
            };
            if new_events.is_empty() {
                return;
            }
            for ev in new_events {
                // TTS — speak if enabled and text is present.
                #[cfg(target_os = "windows")]
                if let Some(ref text) = ev.tts_text {
                    unsafe { self.try_speak(text, &ev.tts_priority) };
                }

                // Visual — only push a deck card when a message is present.
                if !ev.message.is_empty() {
                    if let Some(s) = &ev.sound {
                        if !s.is_empty() {
                            play_sound(s);
                        }
                    }
                    // Evict any cards that were fading out on idle before adding new ones.
                    self.deck.retain(|c| c.target_alpha > 0.001);
                    let spawn_y = self.window_height() as f32 + SPOTLIGHT_H as f32;
                    self.deck.push(DeckCard::new(ev, spawn_y));
                    while self.deck.len() > self.max_entries {
                        self.deck.remove(0);
                    }
                    assign_deck_targets(&mut self.deck, self.max_entries);
                } else if ev.sound.as_deref().is_some_and(|s| !s.is_empty()) {
                    play_sound(ev.sound.as_deref().unwrap());
                }
            }
        }

        fn tick_animate(&mut self) {
            let now = Instant::now();
            let dt = now.duration_since(self.last_tick).as_secs_f32().min(0.1);
            self.last_tick = now;

            for card in &mut self.deck {
                card.cur_y = step_lerp(card.cur_y, card.target_y, dt, SPRING);
                card.cur_alpha = step_lerp(card.cur_alpha, card.target_alpha, dt, SPRING);
                card.cur_scale = step_lerp(card.cur_scale, card.target_scale, dt, SPRING);
            }

            // Start fading out all cards once the overlay goes idle (0 = never hide).
            if self.idle_secs > 0 {
                if let Some(newest) = self.deck.last() {
                    if newest.arrived.elapsed() > Duration::from_secs(self.idle_secs as u64) {
                        for c in &mut self.deck {
                            c.target_alpha = 0.0;
                        }
                    }
                }
            }

            self.deck
                .retain(|c| c.cur_alpha > 0.005 || c.target_alpha > 0.001);
        }

        fn window_height(&self) -> i32 {
            if self.deck.is_empty() {
                return SPOTLIGHT_H + CARD_PAD_V * 2;
            }
            let n = self.deck.len();
            let hist_h: i32 = self.deck[..n.saturating_sub(1)]
                .iter()
                .map(|c| (HISTORY_H as f32 * c.target_scale).round() as i32 + CARD_GAP)
                .sum();
            hist_h + SPOTLIGHT_H + CARD_PAD_V * 2
        }
    }

    // ── Animation & layout ────────────────────────────────────────────────────

    /// Ease-out-cubic spring: moves `cur` toward `target` by `speed` units/s.
    fn step_lerp(cur: f32, target: f32, dt: f32, speed: f32) -> f32 {
        let t = (speed * dt).min(1.0);
        let ease = 1.0 - (1.0 - t).powi(3);
        cur + (target - cur) * ease
    }

    fn assign_deck_targets(deck: &mut [DeckCard], max_entries: usize) {
        let n = deck.len();
        if n == 0 {
            return;
        }
        // Spread the fade from HIST_ALPHA_TOP → HIST_ALPHA_MIN across the full
        // configured history depth so the topmost card is always slightly readable.
        let hist_slots = (max_entries.saturating_sub(1)).max(1) as f32;

        // Pass 1: set target_alpha and target_scale for every card.
        for (i, card) in deck.iter_mut().enumerate() {
            let from_bottom = (n - 1 - i) as i32;
            if from_bottom == 0 {
                card.target_alpha = 1.0;
                card.target_scale = 1.0;
            } else {
                // t ∈ [0, 1]: 0 = just above spotlight, 1 = oldest/topmost slot
                let t = ((from_bottom - 1) as f32 / hist_slots).min(1.0);
                let inv = 1.0 - t;
                card.target_alpha =
                    HIST_ALPHA_MIN + (HIST_ALPHA_TOP - HIST_ALPHA_MIN) * inv.powf(1.5);
                card.target_scale = HIST_SCALE_MIN + (HIST_SCALE_TOP - HIST_SCALE_MIN) * inv;
            }
        }

        // Pass 2: compute target_y by stacking scaled card heights bottom-up.
        // History cards shrink in height with their scale so stacking stays tight.
        let hist_h: i32 = deck[..n.saturating_sub(1)]
            .iter()
            .map(|c| (HISTORY_H as f32 * c.target_scale).round() as i32 + CARD_GAP)
            .sum();
        let window_h = hist_h + SPOTLIGHT_H + CARD_PAD_V * 2;
        let spotlight_y = (window_h - CARD_PAD_V - SPOTLIGHT_H) as f32;

        deck[n - 1].target_y = spotlight_y;
        let mut y_top = spotlight_y;
        for i in (0..n - 1).rev() {
            let card_h = (HISTORY_H as f32 * deck[i].target_scale).round() as f32;
            y_top -= CARD_GAP as f32 + card_h;
            deck[i].target_y = y_top;
        }
    }

    // ── Pixel helpers ─────────────────────────────────────────────────────────

    /// Pack an RGBA colour into a premultiplied 32-bpp DIB pixel.
    /// Memory layout: [B, G, R, A] → u32 = (A<<24)|(R<<16)|(G<<8)|B.
    #[inline]
    fn premult(r: u8, g: u8, b: u8, a: u8) -> u32 {
        let a32 = a as u32;
        ((a32) << 24)
            | ((r as u32 * a32 / 255) << 16)
            | ((g as u32 * a32 / 255) << 8)
            | (b as u32 * a32 / 255)
    }

    /// Porter-Duff src-over on two premultiplied pixels.
    #[inline]
    fn blend_over(src: u32, dst: u32) -> u32 {
        let sa = src >> 24;
        let inv = 255 - sa;
        let ch = |sh: u32| ((src >> sh & 0xFF) + ((dst >> sh & 0xFF) * inv >> 8)).min(255);
        ((sa + ((dst >> 24) * inv >> 8)).min(255) << 24) | (ch(16) << 16) | (ch(8) << 8) | ch(0)
    }

    /// Fill a rounded rectangle into the DIB pixel slice.
    fn fill_rrect(
        pix: &mut [u32],
        dw: i32,
        dh: i32,
        x1: i32,
        y1: i32,
        x2: i32,
        y2: i32,
        r: i32,
        c: u32,
    ) {
        let (x1, y1) = (x1.max(0), y1.max(0));
        let (x2, y2) = (x2.min(dw), y2.min(dh));
        let rf = r as f32;
        for y in y1..y2 {
            for x in x1..x2 {
                if (x < x1 + r || x >= x2 - r) && (y < y1 + r || y >= y2 - r) {
                    let cx = if x < x1 + r {
                        (x1 + r) as f32
                    } else {
                        (x2 - r) as f32
                    };
                    let cy = if y < y1 + r {
                        (y1 + r) as f32
                    } else {
                        (y2 - r) as f32
                    };
                    let dx = x as f32 - cx;
                    let dy = y as f32 - cy;
                    if dx * dx + dy * dy > rf * rf {
                        continue;
                    }
                }
                let idx = (y * dw + x) as usize;
                pix[idx] = blend_over(c, pix[idx]);
            }
        }
    }

    /// Fill a plain rectangle into the DIB pixel slice.
    fn fill_rect(pix: &mut [u32], dw: i32, dh: i32, x1: i32, y1: i32, x2: i32, y2: i32, c: u32) {
        let (x1, y1) = (x1.max(0), y1.max(0));
        let (x2, y2) = (x2.min(dw), y2.min(dh));
        for y in y1..y2 {
            let base = (y * dw) as usize;
            for x in x1 as usize..x2 as usize {
                pix[base + x] = blend_over(c, pix[base + x]);
            }
        }
    }

    // ── Icon loading & compositing ────────────────────────────────────────────

    /// Load a PNG/JPG icon from the `icons/` directory next to the executable.
    /// Returns premultiplied BGRA pixels at `size×size`, or `None` on failure.
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
            out.push(premult(r, g, b, a));
        }
        Some(out)
    }

    /// Composite premultiplied icon pixels into the main DIB at `(dx, dy)`,
    /// scaled by `card_alpha`.
    fn composite_icon(
        pix: &mut [u32],
        dw: i32,
        dh: i32,
        icon_pix: &[u32],
        icon_sz: i32,
        dx: i32,
        dy: i32,
        card_alpha: f32,
    ) {
        let ka = (card_alpha * 255.0) as u32;
        for iy in 0..icon_sz {
            for ix in 0..icon_sz {
                let src = icon_pix[(iy * icon_sz + ix) as usize];
                // Scale all channels of a premultiplied pixel by ka/255.
                let scaled = (((src >> 24) * ka >> 8) << 24)
                    | (((src >> 16 & 0xFF) * ka >> 8) << 16)
                    | (((src >> 8 & 0xFF) * ka >> 8) << 8)
                    | ((src & 0xFF) * ka >> 8);
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

    // ── GDI text → DIB compositing ────────────────────────────────────────────

    /// Draw `text` with GDI (white on opaque black), extract per-pixel coverage
    /// from the brightness channel, then alpha-composite with `text_rgb` into `pix`.
    #[cfg(target_os = "windows")]
    unsafe fn composite_text(
        pix: &mut [u32],
        dw: i32,
        dh: i32,
        text: &str,
        font: HFONT,
        (tr, tg, tb): (u8, u8, u8),
        alpha: f32,
        dx: i32,
        dy: i32,
        rw: i32,
        rh: i32,
    ) {
        if rw <= 0 || rh <= 0 || alpha < 0.004 {
            return;
        }

        let bmi = BITMAPINFO {
            bmiHeader: BITMAPINFOHEADER {
                biSize: mem::size_of::<BITMAPINFOHEADER>() as u32,
                biWidth: rw,
                biHeight: -rh,
                biPlanes: 1,
                biBitCount: 32,
                ..Default::default()
            },
            ..Default::default()
        };

        let hdc_screen = GetDC(None);
        let hdc_tmp = CreateCompatibleDC(hdc_screen);
        let mut bits: *mut c_void = std::ptr::null_mut();
        let Ok(hbm) = CreateDIBSection(hdc_screen, &bmi, DIB_RGB_COLORS, &mut bits, None, 0) else {
            let _ = DeleteDC(hdc_tmp);
            ReleaseDC(None, hdc_screen);
            return;
        };
        ReleaseDC(None, hdc_screen);

        let tmp = std::slice::from_raw_parts_mut(bits as *mut u32, (rw * rh) as usize);
        // Opaque black background. GDI will not write alpha; any non-black pixel
        // after drawing is text coverage (handles anti-aliasing correctly).
        tmp.fill(0xFF00_0000u32);

        let old_bm = SelectObject(hdc_tmp, HGDIOBJ(hbm.0));
        let old_fn = SelectObject(hdc_tmp, HGDIOBJ(font.0));
        SetBkMode(hdc_tmp, TRANSPARENT);
        SetTextColor(hdc_tmp, COLORREF(0x00FF_FFFF)); // white text

        let mut rect = windows::Win32::Foundation::RECT {
            left: 0,
            top: 0,
            right: rw,
            bottom: rh,
        };
        let mut tw: Vec<u16> = text.encode_utf16().chain(std::iter::once(0)).collect();
        let len = tw.len() - 1;
        DrawTextW(
            hdc_tmp,
            &mut tw[..len],
            &mut rect,
            DT_LEFT | DT_SINGLELINE | DT_VCENTER | DT_NOCLIP,
        );

        SelectObject(hdc_tmp, old_fn);
        SelectObject(hdc_tmp, old_bm);
        let _ = DeleteDC(hdc_tmp);

        // Composite coverage pixels into main DIB.
        for (i, &px) in tmp.iter().enumerate() {
            let cov = (px >> 16 & 0xFF).max(px >> 8 & 0xFF).max(px & 0xFF) as u8;
            if cov == 0 {
                continue;
            }
            let sa = (cov as f32 / 255.0 * alpha * 255.0) as u8;
            if sa == 0 {
                continue;
            }
            let sx = dx + (i as i32 % rw);
            let sy = dy + (i as i32 / rw);
            if sx < 0 || sy < 0 || sx >= dw || sy >= dh {
                continue;
            }
            let di = (sy * dw + sx) as usize;
            pix[di] = blend_over(premult(tr, tg, tb, sa), pix[di]);
        }

        let _ = DeleteObject(HGDIOBJ(hbm.0));
    }

    // ── Frame rendering ───────────────────────────────────────────────────────

    #[cfg(target_os = "windows")]
    unsafe fn render_frame(hwnd: HWND, state: &mut OverlayState) {
        state.ensure_fonts();
        let Some(font_normal) = state.hfont_normal else {
            return;
        };
        let Some(font_bold) = state.hfont_bold else {
            return;
        };

        let w = state.window_w;
        let h = if state.deck.is_empty() {
            1
        } else {
            state.window_height()
        };

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
        pix.fill(0); // fully transparent canvas

        // Pre-populate the scaled-font cache for all history card sizes so the
        // render loop can look up without needing &mut state inside the iter.
        let n = state.deck.len();
        {
            let font_size = state.font_size;
            let font_name = state.font_name.clone();
            for card in &state.deck {
                let card_h =
                    ((HISTORY_H as f32 * card.cur_scale).round() as i32).max(CORNER_R * 2 + 2);
                // Quadratic falloff so font shrinks much faster than card scale alone.
                let desired_pt =
                    (font_size as f32 * card.cur_scale * card.cur_scale).round() as i32;
                // Hard cap: font height ≤ 70% of card pixel height (convert px→pt at 96 DPI).
                let max_pt = (card_h as f32 * 0.70 * 72.0 / 96.0).round() as i32;
                let pt = desired_pt.min(max_pt).max(6);
                state
                    .scaled_font_cache
                    .entry(pt)
                    .or_insert_with(|| make_font(&font_name, pt, false));
            }
        }
        let font_size = state.font_size;
        let font_cache = &state.scaled_font_cache;

        for (i, card) in state.deck.iter().enumerate() {
            let is_spot = i + 1 == n;
            let alpha = card.cur_alpha.clamp(0.0, 1.0);
            if alpha < 0.005 {
                continue;
            }

            let card_h = if is_spot {
                SPOTLIGHT_H
            } else {
                ((HISTORY_H as f32 * card.cur_scale).round() as i32).max(CORNER_R * 2 + 2)
            };
            let cy = card.cur_y as i32;
            let inset = if is_spot {
                0
            } else {
                let card_w = (w as f32 * card.cur_scale).round() as i32;
                ((w - card_w) / 2).max(0)
            };

            // Resolve text color early so accent bar can share it.
            let text_rgb = if let Some(c) = parse_hex_color(&card.event.message_color) {
                (
                    (c >> 16 & 0xFF) as u8,
                    (c >> 8 & 0xFF) as u8,
                    (c & 0xFF) as u8,
                )
            } else {
                card.category.text_rgb()
            };

            // ── Background panel ──────────────────────────────────────────
            let (br, bg, bb, ba) = CARD_BG;
            fill_rrect(
                pix,
                w,
                h,
                inset,
                cy,
                w - inset,
                cy + card_h,
                CORNER_R,
                premult(br, bg, bb, (ba as f32 * alpha) as u8),
            );

            // ── Left accent bar (matches font color) ──────────────────────
            let (ar, ag, ab) = text_rgb;
            fill_rect(
                pix,
                w,
                h,
                inset,
                cy + CORNER_R,
                inset + ACCENT_W,
                cy + card_h - CORNER_R,
                premult(ar, ag, ab, (255.0 * alpha) as u8),
            );

            // ── Icon swatch ───────────────────────────────────────────────
            let icon_sz = if is_spot { ICON_SZ_SPOT } else { ICON_SZ_HIST };
            let ix = inset + ACCENT_W + CARD_PAD_X;
            let iy = cy + (card_h - icon_sz) / 2;
            let icon_name = &card.event.icon;
            let drew_png = if icon_name.ends_with(".png") || icon_name.ends_with(".jpg") {
                if let Some(icon_pix) = state
                    .icon_cache
                    .entry(format!("{}@{}", icon_name, icon_sz))
                    .or_insert_with(|| load_icon_pixels_premult(icon_name, icon_sz))
                    .as_ref()
                    .map(|v| v.as_slice())
                {
                    composite_icon(pix, w, h, icon_pix, icon_sz, ix, iy, alpha);
                    true
                } else {
                    false
                }
            } else {
                false
            };
            if !drew_png {
                // Fallback: coloured rounded square using event.color or category accent.
                let (ir, ig, ib) = if let Some(c) = parse_hex_color(&card.event.color) {
                    (
                        (c >> 16 & 0xFF) as u8,
                        (c >> 8 & 0xFF) as u8,
                        (c & 0xFF) as u8,
                    )
                } else {
                    card.category.accent()
                };
                fill_rrect(
                    pix,
                    w,
                    h,
                    ix,
                    iy,
                    ix + icon_sz,
                    iy + icon_sz,
                    2,
                    premult(ir, ig, ib, (200.0 * alpha) as u8),
                );
            }

            // ── Message text ──────────────────────────────────────────────
            let tx = ix + icon_sz + ICON_TEXT_GAP;
            let font = if is_spot {
                font_bold
            } else {
                let desired_pt =
                    (font_size as f32 * card.cur_scale * card.cur_scale).round() as i32;
                let max_pt = (card_h as f32 * 0.70 * 72.0 / 96.0).round() as i32;
                let pt = desired_pt.min(max_pt).max(6);
                *font_cache.get(&pt).unwrap_or(&font_normal)
            };
            composite_text(
                pix,
                w,
                h,
                &card.event.message,
                font,
                text_rgb,
                alpha,
                tx,
                cy,
                w - tx - CARD_PAD_X,
                card_h,
            );
        }

        let new_top = state.anchor_bottom_y - h;
        let dst_pt = POINT {
            x: state.window_x,
            y: new_top,
        };
        let win_size = SIZE { cx: w, cy: h };
        let src_pt = POINT { x: 0, y: 0 };
        let blend = BLENDFUNCTION {
            BlendOp: 0, // AC_SRC_OVER
            BlendFlags: 0,
            SourceConstantAlpha: 255,
            AlphaFormat: 1, // AC_SRC_ALPHA — use per-pixel alpha from DIB
        };
        let _ = UpdateLayeredWindow(
            hwnd,
            hdc_screen,
            Some(&dst_pt), // pin bottom: move top up as height grows
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

    pub fn spawn_overlay(
        cfg: Arc<Mutex<Config>>,
        queue: Arc<Mutex<Vec<OverlayEvent>>>,
        quit: Arc<AtomicBool>,
    ) {
        std::thread::Builder::new()
            .name("froklog-overlay".into())
            .spawn(move || run_overlay_thread(cfg, queue, quit))
            .expect("spawn overlay thread");
    }

    #[cfg(target_os = "windows")]
    fn run_overlay_thread(
        cfg: Arc<Mutex<Config>>,
        queue: Arc<Mutex<Vec<OverlayEvent>>>,
        quit: Arc<AtomicBool>,
    ) {
        unsafe {
            // COM is required for SAPI TTS (ISpVoice).
            let _ = windows::Win32::System::Com::CoInitializeEx(
                None,
                windows::Win32::System::Com::COINIT_APARTMENTTHREADED,
            );

            let hmodule = GetModuleHandleW(PCWSTR::null()).unwrap_or_default();
            let hinstance = windows::Win32::Foundation::HINSTANCE(hmodule.0);
            let class_w: Vec<u16> = CLASS_NAME.encode_utf16().collect();

            let wc = WNDCLASSEXW {
                cbSize: mem::size_of::<WNDCLASSEXW>() as u32,
                style: CS_HREDRAW | CS_VREDRAW,
                lpfnWndProc: Some(overlay_wnd_proc),
                hInstance: hinstance,
                lpszClassName: PCWSTR(class_w.as_ptr()),
                hCursor: LoadCursorW(None, IDC_ARROW).unwrap_or_default(),
                hbrBackground: HBRUSH(std::ptr::null_mut()),
                ..Default::default()
            };
            let _ = RegisterClassExW(&wc);

            let (wx, anchor_bottom) = initial_position(&cfg.lock().unwrap());
            let state = Box::new(OverlayState::new(&cfg, queue, quit, wx, anchor_bottom));
            let state_ptr = Box::into_raw(state);

            let hwnd = CreateWindowExW(
                WS_EX_TOPMOST | WS_EX_LAYERED | WS_EX_TOOLWINDOW | WS_EX_NOACTIVATE,
                PCWSTR(class_w.as_ptr()),
                PCWSTR::null(),
                WS_POPUP | WS_VISIBLE,
                wx,
                anchor_bottom - 1, // 1px initial window; top = anchor - height
                WINDOW_WIDTH,
                1,
                None,
                None,
                hinstance,
                Some(state_ptr as *const c_void),
            )
            .expect("CreateWindowExW overlay");

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

            windows::Win32::System::Com::CoUninitialize();
        }
    }

    #[cfg(not(target_os = "windows"))]
    fn run_overlay_thread(
        _cfg: Arc<Mutex<Config>>,
        _queue: Arc<Mutex<Vec<OverlayEvent>>>,
        _quit: Arc<AtomicBool>,
    ) {
    }

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
            SetWindowLongPtrW(hwnd, GWLP_USERDATA, cs.lpCreateParams as isize);
            return LRESULT(0);
        }

        let ptr = GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *mut OverlayState;
        if ptr.is_null() {
            return DefWindowProcW(hwnd, msg, wparam, lparam);
        }
        let state = &mut *ptr;

        match msg {
            WM_TIMER => {
                if state.quit.load(Ordering::Relaxed) {
                    let _ = DestroyWindow(hwnd);
                    return LRESULT(0);
                }
                state.sync_config();
                state.drain_queue();
                state.tick_animate();
                render_frame(hwnd, state);
                LRESULT(0)
            }

            WM_PAINT => {
                // UpdateLayeredWindow owns the surface; validate to silence WM_PAINT loops.
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
                    state.anchor_bottom_y = rect.bottom;
                    let mut cfg = state.config.lock().unwrap();
                    cfg.overlay_x = rect.left;
                    cfg.overlay_y = rect.bottom; // store bottom anchor so grow-up is stable on restart
                    cfg.save();
                }
                LRESULT(0)
            }

            WM_DESTROY => {
                state.drop_fonts();
                state.drop_icon_cache();
                drop(Box::from_raw(ptr));
                SetWindowLongPtrW(hwnd, GWLP_USERDATA, 0);
                PostQuitMessage(0);
                LRESULT(0)
            }

            _ => DefWindowProcW(hwnd, msg, wparam, lparam),
        }
    }

    // ── Utilities ─────────────────────────────────────────────────────────────

    #[cfg(target_os = "windows")]
    unsafe fn make_font(name: &str, pt_size: i32, bold: bool) -> HFONT {
        let height = -(pt_size * 96 / 72);
        let weight = if bold {
            FW_BOLD.0 as i32
        } else {
            FW_NORMAL.0 as i32
        };
        let nw: Vec<u16> = name.encode_utf16().chain(std::iter::once(0)).collect();
        let mut face = [0u16; 32];
        let n = nw.len().min(31);
        face[..n].copy_from_slice(&nw[..n]);
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

    // ── TTS helpers ───────────────────────────────────────────────────────────

    #[cfg(target_os = "windows")]
    fn tts_speed_to_rate(speed: &crate::config::TtsSpeed) -> i32 {
        match speed {
            crate::config::TtsSpeed::Normal => 0,
            crate::config::TtsSpeed::Fast => 2,
            crate::config::TtsSpeed::Faster => 4,
            crate::config::TtsSpeed::Fastest => 7,
        }
    }

    /// Enumerate installed SAPI voices. Returns (display_name, token_key_id) pairs.
    /// Reads from HKLM\SOFTWARE\Microsoft\Speech\Voices\Tokens via the registry.
    #[cfg(target_os = "windows")]
    pub fn enumerate_tts_voices() -> Vec<(String, String)> {
        use windows::Win32::System::Registry::{
            RegCloseKey, RegEnumKeyExW, RegOpenKeyExW, RegQueryValueExW, HKEY, HKEY_LOCAL_MACHINE,
            KEY_READ, REG_VALUE_TYPE,
        };

        let mut voices = vec![("System Default".to_string(), String::new())];

        let base = windows::core::w!("SOFTWARE\\Microsoft\\Speech\\Voices\\Tokens");
        let mut hroot: HKEY = Default::default();
        if unsafe { RegOpenKeyExW(HKEY_LOCAL_MACHINE, base, 0, KEY_READ, &mut hroot) }.is_err() {
            return voices;
        }

        let mut i = 0u32;
        loop {
            let mut name_buf = [0u16; 256];
            let mut name_len = 256u32;
            let res = unsafe {
                RegEnumKeyExW(
                    hroot,
                    i,
                    windows::core::PWSTR(name_buf.as_mut_ptr()),
                    &mut name_len,
                    None,
                    windows::core::PWSTR(std::ptr::null_mut()),
                    None,
                    None,
                )
            };
            if res.is_err() {
                break;
            }
            let key_name = String::from_utf16_lossy(&name_buf[..name_len as usize]);

            // Try to read Attributes\Name for a friendly display name.
            let attr_path = format!(
                "SOFTWARE\\Microsoft\\Speech\\Voices\\Tokens\\{}\\Attributes",
                key_name
            );
            let attr_path_w: Vec<u16> =
                attr_path.encode_utf16().chain(std::iter::once(0)).collect();
            let mut hattr: HKEY = Default::default();
            let display = if unsafe {
                RegOpenKeyExW(
                    HKEY_LOCAL_MACHINE,
                    windows::core::PCWSTR(attr_path_w.as_ptr()),
                    0,
                    KEY_READ,
                    &mut hattr,
                )
            }
            .is_ok()
            {
                let name_val_w: Vec<u16> = "Name\0".encode_utf16().collect();
                let mut val_buf = [0u16; 256];
                let mut val_len = (val_buf.len() * 2) as u32;
                let mut val_type = REG_VALUE_TYPE(0);
                let friendly = if unsafe {
                    RegQueryValueExW(
                        hattr,
                        windows::core::PCWSTR(name_val_w.as_ptr()),
                        None,
                        Some(&mut val_type),
                        Some(val_buf.as_mut_ptr() as *mut u8),
                        Some(&mut val_len),
                    )
                }
                .is_ok()
                {
                    let chars = (val_len as usize / 2).saturating_sub(1);
                    String::from_utf16_lossy(&val_buf[..chars])
                } else {
                    key_name.clone()
                };
                unsafe {
                    let _ = RegCloseKey(hattr);
                }
                friendly
            } else {
                key_name.clone()
            };

            if !display.is_empty() {
                voices.push((display, key_name));
            }
            i += 1;
        }
        unsafe {
            let _ = RegCloseKey(hroot);
        }
        voices
    }

    #[cfg(target_os = "windows")]
    impl OverlayState {
        /// Speak `text` via SAPI, honouring global TTS config and per-alert priority.
        unsafe fn try_speak(
            &mut self,
            text: &str,
            priority: &crate::triggers::engine::VoicePriority,
        ) {
            use crate::config::TtsAudioMode;
            use crate::triggers::engine::VoicePriority;
            use windows::Win32::Media::Speech::{ISpVoice, SpVoice};
            use windows::Win32::System::Com::{CoCreateInstance, CLSCTX_ALL};

            // Read current TTS settings.
            let (enabled, speed, mode, re, ro, ra, voice_id) = {
                let cfg = self.config.lock().unwrap();
                (
                    cfg.tts_enabled,
                    cfg.tts_speed.clone(),
                    cfg.tts_audio_mode.clone(),
                    cfg.tts_read_emergency,
                    cfg.tts_read_operational,
                    cfg.tts_read_ambient,
                    cfg.tts_voice.clone(),
                )
            };

            if !enabled {
                return;
            }
            match priority {
                VoicePriority::Emergency if !re => return,
                VoicePriority::Operational if !ro => return,
                VoicePriority::Ambient if !ra => return,
                _ => {}
            }

            // Lazily create the ISpVoice COM object.
            if self.tts_voice.is_none() {
                if let Ok(voice) = CoCreateInstance::<_, ISpVoice>(&SpVoice, None, CLSCTX_ALL) {
                    if !voice_id.is_empty() {
                        apply_tts_voice_by_id(&voice, &voice_id);
                    }
                    let rate = tts_speed_to_rate(&speed);
                    let _ = voice.SetRate(rate);
                    self.tts_rate = rate;
                    self.tts_voice = Some(voice);
                }
            }

            let Some(ref voice) = self.tts_voice else {
                return;
            };

            // Sync rate if the user changed it since last call.
            let new_rate = tts_speed_to_rate(&speed);
            if new_rate != self.tts_rate {
                self.tts_rate = new_rate;
                let _ = voice.SetRate(new_rate);
            }

            // Determine speak flags: SPF_ASYNC=1, SPF_PURGEBEFORESPEAK=2.
            let flags: u32 = match mode {
                TtsAudioMode::InterruptConstantly => 1 | 2,
                TtsAudioMode::QueueAll => 1,
                TtsAudioMode::SmartPriority => match priority {
                    VoicePriority::Emergency => 1 | 2,
                    VoicePriority::Ambient => {
                        // Suppress if voice is already speaking.
                        if is_sapi_voice_speaking(voice) {
                            return;
                        }
                        1
                    }
                    _ => 1,
                },
            };

            let wide: Vec<u16> = text.encode_utf16().chain(std::iter::once(0u16)).collect();
            let _ = voice.Speak(windows::core::PCWSTR(wide.as_ptr()), flags, None);
        }
    }

    #[cfg(target_os = "windows")]
    unsafe fn is_sapi_voice_speaking(voice: &windows::Win32::Media::Speech::ISpVoice) -> bool {
        use windows::Win32::Media::Speech::{SPRS_DONE, SPVOICESTATUS};
        let mut status = std::mem::zeroed::<SPVOICESTATUS>();
        if voice.GetStatus(&mut status, std::ptr::null_mut()).is_ok() {
            status.dwRunningState != SPRS_DONE.0 as u32
        } else {
            false
        }
    }

    #[cfg(target_os = "windows")]
    unsafe fn apply_tts_voice_by_id(
        voice: &windows::Win32::Media::Speech::ISpVoice,
        voice_id: &str,
    ) {
        use windows::Win32::Media::Speech::{
            ISpObjectToken, ISpObjectTokenCategory, SpObjectTokenCategory, SPCAT_VOICES,
        };
        use windows::Win32::System::Com::{CoCreateInstance, CoTaskMemFree, CLSCTX_ALL};

        let Ok(cat) =
            CoCreateInstance::<_, ISpObjectTokenCategory>(&SpObjectTokenCategory, None, CLSCTX_ALL)
        else {
            return;
        };
        if cat.SetId(SPCAT_VOICES, false).is_err() {
            return;
        }
        let Ok(enum_tokens) = cat.EnumTokens(
            windows::core::PCWSTR(std::ptr::null()),
            windows::core::PCWSTR(std::ptr::null()),
        ) else {
            return;
        };

        loop {
            let mut token: Option<ISpObjectToken> = None;
            if enum_tokens.Next(1, &mut token, None).is_err() {
                break;
            }
            let Some(tok) = token else { break };

            if let Ok(id_ptr) = tok.GetId() {
                // id_ptr is a full registry path; compare just the final component.
                let full = id_ptr.to_string().unwrap_or_default();
                CoTaskMemFree(Some(id_ptr.0 as *mut _));
                let token_key = full.rsplit('\\').next().unwrap_or("");
                if token_key == voice_id || full.contains(voice_id) {
                    let _ = voice.SetVoice(&tok);
                    break;
                }
            }
        }
    }

    fn play_sound(path: &str) {
        #[cfg(target_os = "windows")]
        {
            let resolved = resolve_sound_path(path);
            std::thread::spawn(move || unsafe {
                let pw: Vec<u16> = resolved.encode_utf16().chain(std::iter::once(0)).collect();
                let _ = windows::Win32::Media::Audio::PlaySoundW(
                    PCWSTR(pw.as_ptr()),
                    None,
                    windows::Win32::Media::Audio::SND_FILENAME
                        | windows::Win32::Media::Audio::SND_ASYNC,
                );
            });
        }
    }

    fn resolve_sound_path(s: &str) -> String {
        if s.starts_with("sounds/") || s.starts_with("sounds\\") {
            if let Ok(exe) = std::env::current_exe() {
                if let Some(dir) = exe.parent() {
                    let p = dir.join(s);
                    if p.exists() {
                        return p.to_string_lossy().into_owned();
                    }
                }
            }
        }
        s.to_string()
    }

    /// Returns `(x, anchor_bottom_y)`.  The overlay window bottom is pinned at
    /// `anchor_bottom_y` and grows upward as cards are added.
    fn initial_position(cfg: &Config) -> (i32, i32) {
        #[cfg(target_os = "windows")]
        {
            if cfg.overlay_x >= 0 && cfg.overlay_y >= 0 {
                // overlay_y was saved as rect.bottom (the bottom anchor).
                return (cfg.overlay_x, cfg.overlay_y);
            }
            unsafe {
                let sw = GetSystemMetrics(SM_CXSCREEN);
                let sh = GetSystemMetrics(SM_CYSCREEN);
                // Default anchor: 3/4 down the screen + one-card height
                let anchor = sh * 3 / 4 + SPOTLIGHT_H + CARD_PAD_V * 2;
                ((sw - WINDOW_WIDTH) / 2, anchor)
            }
        }
        #[cfg(not(target_os = "windows"))]
        (cfg.overlay_x.max(0), cfg.overlay_y.max(1))
    }

    /// Parse `#RRGGBB` or `RRGGBB` → `0x00RRGGBB`.
    fn parse_hex_color(s: &str) -> Option<u32> {
        let s = s.trim_start_matches('#');
        if s.len() == 6 {
            let r = u32::from_str_radix(&s[0..2], 16).ok()?;
            let g = u32::from_str_radix(&s[2..4], 16).ok()?;
            let b = u32::from_str_radix(&s[4..6], 16).ok()?;
            Some((r << 16) | (g << 8) | b)
        } else {
            None
        }
    }

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
