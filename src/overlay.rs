/// Fly-in alert overlay window.
///
/// Real Slint window (`ui/overlay_alert.slint`, `AlertOverlayWindow`)
/// replacing the old per-pixel-alpha `WS_EX_LAYERED` + `UpdateLayeredWindow`
/// popup. All business logic below (phase state machine, priority queue,
/// TTS, animation math) is preserved from the Win32 version essentially
/// unchanged — only the "paint a GDI DIB" step became "push properties to
/// the Slint window", driven by a `slint::Timer` on the UI thread instead
/// of a dedicated OS thread pumping its own Win32 message loop (Slint
/// windows must live on the thread that drives their event loop).
///
/// Not carried over in this pass (see ui/overlay_alert.slint's doc comment
/// for the parallel note on the Slint side): drag-to-move and per-frame
/// window resize/recenter around an anchor point. The window is created
/// once at a fixed size/position and stays there; the Windows tab's
/// Position X/Y fields still cover repositioning.
#[cfg(feature = "tray")]
#[allow(clippy::module_inception)]
pub mod overlay {
    use std::collections::HashMap;
    use std::sync::atomic::{AtomicBool, AtomicU8, Ordering};
    use std::sync::{Arc, Mutex};
    use std::time::{Duration, Instant};

    use slint::ComponentHandle;

    use crate::alert_engine::alert_engine::{ActiveAlert, AlertEngine, AlertPhase};
    use crate::config::AlertStyle;
    use crate::overlay_draw::overlay_draw::parse_hex_color;
    use crate::overlay_registry::overlay_registry::OverlayKind;
    use crate::overlay_shell;
    use crate::tray::tray::AppHandle;
    use crate::triggers::engine::Treatment;

    include!(concat!(env!("OUT_DIR"), "/overlay_alert.rs"));

    // ── Constants ─────────────────────────────────────────────────────────────

    const ANIM_INTERVAL_MS: u64 = 16;

    /// Vertical rise (px) during fly-in, easing to 0 as the message settles.
    const RISE_PX: f32 = 26.0;
    /// Fraction of the vertical distance toward the history window's anchor
    /// travelled during shrink-out (the remainder is covered by the fade
    /// completing). Horizontal position stays fixed — the message shrinks
    /// and fades in place, matching fly-in's own centered growth, rather
    /// than drifting sideways.
    const TRAVEL_FRACTION: f32 = 0.35;
    const VIBRATE_PX: f32 = 3.0;
    const PULSE_AMOUNT: f32 = 0.07;

    /// Default text fill/stroke when a trigger doesn't override them.
    const DEFAULT_TEXT_RGB: (u8, u8, u8) = (255, 255, 255);
    const DEFAULT_BORDER_RGB: (u8, u8, u8) = (0, 0, 0);

    // ── State ─────────────────────────────────────────────────────────────────

    /// Window-side state: what to show and how big, driven each tick by the
    /// shared `AlertEngine`'s phase/queue decisions. See `alert_engine.rs`
    /// for the queue/priority/TTS/phase-timing logic this window and the
    /// merged alert+history window (`overlay_merged.rs`) both drive.
    struct OverlayState {
        handle: Arc<AppHandle>,
        engine: AlertEngine,
        overlay_enabled: bool,
        /// Only this window drains `AlertEngine`'s alert queue when this is
        /// `Separate` — when `Merged`, the combined alert+history window
        /// (`overlay_merged.rs`) owns that role instead, and this window
        /// stays dormant/hidden even if `overlay_enabled` is true. See
        /// `overlay_registry.rs`'s `OverlayKind` doc comment.
        alert_style: AlertStyle,
        start_pt: i32,
        max_pt: i32,
        alpha: u8,
        visible: bool,
        force_show: bool,
        locked: bool,
        hide_inactive_secs: u32,
        icon_cache: HashMap<String, Option<slint::Image>>,
        tick_count: u64,
    }

    impl OverlayState {
        fn new(handle: &Arc<AppHandle>) -> Self {
            // Constructed before taking `cfg` below: `AlertEngine::new`
            // locks `handle.config` itself, and `std::sync::Mutex` isn't
            // reentrant — calling it while `cfg` was already held here
            // deadlocked this thread (and with it the whole app, since
            // every overlay window is created sequentially on this same
            // thread before the event loop starts) on every launch.
            let engine = AlertEngine::new(handle, false);
            let cfg = handle.config.lock().unwrap();
            Self {
                handle: Arc::clone(handle),
                engine,
                overlay_enabled: cfg.overlay_alert.enabled,
                alert_style: cfg.alert_style,
                start_pt: cfg.overlay_start_font_size.max(6) as i32,
                max_pt: cfg.overlay_max_font_size.max(6) as i32,
                alpha: cfg.overlay_alpha,
                visible: false,
                force_show: handle.force_show_windows.load(Ordering::Relaxed),
                locked: cfg.overlay_alert.locked,
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
            self.overlay_enabled = cfg.overlay_alert.enabled;
            self.alert_style = cfg.alert_style;
            self.start_pt = cfg.overlay_start_font_size.max(6) as i32;
            self.max_pt = cfg.overlay_max_font_size.max(6) as i32;
            self.alpha = cfg.overlay_alpha;
            self.locked = cfg.overlay_alert.locked;
            self.hide_inactive_secs = cfg.overlay_hide_inactive_secs;
            drop(cfg);
            self.engine.sync_config();
            let new_force_show = self.handle.force_show_windows.load(Ordering::Relaxed);
            if new_force_show != self.force_show {
                tracing::info!(
                    "alert overlay: force_show {} -> {}",
                    self.force_show,
                    new_force_show
                );
            }
            self.force_show = new_force_show;
        }
    }

    // ── Animation math ────────────────────────────────────────────────────────

    /// Ease-out-cubic: `t` in `[0, 1]` -> eased progress in `[0, 1]`.
    fn ease_out_cubic(t: f32) -> f32 {
        let t = t.clamp(0.0, 1.0);
        1.0 - (1.0 - t).powi(3)
    }

    /// Everything needed to render one frame of the active alert, resolved
    /// from its current phase/elapsed time. Pushed straight into the Slint
    /// window's properties by the timer tick below. Also reused by
    /// `overlay_merged.rs`'s top "incoming alert" slot, which drives the
    /// same phase math against different Slint properties.
    pub(crate) struct FrameParams {
        pub font_pt: f32,
        pub alpha: f32,
        pub x_offset: f32,
        pub y_offset: f32,
        pub glow: bool,
        pub glow_pulse: f32,
    }

    pub(crate) fn resolve_frame(
        active: &ActiveAlert,
        start_pt: i32,
        max_pt: i32,
        fly_ms: u32,
        now: Instant,
    ) -> FrameParams {
        let elapsed = now.duration_since(active.phase_started);
        let fly_ms = fly_ms.max(1) as f32;
        match active.phase {
            AlertPhase::FlyIn => {
                let t = (elapsed.as_secs_f32() * 1000.0) / fly_ms;
                let ease = ease_out_cubic(t);
                FrameParams {
                    font_pt: start_pt as f32 + (max_pt - start_pt) as f32 * ease,
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
                let mut font_pt = max_pt as f32;
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
                        font_pt = max_pt as f32 * (1.0 + PULSE_AMOUNT * wobble);
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
                // Stand-in travel target toward the history window: the exact
                // on-screen history position isn't tracked cross-window in
                // this pass (see overlay_alert.slint's doc comment). Vertical
                // only — mirrors fly-in's horizontally-centered growth (see
                // overlay_alert.slint's `wrap` Rectangle, which recenters
                // itself on the message's current width every frame) instead
                // of also drifting sideways.
                let travel_dy = 200.0;
                FrameParams {
                    font_pt: max_pt as f32 - (max_pt - start_pt) as f32 * ease,
                    alpha: 1.0 - ease,
                    x_offset: 0.0,
                    y_offset: travel_dy * TRAVEL_FRACTION * ease,
                    glow: false,
                    glow_pulse: 0.0,
                }
            }
        }
    }

    // ── Window creation & tick ────────────────────────────────────────────────

    /// Creates the alert overlay window and starts its animation timer. Must
    /// run on the Slint UI thread — see tray.rs's module doc.
    pub fn create_alert_window(handle: Arc<AppHandle>) {
        tracing::info!("alert overlay created, handle={:p}", Arc::as_ptr(&handle));
        let window = AlertOverlayWindow::new().expect("create alert overlay window");
        window.set_has_icon(false);
        // See overlay_alert.slint's `panel-fallback` doc comment: X11 without
        // a compositor doesn't blend this window's alpha, so its "transparent"
        // background shows as opaque black. Windows' DWM always composites,
        // so this stays off there. On Linux, only turn it on when there's
        // actually no compositor running (checked once, at creation, via the
        // same gdk_screen_is_composited() call tray.rs uses for its warning
        // dialog) — forcing it on for all of Linux drew an unwanted black
        // rounded-rect behind the alert text even on setups that composite
        // fine (Mutter/KWin/picom).
        #[cfg(target_os = "linux")]
        window.set_panel_fallback(!crate::tray::tray::is_composited());
        // NOT called here — `with_winit_window` (inside `hide_from_taskbar`)
        // silently no-ops until the native window actually exists, which
        // winit only guarantees once the event loop has processed it (see
        // that trait's own doc comment). At creation time, before this
        // window has ever been `.show()`n, there's nothing for it to find
        // yet. Called instead on first `.show()` below.

        let mut state = OverlayState::new(&handle);
        let cfg_x_y = {
            let cfg = handle.config.lock().unwrap();
            (cfg.overlay_alert.x, cfg.overlay_alert.y)
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

        // Drag-to-move: the window has no title bar to drag by (no-frame),
        // so overlay_alert.slint's TouchArea reports the press and we hand
        // it straight to the OS's own interactive-move loop
        // (WinitWindowAccessor::with_winit_window + drag_window), same
        // mechanism a real title bar uses.
        window.on_drag_requested({
            let weak = window.as_weak();
            let handle = Arc::clone(&handle);
            move || {
                overlay_shell::overlay_shell::begin_drag(
                    weak.clone(),
                    Arc::clone(&handle),
                    OverlayKind::Alert,
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
                if state.tick_count.is_multiple_of(300) {
                    tracing::info!(
                        "alert overlay heartbeat: force_show={}, visible={}, handle={:p}",
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
                let fly_ms = state.engine.fly_ms();
                // Log-inactivity hide wins over everything else, including
                // Show All Windows — see `AppHandle::log_inactive`'s doc
                // comment. Checked *before* ticking the engine: `tick()`
                // drains the shared queue and advances whatever's active
                // through its full fly-in/hold/shrink-out lifecycle on
                // wall-clock time alone, with no awareness of whether this
                // window is actually on screen to show it. Ticking it while
                // hidden let a message that arrived during the sleep window
                // play all the way through and land in history without ever
                // being drawn — by the time log activity resumed and the
                // overlay woke back up, it was already gone. Skipping
                // `tick()` here instead freezes the queue/active-alert state
                // for the duration of the sleep, so it picks up untouched
                // once activity resumes.
                let log_inactive = state.handle.log_inactive(state.hide_inactive_secs);
                // Only drain/advance the shared alert queue while this
                // window is the active alert presentation — see
                // `OverlayState::alert_style`'s doc comment.
                let active = if !log_inactive && state.alert_style == AlertStyle::Separate {
                    state.engine.tick()
                } else {
                    None
                };

                let show = !log_inactive
                    && state.alert_style == AlertStyle::Separate
                    && (state.overlay_enabled || state.force_show)
                    && active.is_some();
                if !show {
                    if state.visible {
                        tracing::info!("alert overlay: hiding");
                        if let Err(e) = ui.hide() {
                            tracing::warn!("alert overlay: hide() failed: {e}");
                        }
                        state.visible = false;
                        // Slint's Property::set() only marks a property (and
                        // therefore anything depending on it) dirty when the
                        // new value differs from the old one — see
                        // i-slint-core's properties.rs. A placeholder's
                        // `alpha` is a constant 1.0 every tick it's shown,
                        // so the *next* show's `set_alpha(1.0)` would
                        // otherwise be a silent no-op against this same
                        // 1.0 leftover from before hiding — nothing marks
                        // dirty, nothing repaints, and the window can come
                        // back from `.show()` visually blank. Zeroing it
                        // here guarantees the next show's `set_alpha` is a
                        // genuine transition instead of a repeat.
                        ui.set_alpha(0.0);
                    }
                    return;
                }
                if !state.visible {
                    tracing::info!(
                        "alert overlay: showing (force_show={}, is_placeholder={})",
                        state.force_show,
                        active.map(|a| a.is_placeholder).unwrap_or(false)
                    );
                    crate::overlay_draw::overlay_draw::apply_saved_position(
                        &ui,
                        &state.handle,
                        crate::overlay_registry::overlay_registry::OverlayKind::Alert,
                    );
                    if let Err(e) = ui.show() {
                        tracing::warn!("alert overlay: show() failed: {e}");
                    }
                    state.visible = true;
                    tracing::info!(
                        "alert overlay: post-show slint-visible={}",
                        ui.window().is_visible()
                    );
                    crate::overlay_draw::overlay_draw::hide_from_taskbar(weak.clone());
                    crate::overlay_draw::overlay_draw::set_no_activate(weak.clone());
                    crate::overlay_draw::overlay_draw::force_repaint(weak.clone());
                }
                // See `overlay_draw::reassert_topmost`'s doc comment — Slint
                // only sets the OS-level topmost flag once, so this repeats
                // it periodically to recover from another window (a game,
                // a recorder) later taking the topmost slot back.
                if state.tick_count.is_multiple_of(300) {
                    crate::overlay_draw::overlay_draw::reassert_topmost(ui.window());
                }

                let now = Instant::now();
                let active = active.unwrap();
                let frame = resolve_frame(active, state.start_pt, state.max_pt, fly_ms, now);
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
                ui.set_font_size_pt(frame.font_pt.max(1.0));
                ui.set_alpha((frame.alpha * (state.alpha as f32 / 255.0)).clamp(0.0, 1.0));
                ui.set_x_offset(frame.x_offset);
                ui.set_y_offset(frame.y_offset);
                ui.set_glow(frame.glow);
                ui.set_glow_pulse(frame.glow_pulse);
            },
        );
        // Both must outlive this function, for the whole process: the timer
        // (Slint's Timer::drop deregisters it from the thread-local timer
        // list) and the window itself (its component is Rc-based and never
        // `.show()`n here — that's what would normally make Slint's runtime
        // hold a strong "keepalive" reference — so with nothing else holding
        // it, it would be dropped the moment this function returns, and
        // every tick's `weak.upgrade()` above would silently fail forever).
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

    // ── Sound playback ───────────────────────────────────────────────────────

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
    /// setting — used by the action editor's ▶ preview button, so users can
    /// audition a sound even while alerts are muted.
    pub(crate) fn preview_sound_label(label: &str) {
        if let Some(path) = resolve_sound_label(label) {
            play_sound_file(&path);
        }
    }

    /// Plays a raw file path unconditionally, ignoring the "Sound enabled"
    /// setting — used by the sound-label editor's Test button to audition a
    /// freshly-browsed file before it's saved as a label, so there's no
    /// manifest entry yet for `preview_sound_label` to resolve it through.
    pub(crate) fn preview_sound_path(path: &str) {
        play_sound_file(path);
    }

    pub(crate) fn play_sound(path: &str) {
        if !SOUND_ENABLED.load(Ordering::Relaxed) {
            return;
        }
        play_sound_file(path);
    }

    fn play_sound_file(path: &str) {
        // rodio (via cpal) talks to WASAPI on Windows and ALSA/PulseAudio on
        // Linux directly — no platform split needed here.
        let resolved = resolve_sound_path(path);
        let volume = SOUND_VOLUME_PERCENT.load(Ordering::Relaxed) as f32 / 100.0;
        std::thread::spawn(move || {
            let (_stream, handle) = match rodio::OutputStream::try_default() {
                Ok(s) => s,
                Err(e) => {
                    tracing::warn!("sound: failed to open default audio output device: {e:?}");
                    return;
                }
            };
            let sink = match rodio::Sink::try_new(&handle) {
                Ok(s) => s,
                Err(e) => {
                    tracing::warn!("sound: failed to create audio sink: {e:?}");
                    return;
                }
            };
            sink.set_volume(volume);
            let file = match std::fs::File::open(&resolved) {
                Ok(f) => f,
                Err(e) => {
                    tracing::warn!("sound: failed to open {resolved:?}: {e:?}");
                    return;
                }
            };
            let source = match rodio::Decoder::new(std::io::BufReader::new(file)) {
                Ok(s) => s,
                Err(e) => {
                    tracing::warn!("sound: failed to decode {resolved:?}: {e:?}");
                    return;
                }
            };
            tracing::warn!("sound: playing {resolved:?}, handing to audio sink");
            sink.append(source);
            sink.sleep_until_end();
            tracing::warn!("sound: {resolved:?} finished playing");
        });
    }

    fn resolve_sound_path(s: &str) -> String {
        if s.starts_with("sounds/") || s.starts_with("sounds\\") {
            let rest = s
                .trim_start_matches("sounds/")
                .trim_start_matches("sounds\\");
            let p = crate::assets::sounds_dir().join(rest);
            if p.exists() {
                return p.to_string_lossy().into_owned();
            }
        }
        s.to_string()
    }
}
