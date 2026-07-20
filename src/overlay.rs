/// Fly-in alert overlay window.
///
/// Per-pixel-alpha `WS_EX_LAYERED` Win32 popup rendered into a 32-bpp
/// premultiplied-BGRA DIB and pushed to the screen via `UpdateLayeredWindow`,
/// structurally a twin of `overlay_dps.rs`'s independent meter window.
///
/// Unlike the old stacked-deck design, this window shows **one message at a
/// time**: it flies in from a small font size to a large "peak" size
/// (`FlyIn`), optionally applies a per-trigger visual treatment while held at
/// peak size (`Hold`), then shrinks back down while translating toward the
/// history overlay's position (`ShrinkOut`) before handing the message off to
/// `AppHandle.overlay_history` for `overlay_history.rs` to display. Additional
/// events arriving while one is in flight normally queue up (FIFO) and start
/// as soon as the window returns to `Idle`, but `OverlayEvent.priority` can
/// change that: `Emergency` interrupts whatever's currently showing and jumps
/// the queue immediately (the interrupted event is requeued to resume next),
/// and `Ambient` events are dropped outright once the queue is already
/// backed up (`AMBIENT_DROP_QUEUE_LEN`). The `Hold` phase also shortens itself
/// when a backlog exists, so queued messages don't wait behind a full-length
/// hold on the current one.
#[cfg(feature = "tray")]
#[allow(clippy::module_inception)]
pub mod overlay {
    use std::ffi::c_void;
    use std::mem;
    use std::sync::atomic::{AtomicBool, AtomicU8, Ordering};
    use std::sync::{Arc, Mutex};
    use std::time::{Duration, Instant};

    use crate::config::Config;
    #[cfg(target_os = "windows")]
    use crate::overlay_draw::overlay_draw::{
        apply_lock_style, composite_text_stroked, make_font, premult,
    };
    use crate::overlay_draw::overlay_draw::{blend_over, fill_rrect, parse_hex_color};
    use crate::overlay_history::overlay_history::HistoryEntry;
    use crate::tray::tray::AppHandle;
    use crate::triggers::engine::{OverlayEvent, Treatment, VoicePriority};

    // ── Win32 imports ─────────────────────────────────────────────────────────

    #[cfg(target_os = "windows")]
    use windows::Win32::Foundation::{COLORREF, HWND, LPARAM, LRESULT, POINT, SIZE, WPARAM};

    #[cfg(target_os = "windows")]
    use windows::Win32::Graphics::Gdi::{
        BeginPaint, CreateCompatibleDC, CreateDIBSection, DeleteDC, DeleteObject, EndPaint, GetDC,
        ReleaseDC, SelectObject, BITMAPINFO, BITMAPINFOHEADER, BLENDFUNCTION, DIB_RGB_COLORS,
        DT_LEFT, DT_NOCLIP, HBRUSH, HFONT, HGDIOBJ, PAINTSTRUCT,
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
    const TIMER_ANIM: usize = 1;
    const ANIM_INTERVAL_MS: u32 = 16;

    const WINDOW_WIDTH: i32 = 760;
    const PAD_H: i32 = 18;
    const PAD_V: i32 = 12;
    const CORNER_R: i32 = 10;
    const ICON_TEXT_GAP: i32 = 14;

    /// Vertical rise (px) during fly-in, easing to 0 as the message settles.
    const RISE_PX: f32 = 26.0;
    /// Fraction of the distance toward the history window's anchor travelled
    /// during shrink-out (the remainder is covered by the fade completing).
    const TRAVEL_FRACTION: f32 = 0.35;
    const GLOW_LAYERS: i32 = 3;
    const VIBRATE_PX: f32 = 3.0;
    const PULSE_AMOUNT: f32 = 0.07;

    /// Ambient-priority events are dropped instead of queued once this many
    /// events are already waiting — stale low-priority info showing late is
    /// worse than not showing at all.
    const AMBIENT_DROP_QUEUE_LEN: usize = 2;
    /// Hold duration never shrinks below this when a backlog is draining it.
    const MIN_HOLD_SECS: f32 = 0.6;

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
    }

    /// Default text fill/stroke when a trigger doesn't override them.
    const DEFAULT_TEXT_RGB: (u8, u8, u8) = (255, 255, 255);
    const DEFAULT_BORDER_RGB: (u8, u8, u8) = (0, 0, 0);

    // ── Alert lifecycle ────────────────────────────────────────────────────────

    #[derive(Clone, Copy, PartialEq)]
    enum AlertPhase {
        FlyIn,
        Hold,
        ShrinkOut,
    }

    struct ActiveAlert {
        event: OverlayEvent,
        category: EventCategory,
        phase: AlertPhase,
        phase_started: Instant,
        /// True for the synthetic alert injected by the Settings dialog's
        /// "Show All Windows" button — held in `Hold` indefinitely (instead
        /// of aging out to `ShrinkOut` and landing in history) for as long as
        /// `force_show` stays set.
        is_placeholder: bool,
    }

    // ── State ─────────────────────────────────────────────────────────────────

    struct OverlayState {
        handle: Arc<AppHandle>,
        queue: Vec<OverlayEvent>,
        active: Option<ActiveAlert>,
        overlay_enabled: bool,
        start_pt: i32,
        max_pt: i32,
        fly_ms: u32,
        hold_secs: f32,
        alpha: u8,
        font_name: String,
        window_x: i32,
        anchor_center_y: i32,
        visible: bool,
        locked: bool,
        /// Mirrors `AppHandle.force_show_windows` — forces this window
        /// visible (via a placeholder alert) so it can be dragged into
        /// position from the Settings dialog even with no real alert queued.
        force_show: bool,
        #[cfg(target_os = "windows")]
        font_cache: std::collections::HashMap<i32, HFONT>,
        /// Premultiplied BGRA pixel cache keyed by `"filename@size"`.
        icon_cache: std::collections::HashMap<String, Option<Vec<u32>>>,
        #[cfg(target_os = "windows")]
        tts_voice: Option<windows::Win32::Media::Speech::ISpVoice>,
        #[cfg(target_os = "windows")]
        tts_rate: i32,
    }

    #[cfg(target_os = "windows")]
    impl OverlayState {
        fn new(handle: &Arc<AppHandle>, wx: i32, anchor_center_y: i32) -> Self {
            let cfg = handle.config.lock().unwrap();
            Self {
                handle: Arc::clone(handle),
                queue: Vec::new(),
                active: None,
                overlay_enabled: cfg.overlay_enabled,
                start_pt: cfg.overlay_start_font_size.max(6) as i32,
                max_pt: cfg.overlay_max_font_size.max(6) as i32,
                fly_ms: cfg.overlay_fly_ms.max(16),
                hold_secs: cfg.overlay_hold_secs.max(0.0),
                alpha: cfg.overlay_alpha,
                font_name: if cfg.overlay_font.is_empty() {
                    "Segoe UI".to_string()
                } else {
                    cfg.overlay_font.clone()
                },
                window_x: wx,
                anchor_center_y,
                visible: false,
                locked: cfg.overlay_locked,
                force_show: handle.force_show_windows.load(Ordering::Relaxed),
                font_cache: std::collections::HashMap::new(),
                icon_cache: std::collections::HashMap::new(),
                tts_voice: None,
                tts_rate: 0,
            }
        }

        unsafe fn font_for(&mut self, pt: i32) -> HFONT {
            let name = self.font_name.clone();
            *self
                .font_cache
                .entry(pt)
                .or_insert_with(|| make_font(&name, pt, true))
        }

        unsafe fn drop_fonts(&mut self) {
            for f in self.font_cache.drain().map(|(_, f)| f) {
                let _ = DeleteObject(HGDIOBJ(f.0));
            }
        }

        fn drop_icon_cache(&mut self) {
            self.icon_cache.clear();
        }

        /// Reload live-tunable settings from config. Returns true if the
        /// click-through lock state changed and needs to be re-applied to the
        /// window style (covers both the in-window pin click and the settings
        /// dialog's checkbox — the window can't receive clicks while locked,
        /// so this poll is the only way an unlock via the dialog takes effect).
        unsafe fn sync_config(&mut self) -> bool {
            let cfg = self.handle.config.lock().unwrap();
            let new_font = if cfg.overlay_font.is_empty() {
                "Segoe UI".to_string()
            } else {
                cfg.overlay_font.clone()
            };
            let new_voice_id = cfg.tts_voice.clone();
            self.overlay_enabled = cfg.overlay_enabled;
            self.start_pt = cfg.overlay_start_font_size.max(6) as i32;
            self.max_pt = cfg.overlay_max_font_size.max(6) as i32;
            self.fly_ms = cfg.overlay_fly_ms.max(16);
            self.hold_secs = cfg.overlay_hold_secs.max(0.0);
            self.alpha = cfg.overlay_alpha;
            let lock_changed = cfg.overlay_locked != self.locked;
            self.locked = cfg.overlay_locked;
            drop(cfg);
            self.force_show = self.handle.force_show_windows.load(Ordering::Relaxed);

            if new_font != self.font_name {
                self.font_name = new_font;
                self.drop_fonts();
            }

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
            lock_changed
        }

        fn drain_queue(&mut self) {
            let new_events: Vec<OverlayEvent> = {
                let mut q = self.handle.overlay_queue.lock().unwrap();
                q.drain(..).collect()
            };
            for ev in new_events {
                #[cfg(target_os = "windows")]
                if let Some(ref text) = ev.tts_text {
                    unsafe { self.try_speak(text, &ev.tts_priority) };
                }

                if !ev.message.is_empty() {
                    match ev.priority {
                        VoicePriority::Emergency => {
                            // Interrupt whatever's currently showing — it goes back to
                            // the front of the queue to resume (from scratch) right
                            // after this one, rather than being lost.
                            if let Some(active) = self.active.take() {
                                self.queue.insert(0, active.event);
                            }
                            self.queue.insert(0, ev);
                        }
                        VoicePriority::Ambient if self.queue.len() >= AMBIENT_DROP_QUEUE_LEN => {
                            // Backed up already — drop rather than pile on stale info.
                        }
                        VoicePriority::Ambient | VoicePriority::Operational => {
                            self.queue.push(ev);
                        }
                    }
                } else if let Some(s) = ev.sound.as_deref().filter(|s| !s.is_empty()) {
                    play_sound_label(s);
                }
            }
        }

        /// Advance the single-message state machine one tick. Returns true if
        /// a message finished its lifecycle and was handed off to history.
        fn tick_state(&mut self) {
            let now = Instant::now();

            // Placeholder injected for "Show All Windows": drop it the moment
            // force_show clears, and otherwise keep it pinned in Hold forever
            // instead of letting it age through ShrinkOut into history.
            if let Some(active) = self.active.as_mut() {
                if active.is_placeholder {
                    if self.force_show {
                        active.phase = AlertPhase::Hold;
                        active.phase_started = now;
                    } else {
                        self.active = None;
                    }
                    return;
                }
            }

            if self.active.is_none() {
                if !self.queue.is_empty() {
                    let event = self.queue.remove(0);
                    if let Some(s) = event.sound.as_deref() {
                        if !s.is_empty() {
                            play_sound_label(s);
                        }
                    }
                    let category = EventCategory::from_icon(&event.icon);
                    self.active = Some(ActiveAlert {
                        event,
                        category,
                        phase: AlertPhase::FlyIn,
                        phase_started: now,
                        is_placeholder: false,
                    });
                } else if self.force_show {
                    self.active = Some(ActiveAlert {
                        event: OverlayEvent {
                            icon: String::new(),
                            color: String::new(),
                            message: "Drag to position — Alert Overlay".to_string(),
                            message_color: String::new(),
                            border_color: String::new(),
                            sound: None,
                            tts_text: None,
                            tts_priority: VoicePriority::default(),
                            treatment: Treatment::default(),
                            priority: VoicePriority::default(),
                        },
                        category: EventCategory::System,
                        phase: AlertPhase::Hold,
                        phase_started: now,
                        is_placeholder: true,
                    });
                }
                return;
            }

            let fly_dur = Duration::from_millis(self.fly_ms as u64);

            let active = self.active.as_ref().unwrap();
            let elapsed = now.duration_since(active.phase_started);
            let phase = active.phase;
            let dur = match phase {
                AlertPhase::FlyIn | AlertPhase::ShrinkOut => fly_dur,
                // Shorten the hold when a backlog is waiting, so queued messages
                // don't sit behind a full-length hold on the current one.
                AlertPhase::Hold => self.effective_hold_duration(),
            };
            if elapsed < dur {
                return;
            }

            match phase {
                AlertPhase::FlyIn => {
                    let active = self.active.as_mut().unwrap();
                    active.phase = AlertPhase::Hold;
                    active.phase_started = now;
                }
                AlertPhase::Hold => {
                    let active = self.active.as_mut().unwrap();
                    active.phase = AlertPhase::ShrinkOut;
                    active.phase_started = now;
                }
                AlertPhase::ShrinkOut => {
                    let done = self.active.take().unwrap();
                    let mut hist = self.handle.overlay_history.lock().unwrap();
                    hist.push(HistoryEntry::new(
                        done.event.icon,
                        done.event.color,
                        done.event.message,
                        done.event.message_color,
                        done.event.border_color,
                    ));
                }
            }
        }

        /// Hold duration for the current message, shortened when other
        /// messages are already waiting so the queue drains faster. Never
        /// drops below `MIN_HOLD_SECS` so a message is always readable.
        fn effective_hold_duration(&self) -> Duration {
            let backlog = self.queue.len();
            if backlog == 0 {
                return Duration::from_secs_f32(self.hold_secs);
            }
            let scaled = self.hold_secs / (1.0 + backlog as f32 * 0.5);
            let floor = MIN_HOLD_SECS.min(self.hold_secs);
            Duration::from_secs_f32(scaled.clamp(floor, self.hold_secs))
        }
    }

    // ── Animation math ────────────────────────────────────────────────────────

    /// Ease-out-cubic: `t` in `[0, 1]` -> eased progress in `[0, 1]`.
    fn ease_out_cubic(t: f32) -> f32 {
        let t = t.clamp(0.0, 1.0);
        1.0 - (1.0 - t).powi(3)
    }

    /// Everything needed to draw one frame of the active alert, resolved from
    /// its current phase/elapsed time.
    struct FrameParams {
        font_pt: i32,
        alpha: f32,
        x_offset: f32,
        y_offset: f32,
        glow: bool,
        glow_pulse: f32,
    }

    #[allow(clippy::too_many_arguments)]
    fn resolve_frame(
        active: &ActiveAlert,
        start_pt: i32,
        max_pt: i32,
        fly_ms: u32,
        travel_dx: f32,
        travel_dy: f32,
        now: Instant,
    ) -> FrameParams {
        let elapsed = now.duration_since(active.phase_started);
        let fly_ms = fly_ms.max(1) as f32;
        match active.phase {
            AlertPhase::FlyIn => {
                let t = (elapsed.as_secs_f32() * 1000.0) / fly_ms;
                let ease = ease_out_cubic(t);
                FrameParams {
                    font_pt: (start_pt as f32 + (max_pt - start_pt) as f32 * ease).round() as i32,
                    alpha: ease,
                    x_offset: 0.0,
                    y_offset: (1.0 - ease) * RISE_PX,
                    glow: false,
                    glow_pulse: 0.0,
                }
            }
            AlertPhase::Hold => {
                let t = elapsed.as_secs_f32();
                let treatment = active.event.treatment;
                let mut font_pt = max_pt;
                let mut x_offset = 0.0f32;
                let mut y_offset = 0.0f32;
                let mut glow = false;
                let mut glow_pulse = 0.0f32;
                match treatment {
                    Treatment::None => {}
                    Treatment::Glow => {
                        glow = true;
                        glow_pulse = 0.5 + 0.5 * (t * 2.0 * std::f32::consts::PI * 0.8).sin();
                    }
                    Treatment::Vibrate => {
                        x_offset = (t * 2.0 * std::f32::consts::PI * 13.0).sin() * VIBRATE_PX;
                        y_offset = (t * 2.0 * std::f32::consts::PI * 17.0).cos() * VIBRATE_PX;
                    }
                    Treatment::Pulse => {
                        let wobble = (t * 2.0 * std::f32::consts::PI * 1.6).sin();
                        font_pt = (max_pt as f32 * (1.0 + PULSE_AMOUNT * wobble)).round() as i32;
                    }
                }
                FrameParams {
                    font_pt,
                    alpha: 1.0,
                    x_offset,
                    y_offset,
                    glow,
                    glow_pulse,
                }
            }
            AlertPhase::ShrinkOut => {
                let t = (elapsed.as_secs_f32() * 1000.0) / fly_ms;
                let ease = ease_out_cubic(t);
                FrameParams {
                    font_pt: (max_pt as f32 - (max_pt - start_pt) as f32 * ease).round() as i32,
                    alpha: 1.0 - ease,
                    x_offset: travel_dx * TRAVEL_FRACTION * ease,
                    y_offset: travel_dy * TRAVEL_FRACTION * ease,
                    glow: false,
                    glow_pulse: 0.0,
                }
            }
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
        card_alpha: f32,
    ) {
        let ka = (card_alpha * 255.0) as u32;
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

    // ── Text measurement ──────────────────────────────────────────────────────

    /// Measure the rendered pixel width of `text` in `font` so the icon+text
    /// unit can be centered horizontally in the window.
    #[cfg(target_os = "windows")]
    unsafe fn measure_text_width(font: HFONT, text: &str) -> i32 {
        use windows::Win32::Graphics::Gdi::GetTextExtentPoint32W;

        let hdc_screen = GetDC(None);
        let hdc = CreateCompatibleDC(hdc_screen);
        ReleaseDC(None, hdc_screen);
        let old = SelectObject(hdc, HGDIOBJ(font.0));
        let wide: Vec<u16> = text.encode_utf16().collect();
        let mut size = windows::Win32::Foundation::SIZE::default();
        let _ = GetTextExtentPoint32W(hdc, &wide, &mut size);
        SelectObject(hdc, old);
        let _ = DeleteDC(hdc);
        size.cx
    }

    // ── Frame rendering ───────────────────────────────────────────────────────

    #[cfg(target_os = "windows")]
    unsafe fn render_frame(hwnd: HWND, state: &mut OverlayState) {
        let Some(active) = state.active.as_ref() else {
            return;
        };

        let (hx, hy) = {
            let cfg = state.handle.config.lock().unwrap();
            if cfg.overlay_history_x >= 0 && cfg.overlay_history_y >= 0 {
                (cfg.overlay_history_x, cfg.overlay_history_y)
            } else {
                (state.window_x, state.anchor_center_y + 220)
            }
        };
        let travel_dx = (hx - state.window_x) as f32;
        let travel_dy = (hy - state.anchor_center_y) as f32;

        let frame = resolve_frame(
            active,
            state.start_pt,
            state.max_pt,
            state.fly_ms,
            travel_dx,
            travel_dy,
            Instant::now(),
        );
        if frame.alpha < 0.004 {
            return;
        }
        // overlay_alpha is a user-configurable ceiling on how opaque the
        // icon/text ever get, applied on top of the animation's own alpha.
        let overall_alpha = frame.alpha * (state.alpha as f32 / 255.0);

        let font = state.font_for(frame.font_pt.max(6));
        let text = &state.active.as_ref().unwrap().event.message;
        let category = state.active.as_ref().unwrap().category;
        let icon_name = state.active.as_ref().unwrap().event.icon.clone();
        let event_color = state.active.as_ref().unwrap().event.color.clone();
        let message_color = state.active.as_ref().unwrap().event.message_color.clone();
        let border_color = state.active.as_ref().unwrap().event.border_color.clone();

        let px_h = frame.font_pt as f32 * 96.0 / 72.0;
        let line_h = (px_h * 1.35).round() as i32;
        let icon_sz = (frame.font_pt as f32 * 1.5).round().clamp(14.0, 160.0) as i32;
        let content_h = line_h.max(icon_sz);
        let h = content_h + PAD_V * 2;
        let w = WINDOW_WIDTH;

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

        let text_rgb = parse_hex_color(&message_color)
            .map(|c| {
                (
                    (c >> 16 & 0xFF) as u8,
                    (c >> 8 & 0xFF) as u8,
                    (c & 0xFF) as u8,
                )
            })
            .unwrap_or(DEFAULT_TEXT_RGB);
        let border_rgb = parse_hex_color(&border_color)
            .map(|c| {
                (
                    (c >> 16 & 0xFF) as u8,
                    (c >> 8 & 0xFF) as u8,
                    (c & 0xFF) as u8,
                )
            })
            .unwrap_or(DEFAULT_BORDER_RGB);

        // Measure icon+gap+text so the whole unit can be horizontally centered.
        let text_w = measure_text_width(font, text);
        let unit_w = icon_sz + ICON_TEXT_GAP + text_w;
        let unit_x = ((w - unit_w) / 2).max(PAD_H);

        // ── Glow halo (drawn first, behind everything — no background panel) ──
        if frame.glow {
            let (gr, gg, gb) = text_rgb;
            let cx_glow = unit_x + icon_sz / 2;
            let cy_glow = h / 2;
            for i in (1..=GLOW_LAYERS).rev() {
                let grow = i * 14;
                let glow_alpha = (46.0 * frame.glow_pulse * overall_alpha / i as f32) as u8;
                fill_rrect(
                    pix,
                    w,
                    h,
                    cx_glow - icon_sz / 2 - grow,
                    cy_glow - icon_sz / 2 - grow,
                    unit_x + unit_w + grow,
                    cy_glow + icon_sz / 2 + grow,
                    CORNER_R + grow,
                    premult(gr, gg, gb, glow_alpha),
                );
            }
        }

        // The x/y offsets (rise-in, vibrate, shrink-travel) move the whole
        // window via `dst_pt` below, so content here stays centered in place.
        let cx = unit_x;
        let cy = (h - icon_sz) / 2;

        // ── Icon swatch ────────────────────────────────────────────────────
        let ix = cx;
        let iy = cy;
        let drew_png = if icon_name.ends_with(".png") || icon_name.ends_with(".jpg") {
            if let Some(icon_pix) = state
                .icon_cache
                .entry(format!("{}@{}", icon_name, icon_sz))
                .or_insert_with(|| load_icon_pixels_premult(&icon_name, icon_sz))
                .as_ref()
                .map(|v| v.as_slice())
            {
                composite_icon(pix, w, h, icon_pix, icon_sz, ix, iy, overall_alpha);
                true
            } else {
                false
            }
        } else {
            false
        };
        if !drew_png {
            let (ir, ig, ib) = if let Some(c) = parse_hex_color(&event_color) {
                (
                    (c >> 16 & 0xFF) as u8,
                    (c >> 8 & 0xFF) as u8,
                    (c & 0xFF) as u8,
                )
            } else {
                category.accent()
            };
            fill_rrect(
                pix,
                w,
                h,
                ix,
                iy,
                ix + icon_sz,
                iy + icon_sz,
                4,
                premult(ir, ig, ib, (200.0 * overall_alpha) as u8),
            );
        }

        // ── Message text ───────────────────────────────────────────────────
        let tx = cx + icon_sz + ICON_TEXT_GAP;
        let stroke_width = ((frame.font_pt as f32 / 20.0).round() as i32).clamp(1, 3);
        composite_text_stroked(
            pix,
            w,
            h,
            text,
            font,
            text_rgb,
            border_rgb,
            stroke_width,
            overall_alpha,
            tx,
            0,
            w - tx - PAD_H,
            h,
            DT_LEFT | DT_NOCLIP,
        );

        let dst_pt = POINT {
            x: state.window_x + frame.x_offset.round() as i32,
            y: state.anchor_center_y - h / 2 + frame.y_offset.round() as i32,
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

    pub fn spawn_overlay(handle: Arc<AppHandle>) {
        std::thread::Builder::new()
            .name("froklog-overlay".into())
            .spawn(move || run_overlay_thread(handle))
            .expect("spawn overlay thread");
    }

    #[cfg(target_os = "windows")]
    fn run_overlay_thread(handle: Arc<AppHandle>) {
        unsafe {
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

            let (wx, anchor_center_y) = initial_position(&handle.config.lock().unwrap());
            let state = Box::new(OverlayState::new(&handle, wx, anchor_center_y));
            let state_ptr = Box::into_raw(state);

            // No WS_VISIBLE — the first WM_TIMER tick decides whether to show it.
            let hwnd = CreateWindowExW(
                WS_EX_TOPMOST | WS_EX_LAYERED | WS_EX_TOOLWINDOW | WS_EX_NOACTIVATE,
                PCWSTR(class_w.as_ptr()),
                PCWSTR::null(),
                WS_POPUP,
                wx,
                anchor_center_y - 1,
                WINDOW_WIDTH,
                1,
                None,
                None,
                hinstance,
                Some(state_ptr as *const c_void),
            )
            .expect("CreateWindowExW overlay");

            handle
                .overlay_hwnd
                .store(hwnd.0 as isize, Ordering::Relaxed);

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
    fn run_overlay_thread(_handle: Arc<AppHandle>) {}

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
                if state.handle.quit.load(Ordering::Relaxed) {
                    let _ = DestroyWindow(hwnd);
                    return LRESULT(0);
                }
                let lock_changed = state.sync_config();
                if lock_changed {
                    apply_lock_style(hwnd, state.locked);
                }
                state.drain_queue();
                state.tick_state();

                let show = (state.overlay_enabled || state.force_show) && state.active.is_some();
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
                render_frame(hwnd, state);
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
                    state.anchor_center_y = (rect.top + rect.bottom) / 2;
                    let mut cfg = state.handle.config.lock().unwrap();
                    cfg.overlay_x = rect.left;
                    cfg.overlay_y = state.anchor_center_y;
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

            let (enabled, speed, mode, re, ro, ra, voice_id) = {
                let cfg = self.handle.config.lock().unwrap();
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

            let new_rate = tts_speed_to_rate(&speed);
            if new_rate != self.tts_rate {
                self.tts_rate = new_rate;
                let _ = voice.SetRate(new_rate);
            }

            let flags: u32 = match mode {
                TtsAudioMode::InterruptConstantly => 1 | 2,
                TtsAudioMode::QueueAll => 1,
                TtsAudioMode::SmartPriority => match priority {
                    VoicePriority::Emergency => 1 | 2,
                    VoicePriority::Ambient => {
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

    /// Overall volume (0-100) applied to sounds played via `play_sound`, kept
    /// as a process-wide atomic so the fire-and-forget playback thread doesn't
    /// need a `Config` handle threaded through it. Set from `Config.sound_volume`
    /// at startup and whenever the Settings dialog is saved.
    static SOUND_VOLUME_PERCENT: AtomicU8 = AtomicU8::new(100);
    /// Master mute for `play_sound`, mirroring `Config.sound_enabled`.
    static SOUND_ENABLED: AtomicBool = AtomicBool::new(true);

    pub fn set_sound_volume_percent(percent: u8) {
        SOUND_VOLUME_PERCENT.store(percent, Ordering::Relaxed);
    }

    pub fn set_sound_enabled(enabled: bool) {
        SOUND_ENABLED.store(enabled, Ordering::Relaxed);
    }

    /// Name of the currently-active sound package, kept as a process-wide
    /// value for the same reason as `SOUND_VOLUME_PERCENT` above — sound
    /// resolution happens at play-time, so updating this is all that's
    /// needed for a package switch to take effect on the next sound played,
    /// with no engine restart. Set from `Config.sound_package` at startup
    /// and whenever the Settings dialog is saved.
    static ACTIVE_SOUND_PACKAGE: Mutex<String> = Mutex::new(String::new());

    pub fn set_active_sound_package(name: &str) {
        *ACTIVE_SOUND_PACKAGE.lock().unwrap() = name.to_string();
    }

    /// Resolves `label` to a playable path via the active sound package,
    /// falling back to the default package. `None` if empty or unresolvable —
    /// an unresolvable label is never a crash, just a missed sound.
    fn resolve_sound_label(label: &str) -> Option<String> {
        if label.is_empty() {
            return None;
        }
        let active = ACTIVE_SOUND_PACKAGE.lock().unwrap().clone();
        let active = if active.is_empty() {
            crate::sound_packages::sound_packages::DEFAULT_PACKAGE.to_string()
        } else {
            active
        };
        crate::sound_packages::sound_packages::resolve_label(&active, label)
            .map(|p| p.to_string_lossy().into_owned())
    }

    /// Plays the sound file that `label` resolves to, subject to the
    /// "Sound enabled" setting — this is what triggers use to fire alerts.
    pub(crate) fn play_sound_label(label: &str) {
        if let Some(path) = resolve_sound_label(label) {
            play_sound(&path);
        }
    }

    /// Plays `label`'s sound unconditionally, ignoring the "Sound enabled"
    /// setting — used by the trigger editor's ▶ preview button, so users can
    /// audition a sound even while alerts are muted.
    pub(crate) fn preview_sound_label(label: &str) {
        if let Some(path) = resolve_sound_label(label) {
            play_sound_file(&path);
        }
    }

    pub(crate) fn play_sound(path: &str) {
        if !SOUND_ENABLED.load(Ordering::Relaxed) {
            return;
        }
        play_sound_file(path);
    }

    /// Plays `path` unconditionally, ignoring the "Sound enabled" setting —
    /// used by the Sounds tab's preview button.
    pub(crate) fn preview_sound(path: &str) {
        play_sound_file(path);
    }

    fn play_sound_file(path: &str) {
        #[cfg(target_os = "windows")]
        {
            let resolved = resolve_sound_path(path);
            let volume = SOUND_VOLUME_PERCENT.load(Ordering::Relaxed) as f32 / 100.0;
            std::thread::spawn(move || {
                let Ok((_stream, handle)) = rodio::OutputStream::try_default() else {
                    return;
                };
                let Ok(sink) = rodio::Sink::try_new(&handle) else {
                    return;
                };
                sink.set_volume(volume);
                let Ok(file) = std::fs::File::open(&resolved) else {
                    return;
                };
                let Ok(source) = rodio::Decoder::new(std::io::BufReader::new(file)) else {
                    return;
                };
                sink.append(source);
                sink.sleep_until_end();
            });
        }
        #[cfg(not(target_os = "windows"))]
        {
            let _ = path;
        }
    }

    #[cfg(target_os = "windows")]
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

    /// Returns `(x, anchor_center_y)`. The overlay window is centred
    /// vertically on `anchor_center_y` and grows/shrinks around it.
    fn initial_position(cfg: &Config) -> (i32, i32) {
        #[cfg(target_os = "windows")]
        {
            if cfg.overlay_x >= 0 && cfg.overlay_y >= 0 {
                return (cfg.overlay_x, cfg.overlay_y);
            }
            unsafe {
                let sw = GetSystemMetrics(SM_CXSCREEN);
                let sh = GetSystemMetrics(SM_CYSCREEN);
                let anchor = (sh as f32 * 0.35) as i32;
                ((sw - WINDOW_WIDTH) / 2, anchor)
            }
        }
        #[cfg(not(target_os = "windows"))]
        (cfg.overlay_x.max(0), cfg.overlay_y.max(1))
    }
}
