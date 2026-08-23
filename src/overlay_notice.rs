/// Fade/slide/fly-in Notice overlay window — a second, lighter overlay kind
/// alongside Alert (`overlay.rs`). Structurally a trimmed copy of that
/// module: same window lifecycle (timer tick, drag-to-move, Show All
/// Windows placeholder, log-inactivity hide, Test-button override), driven
/// by the same shared `AlertEngine` (constructed with
/// `EngineSource::Notice`, so it drains `AppHandle::notice_queue` instead of
/// `overlay_queue` — see `alert_engine.rs`).
///
/// What's deliberately different from Alert: a single fixed `font_pt`
/// instead of an animated start->max growth (no "Start Size" setting —
/// see `Config::overlay_notice_font_size`), no Glow/Vibrate/Pulse hold-phase
/// treatment at all, and the FlyIn/ShrinkOut phases are driven by
/// `resolve_notice_frame` (below) instead of `overlay::resolve_frame` — a
/// fade/slide/fly entrance/exit chosen by `Config::overlay_notice_transition`
/// rather than Alert's font-size growth.
#[cfg(feature = "tray")]
#[allow(clippy::module_inception)]
pub mod overlay_notice {
    use std::collections::HashMap;
    use std::sync::atomic::Ordering;
    use std::sync::Arc;
    use std::time::{Duration, Instant};

    use slint::ComponentHandle;

    use crate::alert_engine::alert_engine::{ActiveAlert, AlertEngine, AlertPhase, EngineSource};
    use crate::config::NoticeTransition;
    use crate::overlay_draw::overlay_draw::parse_hex_color;
    use crate::overlay_registry::overlay_registry::OverlayKind;
    use crate::overlay_shell;
    use crate::tray::tray::AppHandle;

    include!(concat!(env!("OUT_DIR"), "/overlay_notice.rs"));

    // ── Constants ─────────────────────────────────────────────────────────────

    const ANIM_INTERVAL_MS: u64 = 16;

    /// Vertical rise (px) during Slide's fly-in, easing to 0 as the message
    /// settles — mirrors Alert's `RISE_PX` but smaller, since Notice is a
    /// lighter-weight callout.
    const SLIDE_PX: f32 = 20.0;
    /// Horizontal travel (px) during Fly's fly-in.
    const FLY_PX: f32 = 60.0;

    const DEFAULT_TEXT_RGB: (u8, u8, u8) = (255, 255, 255);
    const DEFAULT_BORDER_RGB: (u8, u8, u8) = (0, 0, 0);

    // ── State ─────────────────────────────────────────────────────────────────

    struct OverlayState {
        handle: Arc<AppHandle>,
        engine: AlertEngine,
        overlay_enabled: bool,
        font_pt: i32,
        alpha: u8,
        transition: NoticeTransition,
        visible: bool,
        force_show: bool,
        locked: bool,
        hide_inactive_secs: u32,
        icon_cache: HashMap<String, Option<slint::Image>>,
        tick_count: u64,
    }

    impl OverlayState {
        fn new(handle: &Arc<AppHandle>) -> Self {
            // See overlay.rs's `OverlayState::new` doc comment — same
            // deadlock hazard (`AlertEngine::new` locks `handle.config`
            // itself), same fix (construct it before taking `cfg` here).
            let engine = AlertEngine::new(handle, EngineSource::Notice);
            let cfg = handle.config.lock().unwrap();
            Self {
                handle: Arc::clone(handle),
                engine,
                overlay_enabled: cfg.overlay_notice.enabled,
                font_pt: cfg.overlay_notice_font_size.max(6) as i32,
                alpha: cfg.overlay_notice_alpha,
                transition: cfg.overlay_notice_transition,
                visible: false,
                force_show: handle.force_show_windows.load(Ordering::Relaxed),
                locked: cfg.overlay_notice.locked,
                hide_inactive_secs: cfg.overlay_hide_inactive_secs,
                icon_cache: HashMap::new(),
                tick_count: 0,
            }
        }

        fn icon_for(&mut self, filename: &str) -> Option<slint::Image> {
            if filename.is_empty() {
                return None;
            }
            self.icon_cache
                .entry(filename.to_string())
                .or_insert_with(|| load_icon_image(filename))
                .clone()
        }

        /// Reload live-tunable settings from config.
        fn sync_config(&mut self) {
            let cfg = self.handle.config.lock().unwrap();
            self.overlay_enabled = cfg.overlay_notice.enabled;
            self.font_pt = cfg.overlay_notice_font_size.max(6) as i32;
            self.alpha = cfg.overlay_notice_alpha;
            self.transition = cfg.overlay_notice_transition;
            self.locked = cfg.overlay_notice.locked;
            self.hide_inactive_secs = cfg.overlay_hide_inactive_secs;
            drop(cfg);
            self.engine.sync_config();
            self.force_show = self.handle.force_show_windows.load(Ordering::Relaxed);
        }
    }

    // ── Animation math ────────────────────────────────────────────────────────

    fn ease_out_cubic(t: f32) -> f32 {
        let t = t.clamp(0.0, 1.0);
        1.0 - (1.0 - t).powi(3)
    }

    pub(crate) struct NoticeFrameParams {
        pub alpha: f32,
        pub x_offset: f32,
        pub y_offset: f32,
    }

    /// FlyIn/ShrinkOut ease `alpha` 0<->1 for every transition style — `Fade`
    /// adds no offset, `Slide` eases a vertical offset (rises in from
    /// below), `Fly` eases a larger horizontal offset (enters from the
    /// side). Hold is always static: no jitter, matching Notice having no
    /// Glow/Vibrate/Pulse treatment at all.
    pub(crate) fn resolve_notice_frame(
        active: &ActiveAlert,
        transition: NoticeTransition,
        transition_ms: u32,
        now: Instant,
    ) -> NoticeFrameParams {
        let elapsed = now.duration_since(active.phase_started);
        let transition_ms = transition_ms.max(1) as f32;
        match active.phase {
            AlertPhase::FlyIn => {
                let t = (elapsed.as_secs_f32() * 1000.0) / transition_ms;
                let ease = ease_out_cubic(t);
                let (x, y) = entrance_offset(transition, 1.0 - ease);
                NoticeFrameParams {
                    alpha: ease,
                    x_offset: x,
                    y_offset: y,
                }
            }
            AlertPhase::Hold => NoticeFrameParams {
                alpha: 1.0,
                x_offset: 0.0,
                y_offset: 0.0,
            },
            AlertPhase::ShrinkOut => {
                let t = (elapsed.as_secs_f32() * 1000.0) / transition_ms;
                let ease = ease_out_cubic(t);
                let (x, y) = entrance_offset(transition, ease);
                NoticeFrameParams {
                    alpha: 1.0 - ease,
                    x_offset: x,
                    y_offset: y,
                }
            }
        }
    }

    /// `amount` in `[0, 1]` — 0 = settled/on-screen, 1 = fully off-screen at
    /// the transition's start (FlyIn) or end (ShrinkOut).
    fn entrance_offset(transition: NoticeTransition, amount: f32) -> (f32, f32) {
        match transition {
            NoticeTransition::Fade => (0.0, 0.0),
            NoticeTransition::Slide => (0.0, amount * SLIDE_PX),
            NoticeTransition::Fly => (amount * FLY_PX, 0.0),
        }
    }

    // ── Window creation & tick ────────────────────────────────────────────────

    /// Creates the Notice overlay window and starts its animation timer. Must
    /// run on the Slint UI thread — see tray.rs's module doc. Mirrors
    /// `overlay::create_alert_window`'s structure closely; see that
    /// function's comments for the reasoning behind each piece (taskbar/
    /// no-activate/topmost handling, the alpha-reset-on-hide fix, the
    /// log-inactivity/Test-button gating order).
    pub fn create_notice_window(handle: Arc<AppHandle>) {
        tracing::info!("notice overlay created, handle={:p}", Arc::as_ptr(&handle));
        let window = NoticeOverlayWindow::new().expect("create notice overlay window");
        window.set_has_icon(false);
        #[cfg(target_os = "linux")]
        window.set_panel_fallback(!crate::tray::tray::is_composited());

        let mut state = OverlayState::new(&handle);
        let cfg_x_y = {
            let cfg = handle.config.lock().unwrap();
            (cfg.overlay_notice.x, cfg.overlay_notice.y)
        };
        if cfg_x_y != (-1, -1) {
            window.window().set_position(slint::WindowPosition::Logical(
                slint::LogicalPosition::new(cfg_x_y.0 as f32, cfg_x_y.1 as f32),
            ));
        }

        window.on_drag_requested({
            let weak = window.as_weak();
            let handle = Arc::clone(&handle);
            move || {
                overlay_shell::overlay_shell::begin_drag(
                    weak.clone(),
                    Arc::clone(&handle),
                    OverlayKind::Notice,
                );
            }
        });

        let weak = window.as_weak();
        let timer = slint::Timer::default();
        timer.start(
            slint::TimerMode::Repeated,
            Duration::from_millis(ANIM_INTERVAL_MS),
            move || {
                let Some(ui) = weak.upgrade() else { return };
                if state.handle.quit.load(Ordering::Relaxed) {
                    return;
                }
                state.tick_count += 1;
                state.sync_config();
                ui.set_locked(state.locked);
                ui.set_force_show(state.force_show);
                crate::overlay_draw::overlay_draw::sync_click_through(
                    ui.window(),
                    state.locked && !state.force_show,
                );
                let transition_ms = state.engine.fly_ms();
                let has_test_content = state.engine.has_test_content();
                let log_inactive =
                    state.handle.log_inactive(state.hide_inactive_secs) && !has_test_content;
                let active = if !log_inactive {
                    state.engine.tick()
                } else {
                    None
                };

                let show = !log_inactive
                    && (state.overlay_enabled || state.force_show || has_test_content)
                    && active.is_some();
                if !show {
                    if state.visible {
                        tracing::info!("notice overlay: hiding");
                        if let Err(e) = ui.hide() {
                            tracing::warn!("notice overlay: hide() failed: {e}");
                        }
                        state.visible = false;
                        // See overlay.rs's create_alert_window for why this
                        // reset is needed — a placeholder's constant
                        // alpha 1.0 would otherwise make the next show's
                        // set_alpha(1.0) a silent no-op.
                        ui.set_alpha(0.0);
                    }
                    return;
                }
                if !state.visible {
                    tracing::info!(
                        "notice overlay: showing (force_show={}, is_placeholder={})",
                        state.force_show,
                        active.map(|a| a.is_placeholder).unwrap_or(false)
                    );
                    crate::overlay_draw::overlay_draw::apply_saved_position(
                        &ui,
                        &state.handle,
                        OverlayKind::Notice,
                    );
                    if let Err(e) = ui.show() {
                        tracing::warn!("notice overlay: show() failed: {e}");
                    }
                    state.visible = true;
                    crate::overlay_draw::overlay_draw::hide_from_taskbar(weak.clone());
                    crate::overlay_draw::overlay_draw::set_no_activate(weak.clone());
                    crate::overlay_draw::overlay_draw::force_repaint(weak.clone());
                }
                if state.tick_count.is_multiple_of(300) {
                    crate::overlay_draw::overlay_draw::reassert_topmost(ui.window());
                }

                let now = Instant::now();
                let active = active.unwrap();
                let frame = resolve_notice_frame(active, state.transition, transition_ms, now);
                let event = active.event.clone();

                ui.set_message(event.message.into());
                ui.set_text_color(color_from_hex(&event.message_color, DEFAULT_TEXT_RGB));
                ui.set_stroke_color(color_from_hex(&event.border_color, DEFAULT_BORDER_RGB));
                if let Some(icon) = state.icon_for(&event.icon) {
                    ui.set_icon_source(icon);
                    ui.set_has_icon(true);
                } else {
                    ui.set_has_icon(false);
                }
                ui.set_font_size_pt(state.font_pt as f32);
                ui.set_alpha((frame.alpha * (state.alpha as f32 / 255.0)).clamp(0.0, 1.0));
                ui.set_x_offset(frame.x_offset);
                ui.set_y_offset(frame.y_offset);
            },
        );
        // See overlay.rs's create_alert_window for why both must be forgotten.
        std::mem::forget(timer);
        std::mem::forget(window);
    }

    fn color_from_hex(hex: &str, default_rgb: (u8, u8, u8)) -> slint::Color {
        let (r, g, b) = parse_hex_color(hex)
            .map(|c| {
                (
                    (c >> 16 & 0xFF) as u8,
                    (c >> 8 & 0xFF) as u8,
                    (c & 0xFF) as u8,
                )
            })
            .unwrap_or(default_rgb);
        slint::Color::from_rgb_u8(r, g, b)
    }

    fn load_icon_image(filename: &str) -> Option<slint::Image> {
        let exe = std::env::current_exe().ok()?;
        let path = exe.parent()?.join("icons").join(filename);
        slint::Image::load_from_path(&path).ok()
    }
}
