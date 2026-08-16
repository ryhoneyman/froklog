/// Previous-messages history overlay window.
///
/// Real Slint window (`ui/overlay_history.slint`, `HistoryOverlayWindow`)
/// replacing the old `WS_EX_LAYERED` popup — a flat list of messages that
/// have finished their alert lifecycle: icon + message + "Ns" elapsed-time
/// label. Reads `AppHandle.overlay_history`, which `overlay.rs` appends to
/// once a message shrinks away. Same "keep the business logic, replace the
/// paint step" approach as overlay.rs; see its module doc for the fuller
/// explanation of why this now runs on a `slint::Timer` on the UI thread
/// instead of its own OS thread.
#[cfg(feature = "tray")]
#[allow(clippy::module_inception)]
pub mod overlay_history {
    use std::sync::atomic::Ordering;
    use std::sync::Arc;
    use std::time::{Duration, Instant};

    use slint::{Color, ComponentHandle, ModelRc, VecModel};

    use crate::overlay_draw::overlay_draw::parse_hex_color;
    use crate::overlay_registry::overlay_registry::OverlayKind;
    use crate::overlay_shell;
    use crate::tray::tray::AppHandle;

    include!(concat!(env!("OUT_DIR"), "/overlay_history.rs"));

    const TIMER_INTERVAL_MS: u64 = 200;
    const FADE_IN_SECS: f32 = 0.25;

    /// Default text fill/stroke when a trigger doesn't override them.
    const DEFAULT_TEXT_RGB: (u8, u8, u8) = (255, 255, 255);
    const DEFAULT_BORDER_RGB: (u8, u8, u8) = (0, 0, 0);

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

    /// Drains history entries beyond `max_entries` (oldest first) and
    /// returns a plain snapshot of what's left, oldest to newest — shared by
    /// this window and the merged alert+history window
    /// (`overlay_merged.rs`), which both render `AppHandle.overlay_history`
    /// as a row list.
    pub fn snapshot_and_trim(
        handle: &AppHandle,
        max_entries: usize,
    ) -> Vec<(String, String, String, String, String, f32)> {
        let mut hist = handle.overlay_history.lock().unwrap();
        if hist.len() > max_entries {
            let excess = hist.len() - max_entries;
            hist.drain(0..excess);
        }
        hist.iter()
            .map(|e| {
                (
                    e.icon.clone(),
                    e.color.clone(),
                    e.message.clone(),
                    e.message_color.clone(),
                    e.border_color.clone(),
                    e.arrived.elapsed().as_secs_f32(),
                )
            })
            .collect()
    }

    // ── State ─────────────────────────────────────────────────────────────────

    struct HistoryState {
        handle: Arc<AppHandle>,
        history_enabled: bool,
        /// This window only displays while `Separate` — when `Merged`, the
        /// combined alert+history window owns the history list instead. See
        /// `overlay.rs::OverlayState::alert_style`'s doc comment.
        alert_style: crate::config::AlertStyle,
        max_entries: usize,
        idle_secs: u32,
        font_size: f32,
        visible: bool,
        force_show: bool,
        locked: bool,
        icon_cache: std::collections::HashMap<String, Option<slint::Image>>,
        tick_count: u64,
    }

    impl HistoryState {
        fn new(handle: &Arc<AppHandle>) -> Self {
            let cfg = handle.config.lock().unwrap();
            Self {
                handle: Arc::clone(handle),
                history_enabled: cfg.overlay_history.enabled,
                alert_style: cfg.alert_style,
                max_entries: cfg.overlay_history_max_entries.max(1),
                idle_secs: cfg.overlay_history_idle_secs,
                font_size: cfg.overlay_history_font_size.max(8) as f32,
                visible: false,
                force_show: handle.force_show_windows.load(Ordering::Relaxed),
                locked: cfg.overlay_history.locked,
                icon_cache: std::collections::HashMap::new(),
                tick_count: 0,
            }
        }

        fn icon_for(&mut self, filename: &str) -> Option<slint::Image> {
            if filename.is_empty() || !(filename.ends_with(".png") || filename.ends_with(".jpg")) {
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
            self.font_size = cfg.overlay_history_font_size.max(8) as f32;
            self.history_enabled = cfg.overlay_history.enabled;
            self.alert_style = cfg.alert_style;
            self.max_entries = cfg.overlay_history_max_entries.max(1);
            self.idle_secs = cfg.overlay_history_idle_secs;
            self.locked = cfg.overlay_history.locked;
            drop(cfg);
            let new_force_show = self.handle.force_show_windows.load(Ordering::Relaxed);
            if new_force_show != self.force_show {
                tracing::info!(
                    "history overlay: force_show {} -> {}",
                    self.force_show,
                    new_force_show
                );
            }
            self.force_show = new_force_show;
        }
    }

    fn load_icon_image(filename: &str) -> Option<slint::Image> {
        let exe = std::env::current_exe().ok()?;
        let path = exe.parent()?.join("icons").join(filename);
        slint::Image::load_from_path(&path).ok()
    }

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

    // ── Window creation & tick ────────────────────────────────────────────────

    /// Creates the history overlay window and starts its refresh timer. Must
    /// run on the Slint UI thread — see tray.rs's module doc.
    pub fn create_history_window(handle: Arc<AppHandle>) {
        tracing::info!("history overlay created, handle={:p}", Arc::as_ptr(&handle));
        let window = HistoryOverlayWindow::new().expect("create history overlay window");
        // NOT called here — see overlay.rs's create_alert_window for why
        // (the native window doesn't exist yet at creation time). Called on
        // first `.show()` below instead.

        let mut state = HistoryState::new(&handle);
        let cfg_x_y = {
            let cfg = handle.config.lock().unwrap();
            (cfg.overlay_history.x, cfg.overlay_history.y)
        };
        if cfg_x_y.0 >= 0 && cfg_x_y.1 >= 0 {
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
                let Some(w) = weak.upgrade() else { return };
                use slint::winit_030::WinitWindowAccessor;
                let _ = w.window().with_winit_window(|winit_window| {
                    let _ = winit_window.drag_window();
                });
                overlay_shell::overlay_shell::handle_drag_end(
                    weak.clone(),
                    Arc::clone(&handle),
                    OverlayKind::History,
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
                if state.tick_count.is_multiple_of(25) {
                    tracing::info!(
                        "history overlay heartbeat: force_show={}, visible={}, handle={:p}",
                        state.force_show,
                        state.visible,
                        Arc::as_ptr(&state.handle)
                    );
                }
                state.sync_config();
                ui.set_locked(state.locked);
                ui.set_force_show(state.force_show);

                // Only this window trims/renders `overlay_history` while
                // `Separate` — see `HistoryState::alert_style`'s doc
                // comment. Skipped (not just hidden) while `Merged`, so this
                // window's own `max_entries` doesn't fight the merged
                // window's trimming of the same shared list.
                let raw_entries = if state.alert_style == crate::config::AlertStyle::Separate {
                    snapshot_and_trim(&state.handle, state.max_entries)
                } else {
                    Vec::new()
                };

                let last_arrived_secs = raw_entries.last().map(|e| e.5);
                let idle_timed_out = state.idle_secs > 0
                    && last_arrived_secs.is_some_and(|s| s > state.idle_secs as f32);
                let show = state.alert_style == crate::config::AlertStyle::Separate
                    && (state.force_show
                        || (state.history_enabled && !raw_entries.is_empty() && !idle_timed_out));
                if !show {
                    if state.visible {
                        tracing::info!("history overlay: hiding");
                        let _ = ui.hide();
                        state.visible = false;
                    }
                    return;
                }
                if !state.visible {
                    tracing::info!("history overlay: showing (force_show={})", state.force_show);
                    let _ = ui.show();
                    state.visible = true;
                    crate::overlay_draw::overlay_draw::hide_from_taskbar(weak.clone());
                    crate::overlay_draw::overlay_draw::set_no_activate(weak.clone());
                }
                // See `overlay_draw::reassert_topmost`'s doc comment.
                if state.tick_count.is_multiple_of(25) {
                    crate::overlay_draw::overlay_draw::reassert_topmost(ui.window());
                }

                // Newest entry (last pushed) renders at the top row.
                let mut rows: Vec<HistoryRow> = raw_entries
                    .iter()
                    .rev()
                    .map(
                        |(icon, color, message, message_color, border_color, secs_ago)| {
                            let icon_image = state.icon_for(icon);
                            HistoryRow {
                                icon_source: icon_image.clone().unwrap_or_default(),
                                has_icon: icon_image.is_some(),
                                icon_color: color_from_hex(color, (150, 150, 160)),
                                message: message.as_str().into(),
                                message_color: color_from_hex(message_color, DEFAULT_TEXT_RGB),
                                border_color: color_from_hex(border_color, DEFAULT_BORDER_RGB),
                                time_label: format!("{}s", *secs_ago as u64).into(),
                                row_alpha: (*secs_ago / FADE_IN_SECS).clamp(0.0, 1.0),
                            }
                        },
                    )
                    .collect();
                // Show All Windows on an otherwise-empty history: with zero
                // rows this window's content-driven height collapses to
                // just its own padding, leaving nothing on screen to find
                // or grab. A single "Drag me" row (matching the alert
                // overlay's own placeholder text) gives it real, visible
                // size and something to position around.
                if state.force_show && rows.is_empty() {
                    rows.push(HistoryRow {
                        icon_source: Default::default(),
                        has_icon: false,
                        icon_color: Color::from_rgb_u8(150, 150, 160),
                        message: "Drag me".into(),
                        message_color: color_from_hex("", DEFAULT_TEXT_RGB),
                        border_color: color_from_hex("", DEFAULT_BORDER_RGB),
                        time_label: "".into(),
                        row_alpha: 1.0,
                    });
                }
                ui.set_row_font_size(state.font_size);
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
