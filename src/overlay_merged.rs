/// Combined alert+history overlay window — the `AlertStyle::Merged`
/// alternative to running the alert overlay (`overlay.rs`) and history
/// overlay (`overlay_history.rs`) as two separate windows.
///
/// Real Slint window (`ui/overlay_merged.slint`, `MergedOverlayWindow`).
/// Drives its own `AlertEngine` (the same queue/priority/TTS/phase-timing
/// logic the standalone alert window uses) for the top "incoming" slot, and
/// renders `AppHandle.overlay_history` below it via the same
/// `snapshot_and_trim` helper the standalone history window uses — see
/// those two modules for the logic this one composes rather than
/// reimplements. Only active — draining the alert queue and rendering the
/// history list — while `Config::alert_style` is `Merged`; created
/// unconditionally like every other overlay window (see
/// `overlay_registry.rs`), but stays dormant and hidden otherwise so it
/// doesn't fight the standalone windows over the same shared queue/list.
#[cfg(feature = "tray")]
#[allow(clippy::module_inception)]
pub mod overlay_merged {
    use std::collections::HashMap;
    use std::sync::atomic::Ordering;
    use std::sync::Arc;
    use std::time::{Duration, Instant};

    use slint::{Color, ComponentHandle, ModelRc, VecModel};

    use crate::alert_engine::alert_engine::AlertEngine;
    use crate::config::AlertStyle;
    use crate::overlay::overlay::{resolve_frame, FrameParams};
    use crate::overlay_draw::overlay_draw::parse_hex_color;
    use crate::overlay_history::overlay_history::snapshot_and_trim;
    use crate::overlay_registry::overlay_registry::OverlayKind;
    use crate::overlay_shell;
    use crate::tray::tray::AppHandle;

    include!(concat!(env!("OUT_DIR"), "/overlay_merged.rs"));

    const TIMER_INTERVAL_MS: u64 = 50;
    const FADE_IN_SECS: f32 = 0.25;

    const DEFAULT_TEXT_RGB: (u8, u8, u8) = (255, 255, 255);
    const DEFAULT_BORDER_RGB: (u8, u8, u8) = (0, 0, 0);

    fn color_from_hex(hex: &str, default_rgb: (u8, u8, u8)) -> Color {
        let (r, g, b) = parse_hex_color(hex)
            .map(|c| {
                (
                    (c >> 16 & 0xFF) as u8,
                    (c >> 8 & 0xFF) as u8,
                    (c & 0xFF) as u8,
                )
            })
            .unwrap_or(default_rgb);
        Color::from_rgb_u8(r, g, b)
    }

    /// Below one minute, seconds; below one hour, minutes; at/above one
    /// hour, hours — see overlay_history.rs's copy of this fn for why each
    /// tier rounds instead of floor/ceil.
    fn format_time_ago(secs_ago: f32) -> String {
        let mins = secs_ago / 60.0;
        if secs_ago < 60.0 {
            format!("{}s", secs_ago as u64)
        } else if mins < 60.0 {
            format!("{}m", mins.round() as u64)
        } else {
            format!("{}h", (mins / 60.0).round() as u64)
        }
    }

    fn load_icon_image(filename: &str) -> Option<slint::Image> {
        let exe = std::env::current_exe().ok()?;
        let path = exe.parent()?.join("icons").join(filename);
        slint::Image::load_from_path(&path).ok()
    }

    // ── State ─────────────────────────────────────────────────────────────────

    struct MergedState {
        handle: Arc<AppHandle>,
        engine: AlertEngine,
        alert_style: AlertStyle,
        enabled: bool,
        start_pt: i32,
        max_pt: i32,
        alpha: u8,
        history_max_entries: usize,
        history_idle_secs: u32,
        history_font_size: f32,
        visible: bool,
        force_show: bool,
        locked: bool,
        icon_cache: HashMap<String, Option<slint::Image>>,
        tick_count: u64,
    }

    impl MergedState {
        fn new(handle: &Arc<AppHandle>) -> Self {
            // See overlay.rs's `OverlayState::new` for why `engine` is
            // built before `cfg` is locked here: `AlertEngine::new` locks
            // `handle.config` itself, and doing that while `cfg` was
            // already held self-deadlocked this thread on every launch.
            let engine = AlertEngine::new(handle, true);
            let cfg = handle.config.lock().unwrap();
            Self {
                handle: Arc::clone(handle),
                engine,
                alert_style: cfg.alert_style,
                enabled: cfg.overlay_merged.enabled,
                start_pt: cfg.overlay_merged_start_font_size.max(6) as i32,
                max_pt: cfg.overlay_merged_max_font_size.max(6) as i32,
                alpha: cfg.overlay_merged_alpha,
                history_max_entries: cfg.overlay_merged_history_max_entries.max(1),
                history_idle_secs: cfg.overlay_merged_history_idle_secs,
                history_font_size: cfg.overlay_merged_history_font_size.max(8) as f32,
                visible: false,
                force_show: handle.force_show_windows.load(Ordering::Relaxed),
                locked: cfg.overlay_merged.locked,
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

        fn sync_config(&mut self) {
            let cfg = self.handle.config.lock().unwrap();
            self.alert_style = cfg.alert_style;
            self.enabled = cfg.overlay_merged.enabled;
            self.start_pt = cfg.overlay_merged_start_font_size.max(6) as i32;
            self.max_pt = cfg.overlay_merged_max_font_size.max(6) as i32;
            self.alpha = cfg.overlay_merged_alpha;
            self.history_max_entries = cfg.overlay_merged_history_max_entries.max(1);
            self.history_idle_secs = cfg.overlay_merged_history_idle_secs;
            self.history_font_size = cfg.overlay_merged_history_font_size.max(8) as f32;
            self.locked = cfg.overlay_merged.locked;
            drop(cfg);
            self.engine.sync_config();
            self.force_show = self.handle.force_show_windows.load(Ordering::Relaxed);
        }
    }

    // ── Window creation & tick ────────────────────────────────────────────────

    /// Creates the merged alert+history overlay window and starts its
    /// refresh timer. Must run on the Slint UI thread — see tray.rs's
    /// module doc.
    pub fn create_merged_window(handle: Arc<AppHandle>) {
        tracing::info!("merged overlay created, handle={:p}", Arc::as_ptr(&handle));
        let window = MergedOverlayWindow::new().expect("create merged overlay window");
        // NOT called here — see overlay.rs's create_alert_window for why
        // (the native window doesn't exist yet at creation time). Called on
        // first `.show()` below instead.

        let mut state = MergedState::new(&handle);
        let cfg_x_y = {
            let cfg = handle.config.lock().unwrap();
            (cfg.overlay_merged.x, cfg.overlay_merged.y)
        };
        // (-1, -1) is the never-positioned sentinel; any OTHER value is a
        // real saved position — including negative coordinates, which are
        // routine on multi-monitor layouts (a monitor left of or above the
        // primary) and whenever a window is nudged past a screen's top/left
        // edge. Gating on >= 0 silently discarded those on every restart.
        if cfg_x_y != (-1, -1) {
            window.window().set_position(slint::WindowPosition::Logical(
                slint::LogicalPosition::new(cfg_x_y.0 as f32, cfg_x_y.1 as f32),
            ));
        }

        // Drag-to-move — see overlay.rs's create_alert_window for why this
        // exists and how it works (same pattern, same reasoning).
        window.on_drag_requested({
            let weak = window.as_weak();
            let handle = Arc::clone(&handle);
            move || {
                overlay_shell::overlay_shell::begin_drag(
                    weak.clone(),
                    Arc::clone(&handle),
                    OverlayKind::Merged,
                );
            }
        });

        let weak = window.as_weak();
        let timer = slint::Timer::default();
        timer.start(
            slint::TimerMode::Repeated,
            Duration::from_millis(TIMER_INTERVAL_MS),
            move || {
                let Some(ui) = weak.upgrade() else { return };
                if state.handle.quit.load(Ordering::Relaxed) {
                    return;
                }
                state.tick_count += 1;
                if state.tick_count.is_multiple_of(100) {
                    tracing::info!(
                        "merged overlay heartbeat: force_show={}, visible={}, handle={:p}",
                        state.force_show,
                        state.visible,
                        Arc::as_ptr(&state.handle)
                    );
                }
                state.sync_config();
                ui.set_locked(state.locked);
                ui.set_force_show(state.force_show);
                // Locked means click-through for real on Linux (input
                // shape) — except while Show All Windows has the drag
                // TouchArea re-enabled, when the window must stay clickable.
                crate::overlay_draw::overlay_draw::sync_click_through(
                    ui.window(),
                    state.locked && !state.force_show,
                );

                // Only active while this is the selected alert style — see
                // `MergedState::alert_style`'s doc comment on the struct
                // above and `overlay.rs::OverlayState::alert_style`'s for
                // the mirrored gate on the standalone alert window.
                let is_active_style = state.alert_style == AlertStyle::Merged;
                let fly_ms = state.engine.fly_ms();
                let active = if is_active_style {
                    state.engine.tick()
                } else {
                    None
                };
                let raw_entries = if is_active_style {
                    snapshot_and_trim(&state.handle, state.history_max_entries)
                } else {
                    Vec::new()
                };

                let last_arrived_secs = raw_entries.last().map(|e| e.5);
                let idle_timed_out = state.history_idle_secs > 0
                    && last_arrived_secs.is_some_and(|s| s > state.history_idle_secs as f32);
                let has_content = active.is_some() || !raw_entries.is_empty();
                let show = is_active_style
                    && (state.force_show || (state.enabled && has_content && !idle_timed_out));
                if !show {
                    if state.visible {
                        tracing::info!("merged overlay: hiding");
                        let _ = ui.hide();
                        state.visible = false;
                    }
                    return;
                }
                if !state.visible {
                    tracing::info!("merged overlay: showing (force_show={})", state.force_show);
                    crate::overlay_draw::overlay_draw::apply_saved_position(
                        &ui,
                        &state.handle,
                        crate::overlay_registry::overlay_registry::OverlayKind::Merged,
                    );
                    let _ = ui.show();
                    state.visible = true;
                    crate::overlay_draw::overlay_draw::hide_from_taskbar(weak.clone());
                    crate::overlay_draw::overlay_draw::set_no_activate(weak.clone());
                }
                // See `overlay_draw::reassert_topmost`'s doc comment.
                if state.tick_count.is_multiple_of(100) {
                    crate::overlay_draw::overlay_draw::reassert_topmost(ui.window());
                }

                // ── Incoming-alert slot ──────────────────────────────────
                // Placeholder ("Drag me") alerts render here too, same as
                // the standalone alert window (overlay.rs) — Show All
                // Overlays should give this window something to drag by
                // just like the other two, not just an empty incoming slot.
                if let Some(active) = active {
                    let now = Instant::now();
                    let frame: FrameParams =
                        resolve_frame(active, state.start_pt, state.max_pt, fly_ms, now);
                    let event = active.event.clone();
                    ui.set_has_incoming(true);
                    ui.set_incoming_message(event.message.into());
                    ui.set_incoming_text_color(color_from_hex(
                        &event.message_color,
                        DEFAULT_TEXT_RGB,
                    ));
                    ui.set_incoming_stroke_color(color_from_hex(
                        &event.border_color,
                        DEFAULT_BORDER_RGB,
                    ));
                    if let Some(icon) = state.icon_for(&event.icon) {
                        ui.set_incoming_icon_source(icon);
                        ui.set_incoming_has_icon(true);
                    } else {
                        ui.set_incoming_has_icon(false);
                    }
                    ui.set_incoming_font_size_pt(frame.font_pt.max(1.0));
                    ui.set_incoming_alpha(
                        (frame.alpha * (state.alpha as f32 / 255.0)).clamp(0.0, 1.0),
                    );
                    ui.set_incoming_glow(frame.glow);
                    ui.set_incoming_glow_pulse(frame.glow_pulse);
                } else {
                    ui.set_has_incoming(false);
                }

                // ── History rows ─────────────────────────────────────────
                // Newest entry (last pushed) renders at the top row.
                let rows: Vec<MergedHistoryRow> = raw_entries
                    .iter()
                    .rev()
                    .map(
                        |(icon, color, message, message_color, border_color, secs_ago)| {
                            let icon_image = state.icon_for(icon);
                            MergedHistoryRow {
                                icon_source: icon_image.clone().unwrap_or_default(),
                                has_icon: icon_image.is_some(),
                                icon_color: color_from_hex(color, (150, 150, 160)),
                                message: message.as_str().into(),
                                message_color: color_from_hex(message_color, DEFAULT_TEXT_RGB),
                                border_color: color_from_hex(border_color, DEFAULT_BORDER_RGB),
                                time_label: format_time_ago(*secs_ago).into(),
                                row_alpha: (*secs_ago / FADE_IN_SECS).clamp(0.0, 1.0),
                            }
                        },
                    )
                    .collect();
                ui.set_row_font_size(state.history_font_size);
                ui.set_rows(ModelRc::new(VecModel::from(rows)));
            },
        );
        // Both must outlive this function — see overlay.rs's
        // create_alert_window for why the window itself needs forgetting
        // too, not just the timer (it's never `.show()`n here, so nothing
        // else keeps its Rc-based component alive).
        std::mem::forget(timer);
        std::mem::forget(window);
    }
}
