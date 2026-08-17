/// Real backend for the Slint Settings dialog, replacing
/// overlay_config_win.rs. The Settings window's UI thread must create it
/// (see tray.rs's module doc) — this module is only ever entered via
/// `tray::open_or_raise_settings`, which is itself only ever reached
/// through `invoke_from_event_loop`, so that invariant holds without
/// needing to re-check it here.
///
/// `SettingsWindow` is the only top-level Slint window this module creates
/// — add/edit flows (trigger, condition, action, sound label, log profile)
/// that used to each pop a separate window now live in
/// an embedded "drawer" panel inside this same window instead (see the
/// `PanelFrame` stack below, and settings_shell.slint's drawer markup).
/// Shown via `.show()`, kept alive by Slint's own runtime, not by a
/// Rust-side strong reference — only a `Weak` is kept, in a thread-local,
/// purely so a second "Settings…" click can find the already-open window
/// again to switch tabs / raise it.
#[cfg(feature = "tray")]
#[allow(clippy::module_inception)]
pub mod settings_window {
    use std::cell::RefCell;
    use std::rc::Rc;
    use std::sync::atomic::Ordering;
    use std::sync::Arc;

    use slint::{Color, ComponentHandle, Model, ModelRc, SharedString, VecModel, Weak};

    use crate::config::{
        player_name_from_path, server_name_from_path, AlertStyle, Config, LogProfile, LogWatchMode,
        TtsAudioMode, TtsSpeed,
    };
    use crate::overlay_draw::overlay_draw::parse_hex_color;
    use crate::overlay_registry::overlay_registry::OverlayKind;
    use crate::tray::tray::{copy_to_clipboard, AppHandle};
    use crate::trigger_presets::effective_presets;
    use crate::triggers::engine::{
        Action, ChatChannel, Condition, ConditionLogic, MatchType, SoundMode, Treatment,
        TriggerConfig, TriggerDef, VarOp, VoicePriority,
    };

    // Each entry point compiled separately by build.rs; explicit `include!`
    // (not `slint::include_modules!()`) since this crate has more than one
    // .slint entry point sharing one build script — see build.rs's comment.
    // The five add/edit dialogs (trigger, condition, action, sound label,
    // log profile) used to be separate entry points here too;
    // they're now `ui/panels/*.slint` components imported into
    // settings_shell.slint's embedded drawer instead (see the `PanelFrame`
    // stack machinery below), so there's only ever this one entry point.
    include!(concat!(env!("OUT_DIR"), "/settings_shell.rs"));

    thread_local! {
        static SETTINGS_WINDOW: RefCell<Option<Weak<SettingsWindow>>> = const { RefCell::new(None) };
    }

    const DEFAULT_TEXT_RGB: (u8, u8, u8) = (255, 255, 255);
    const DEFAULT_ICON_SWATCH_RGB: (u8, u8, u8) = (200, 200, 210);
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

    fn color_to_hex(c: Color) -> String {
        format!("#{:02X}{:02X}{:02X}", c.red(), c.green(), c.blue())
    }

    fn str_model(items: &[String]) -> ModelRc<SharedString> {
        let v: Vec<SharedString> = items.iter().map(|s| s.as_str().into()).collect();
        ModelRc::new(VecModel::from(v))
    }

    /// Which color-preview property a pushed `ColorPicker` panel should
    /// One entry in the drawer's navigation stack (see settings_shell.slint's
    /// drawer markup) — replaces the six separate `open_*_editor` functions
    /// that used to each create a brand-new top-level window. Rust-only,
    /// never crosses the Slint boundary (`PanelFrame::kind_index` maps a
    /// frame to the `top-panel` int Slint uses to pick which panel is
    /// visible). `Trigger`/`Condition`/`Action` carry the same local
    /// `Rc<RefCell<Vec<_>>>` a nested Condition/Action editor mutates
    /// in-place that `open_trigger_editor` used to create per-session —
    /// unchanged in spirit, just handed down the stack instead of captured
    /// in a closure.
    ///
    /// No `ColorPicker` variant here anymore — Icon/Text/Border color used
    /// to each push one onto this same stack (a full-screen wheel panel);
    /// they now edit inline via `SlimColorPicker`, embedded directly in
    /// `ActionPanel` (Color Box mode) or a small popover off a swatch, so
    /// there's nothing left to navigate to.
    enum PanelFrame {
        Trigger {
            edit_index: Option<usize>,
            conditions: Rc<RefCell<Vec<Condition>>>,
            actions: Rc<RefCell<Vec<Action>>>,
        },
        Condition {
            conditions: Rc<RefCell<Vec<Condition>>>,
            edit_index: Option<usize>,
        },
        Action {
            actions: Rc<RefCell<Vec<Action>>>,
            edit_index: Option<usize>,
        },
        SoundLabel {
            on_ok: Box<dyn Fn(String, String)>,
        },
        LogProfile {
            on_ok: Box<dyn Fn(LogProfile)>,
        },
        // No fields: unlike the other panels, this one doesn't confirm
        // back into a parent form — downloads/deletes apply immediately
        // via their own callbacks (see `voice_catalog_rows`), and Close
        // just pops the drawer like Cancel does everywhere else.
        VoiceManager,
    }

    impl PanelFrame {
        fn kind_index(&self) -> i32 {
            match self {
                PanelFrame::Trigger { .. } => 0,
                PanelFrame::Condition { .. } => 1,
                PanelFrame::Action { .. } => 2,
                PanelFrame::SoundLabel { .. } => 3,
                PanelFrame::LogProfile { .. } => 4,
                PanelFrame::VoiceManager => 5,
            }
        }

        fn breadcrumb_label(&self) -> &'static str {
            match self {
                PanelFrame::Trigger { .. } => "Trigger",
                PanelFrame::Condition { .. } => "Condition",
                PanelFrame::Action { .. } => "Action",
                PanelFrame::SoundLabel { .. } => "Sound Label",
                PanelFrame::LogProfile { .. } => "Log Profile",
                PanelFrame::VoiceManager => "Manage Voices",
            }
        }
    }

    fn sync_drawer_to_top(window: &SettingsWindow, stack: &Rc<RefCell<Vec<PanelFrame>>>) {
        let s = stack.borrow();
        window.set_drawer_open(!s.is_empty());
        window.set_top_panel(s.last().map(|f| f.kind_index()).unwrap_or(-1));
        let crumbs: Vec<String> = s.iter().map(|f| f.breadcrumb_label().to_string()).collect();
        window.set_drawer_crumbs(str_model(&crumbs));
    }

    fn pop_panel(window: &SettingsWindow, stack: &Rc<RefCell<Vec<PanelFrame>>>) {
        stack.borrow_mut().pop();
        sync_drawer_to_top(window, stack);
    }

    /// Header breadcrumb segment `index` (0-based) was clicked — jump
    /// straight back to that level, discarding everything deeper. `index`
    /// is always `< stack.len()` (the Slint side only makes non-last
    /// segments clickable), so truncating to `index + 1` always leaves at
    /// least that one frame.
    fn jump_to_panel(window: &SettingsWindow, stack: &Rc<RefCell<Vec<PanelFrame>>>, index: i32) {
        let index = index.max(0) as usize;
        stack.borrow_mut().truncate(index + 1);
        sync_drawer_to_top(window, stack);
    }

    // ── Public entry points (called from tray.rs) ─────────────────────────────

    pub fn raise_settings(tab: i32) {
        SETTINGS_WINDOW.with(|c| {
            if let Some(w) = c.borrow().as_ref().and_then(|w| w.upgrade()) {
                w.set_current_tab(tab);
                let _ = w.show();
            }
        });
    }

    pub fn raise_settings_no_tab_change() {
        SETTINGS_WINDOW.with(|c| {
            if let Some(w) = c.borrow().as_ref().and_then(|w| w.upgrade()) {
                let _ = w.show();
            }
        });
    }

    /// Called from each overlay window's drag-to-move handler
    /// (overlay.rs/overlay_history.rs/overlay_dps.rs/overlay_merged.rs)
    /// right after saving a new position to Config, so the Appearance tab's
    /// Position X/Y fields reflect a drag immediately if Settings happens to
    /// already be open — previously they only ever got the dragged-to
    /// position on the next full `open_settings()`/`load_config()`, i.e. not
    /// until Settings was closed and reopened. A no-op (nothing to sync) if
    /// Settings isn't currently open. No-op, not an error, either way —
    /// dragging an overlay with Settings closed is the far more common case.
    pub fn sync_window_position(kind: OverlayKind, x: i32, y: i32) {
        SETTINGS_WINDOW.with(|c| {
            let Some(w) = c.borrow().as_ref().and_then(|w| w.upgrade()) else {
                return;
            };
            match kind {
                OverlayKind::Alert => {
                    w.set_win_overlay_pos_x(x.to_string().into());
                    w.set_win_overlay_pos_y(y.to_string().into());
                }
                OverlayKind::History => {
                    w.set_win_history_pos_x(x.to_string().into());
                    w.set_win_history_pos_y(y.to_string().into());
                }
                OverlayKind::Meter => {
                    w.set_win_meter_pos_x(x.to_string().into());
                    w.set_win_meter_pos_y(y.to_string().into());
                }
                OverlayKind::Merged => {
                    w.set_win_merged_pos_x(x.to_string().into());
                    w.set_win_merged_pos_y(y.to_string().into());
                }
            }
        });
    }

    pub fn open_settings(handle: Arc<AppHandle>, initial_tab: i32) {
        // Unlike the overlay windows, Settings keeps a normal taskbar entry
        // — see overlay_draw::hide_from_taskbar's doc comment for the
        // window-attributes hook this opts out of. The hook fires inside
        // `SettingsWindow::new()` itself (Slint applies it while building
        // the window adapter, not deferred until `.show()`), so the wrap
        // has to cover `new()`, not just `show()`.
        let window = crate::overlay_draw::overlay_draw::suppress_utility_window_hint(|| {
            SettingsWindow::new().expect("create settings window")
        });
        window.set_current_tab(initial_tab);
        window.set_pattern_presets(ModelRc::new(VecModel::from(build_pattern_preset_rows())));

        let trigger_cfg = Rc::new(RefCell::new(TriggerConfig::load()));
        let panel_stack: Rc<RefCell<Vec<PanelFrame>>> = Rc::new(RefCell::new(Vec::new()));

        load_config(&window, &handle);
        refresh_trigger_rows(&window, &trigger_cfg);
        wire_callbacks(&window, &handle, &trigger_cfg, &panel_stack);

        // The titlebar X is the only way to close Settings now (see
        // settings_shell.slint's pending-changes dialog doc comment) — if
        // any Save-gated field is dirty, veto the close and pop that dialog
        // instead of silently discarding the edit; otherwise close for real.
        // Also used to discard an in-progress drawer edit (trigger/
        // condition/action/etc.) — no separate veto needed for that any
        // more now that add/edit flows are an embedded drawer inside this
        // same window rather than a second top-level window that could be
        // orphaned (see common/modal-scrim.slint's doc comment).
        {
            let handle = Arc::clone(&handle);
            let weak = window.as_weak();
            window.window().on_close_requested(move || {
                let w = weak.upgrade().unwrap();
                if w.get_dirty() {
                    w.set_pending_changes_open(true);
                    return slint::CloseRequestResponse::KeepWindowShown;
                }
                finish_close_bookkeeping(&handle);
                slint::CloseRequestResponse::HideWindow
            });
        }

        SETTINGS_WINDOW.with(|c| *c.borrow_mut() = Some(window.as_weak()));
        tracing::info!("open_settings: about to show, dirty={}", window.get_dirty());
        window.show().expect("show settings window");

        // load_config's population above sets dirty-gated fields (e.g.
        // start-font-size) whose `changed` notifications Slint defers until
        // the window's first render, which happens inside/after `show()`
        // above — resetting `dirty` before that point gets silently
        // clobbered back to `true` once the deferred notifications flush.
        // Queue the real reset for the next event-loop tick so it runs
        // after that flush instead of racing it.
        {
            let weak = window.as_weak();
            let _ = slint::invoke_from_event_loop(move || {
                if let Some(w) = weak.upgrade() {
                    tracing::info!(
                        "open_settings: deferred dirty reset, was dirty={}",
                        w.get_dirty()
                    );
                    w.set_dirty(false);
                }
            });
        }
    }

    // ── Load Config -> Slint properties ───────────────────────────────────────

    /// Re-scans installed voices (bundled + downloaded) and repopulates the
    /// Voice tab's dropdown, selecting `preferred_id` if it's present (or
    /// showing it as raw text if not — same fallback `load_config` already
    /// used). Called from `load_config` with the saved config's voice, and
    /// from the Manage Voices panel's download/delete completion with
    /// whatever's currently selected — without this second call site, a
    /// freshly-downloaded voice doesn't appear in the dropdown until
    /// Settings is closed and reopened, since `load_config`'s scan only
    /// ever ran once per Settings session.
    fn refresh_voice_dropdown(window: &SettingsWindow, preferred_id: &str) {
        let voices = crate::tts::tts::enumerate_voices();
        let displays: Vec<String> = voices.iter().map(|(d, _)| d.clone()).collect();
        let ids: Vec<String> = voices.iter().map(|(_, id)| id.clone()).collect();
        window.set_tts_voice_options(str_model(&displays));
        window.set_tts_voice_ids(str_model(&ids));
        let display = voices
            .iter()
            .find(|(_, id)| id == preferred_id)
            .map(|(display, _)| display.clone())
            .unwrap_or_else(|| preferred_id.to_string());
        window.set_tts_voice(display.into());
    }

    /// `refresh_voice_dropdown`, preserving whatever's currently selected in
    /// the dropdown (resolved back to its id first) rather than pinning to
    /// the saved config — for refreshing after a download/delete mid-
    /// session, where the user's current pulldown choice may not be saved
    /// yet and shouldn't be clobbered back to the on-disk config value.
    fn refresh_voice_dropdown_preserving_selection(window: &SettingsWindow) {
        let current_id = tts_voice_id_for_display(window, &window.get_tts_voice())
            .unwrap_or_else(|| window.get_tts_voice().to_string());
        refresh_voice_dropdown(window, &current_id);
    }

    /// Looks up a voice's internal id from its display name, using the
    /// `tts-voice-options`/`tts-voice-ids` parallel lists `load_config`'s
    /// enumeration (above) populates — not a fresh `enumerate_voices()`
    /// call, just cheaper to reuse than a reason to avoid blocking. Returns
    /// `None` (rather than an empty-string id) if the display name isn't
    /// found, so callers can leave the previously-saved id alone instead of
    /// clobbering it.
    fn tts_voice_id_for_display(window: &SettingsWindow, display: &str) -> Option<String> {
        let options = window.get_tts_voice_options();
        let ids = window.get_tts_voice_ids();
        options
            .iter()
            .position(|d| d.as_str() == display)
            .and_then(|i| ids.row_data(i))
            .map(|s| s.to_string())
    }

    fn load_config(window: &SettingsWindow, handle: &Arc<AppHandle>) {
        let cfg = handle.config.lock().unwrap();

        // General tab.
        window.set_import_status("".into());
        window.set_import_in_progress(false);
        window.set_dynamic_config_status("".into());
        window.set_dynamic_config_in_progress(false);

        // Logging tab.
        refresh_log_profiles(window, &cfg);
        window.set_server_url(cfg.server_url.clone().unwrap_or_default().into());
        window.set_url_status("".into());
        window.set_url_test_in_progress(false);
        window.set_stream_id(
            cfg.stream_id
                .clone()
                .unwrap_or_else(|| "Not registered".into())
                .into(),
        );
        window.set_is_registered(cfg.is_registered());
        window.set_register_in_progress(false);
        window.set_password(cfg.stream_password.clone().unwrap_or_default().into());
        window.set_remote_logging(cfg.remote_logging_enabled);

        // Overlays tab.
        window.set_overlay_font(
            if cfg.overlay_font.is_empty() {
                "Segoe UI".to_string()
            } else {
                cfg.overlay_font.clone()
            }
            .into(),
        );
        window.set_start_font_size(cfg.overlay_start_font_size.to_string().into());
        window.set_max_font_size(cfg.overlay_max_font_size.to_string().into());
        window.set_fly_ms(cfg.overlay_fly_ms.to_string().into());
        window.set_hold_secs(cfg.overlay_hold_secs.to_string().into());
        window.set_alpha(cfg.overlay_alpha.to_string().into());
        window.set_history_font_size(cfg.overlay_history_font_size.to_string().into());
        window.set_history_idle_secs(cfg.overlay_history_idle_secs.to_string().into());
        window.set_history_max_entries(cfg.overlay_history_max_entries.to_string().into());
        window.set_history_width(cfg.overlay_history_width.to_string().into());
        window.set_merged_start_font_size(cfg.overlay_merged_start_font_size.to_string().into());
        window.set_merged_max_font_size(cfg.overlay_merged_max_font_size.to_string().into());
        window.set_merged_fly_ms(cfg.overlay_merged_fly_ms.to_string().into());
        window.set_merged_hold_secs(cfg.overlay_merged_hold_secs.to_string().into());
        window.set_merged_alpha(cfg.overlay_merged_alpha.to_string().into());
        window
            .set_merged_history_font_size(cfg.overlay_merged_history_font_size.to_string().into());
        window
            .set_merged_history_idle_secs(cfg.overlay_merged_history_idle_secs.to_string().into());
        window.set_merged_history_max_entries(
            cfg.overlay_merged_history_max_entries.to_string().into(),
        );

        // DPS Meter Overlay card (Overlays tab).
        window.set_meter_max_rows(cfg.meter_max_rows.to_string().into());
        window.set_meter_idle_secs(cfg.meter_idle_secs.to_string().into());
        window.set_meter_font_size(cfg.meter_font_size.to_string().into());
        window.set_meter_width(cfg.meter_width.to_string().into());

        // Voice tab.
        window.set_tts_enabled(cfg.tts_enabled);
        window.set_tts_speed(
            match cfg.tts_speed {
                TtsSpeed::Normal => "Normal (1x)",
                TtsSpeed::Fast => "Fast (1.2x)",
                TtsSpeed::Faster => "Faster (1.5x)",
                TtsSpeed::Fastest => "Fastest (2x)",
            }
            .into(),
        );
        window.set_tts_audio_mode(match cfg.tts_audio_mode {
            TtsAudioMode::SmartPriority => 0,
            TtsAudioMode::QueueAll => 1,
            TtsAudioMode::InterruptConstantly => 2,
        });
        window.set_tts_read_emergency(cfg.tts_read_emergency);
        window.set_tts_read_operational(cfg.tts_read_operational);
        window.set_tts_read_ambient(cfg.tts_read_ambient);
        // Voice enumeration is a synchronous local directory scan (bundled +
        // downloaded voice files) — no IPC daemon involved, unlike the old
        // `tts` crate's speech-dispatcher backend, so there's no wedge risk
        // here worth a background thread/timeout for.
        refresh_voice_dropdown(window, &cfg.tts_voice);

        // Windows tab.
        window.set_win_overlay_enabled(cfg.overlay_alert.enabled);
        window.set_win_overlay_locked(cfg.overlay_alert.locked);
        window.set_win_overlay_pos_x(cfg.overlay_alert.x.to_string().into());
        window.set_win_overlay_pos_y(cfg.overlay_alert.y.to_string().into());
        window.set_win_history_enabled(cfg.overlay_history.enabled);
        window.set_win_history_locked(cfg.overlay_history.locked);
        window.set_win_history_pos_x(cfg.overlay_history.x.to_string().into());
        window.set_win_history_pos_y(cfg.overlay_history.y.to_string().into());
        window.set_win_meter_enabled(cfg.overlay_meter.enabled);
        window.set_win_meter_locked(cfg.overlay_meter.locked);
        window.set_win_meter_pos_x(cfg.overlay_meter.x.to_string().into());
        window.set_win_meter_pos_y(cfg.overlay_meter.y.to_string().into());
        window.set_alert_style(match cfg.alert_style {
            AlertStyle::Separate => 0,
            AlertStyle::Merged => 1,
        });
        window.set_win_merged_enabled(cfg.overlay_merged.enabled);
        window.set_win_merged_locked(cfg.overlay_merged.locked);
        window.set_win_merged_pos_x(cfg.overlay_merged.x.to_string().into());
        window.set_win_merged_pos_y(cfg.overlay_merged.y.to_string().into());
        window.set_force_show_active(handle.force_show_windows.load(Ordering::Relaxed));

        // Sounds tab.
        window.set_sound_enabled(cfg.sound_enabled);
        window.set_sound_volume(cfg.sound_volume as f32);
        refresh_sound_packages(window, &cfg.sound_package);
        refresh_sound_labels(window, &cfg.sound_package);

        // Every `set_*` above walks the Save-gated fields through their
        // `changed` handlers, which flip `dirty` true on any value change
        // from the freshly-constructed window's defaults. Resetting `dirty`
        // here doesn't stick, though: Slint defers at least some of those
        // `changed` notifications (confirmed via logging — e.g.
        // start-font-size's fired 8ms after `show()`, not during this
        // synchronous population) until the window's first render, which
        // happens inside/after `show()`. The real reset lives in
        // `open_settings`, deferred via `invoke_from_event_loop` so it runs
        // after that first-render flush instead of being clobbered by it.
    }

    /// Refreshes the log-profile list, Auto-detect checkbox, and "Currently
    /// watching" status from `cfg`. Called on load and after every profile
    /// mutation (add/edit/delete/pin/mode toggle). `cfg.resolve_active_log_path()`
    /// stats every profile's file once here — fine since this only runs on a
    /// user-triggered action (opening Settings or clicking a profile button),
    /// never on a timer.
    fn refresh_log_profiles(window: &SettingsWindow, cfg: &Config) {
        let rows: Vec<String> = cfg
            .log_profiles
            .iter()
            .map(|p| {
                let filename = std::path::Path::new(&p.path)
                    .file_name()
                    .and_then(|f| f.to_str())
                    .unwrap_or(&p.path);
                format!("{}  —  {}", p.name, filename)
            })
            .collect();
        window.set_log_profile_rows(str_model(&rows));

        let is_auto = cfg.log_watch_mode == LogWatchMode::Auto;
        window.set_auto_detect_log(is_auto);

        let active = cfg.resolve_active_profile();
        let active_index = active
            .and_then(|active| cfg.log_profiles.iter().position(|p| p.path == active.path))
            .map(|i| i as i32)
            .unwrap_or(-1);
        window.set_selected_log_profile_index(active_index);

        window.set_watching_status(
            match active {
                Some(profile) => {
                    let display = format!(
                        "{}@{} ({})",
                        profile.effective_player(),
                        capitalize(&profile.effective_server()),
                        game_id_to_label(profile.game.as_deref().unwrap_or("eql")),
                    );
                    if is_auto {
                        format!("Auto-detected: {display}")
                    } else {
                        format!("Watching: {display}")
                    }
                }
                None if cfg.log_profiles.is_empty() => "No log profiles configured".to_string(),
                None => "No log file found yet — waiting for EQ to write one".to_string(),
            }
            .into(),
        );
    }

    /// Uppercases the first character, leaving the rest unchanged (e.g.
    /// "test" -> "Test") — used only for display; the stored value is kept
    /// as entered/detected.
    fn capitalize(s: &str) -> String {
        let mut chars = s.chars();
        match chars.next() {
            Some(first) => first.to_uppercase().collect::<String>() + chars.as_str(),
            None => String::new(),
        }
    }

    /// Applies a log-profile mutation, saves, and restarts the engine only if
    /// the resolved active path actually changed — mirrors the explicit
    /// restart-on-change pattern the remote-logging toggle already uses.
    fn mutate_log_profiles(handle: &Arc<AppHandle>, mutate: impl FnOnce(&mut Config)) -> Config {
        let mut cfg = handle.config.lock().unwrap();
        let old_active = cfg.resolve_active_log_path();
        mutate(&mut cfg);
        cfg.save();
        let new_active = cfg.resolve_active_log_path();
        if new_active != old_active {
            tracing::info!(
                "Log profile changed — restarting engine ({:?} -> {:?})",
                old_active,
                new_active
            );
            handle.restart.store(true, Ordering::Relaxed);
        }
        cfg.clone()
    }

    fn refresh_sound_packages(window: &SettingsWindow, active: &str) {
        let packages = crate::sound_packages::sound_packages::list_packages();
        window.set_sound_package_options(str_model(&packages));
        let selected = if packages.iter().any(|p| p == active) {
            active.to_string()
        } else {
            packages
                .first()
                .cloned()
                .unwrap_or_else(|| "default".into())
        };
        window.set_sound_package(selected.into());
    }

    fn refresh_sound_labels(window: &SettingsWindow, package: &str) {
        let mut labels = crate::sound_packages::sound_packages::load_manifest(package).labels;
        labels.sort_by(|a, b| a.name.cmp(&b.name));
        let rows: Vec<String> = labels
            .iter()
            .map(|entry| format!("{}  —  {}", entry.name, entry.file))
            .collect();
        window.set_sound_labels(str_model(&rows));
        window.set_selected_label_index(-1);
    }

    // ── Instant-save (checkboxes/sliders/radios/standalone pulldowns) ──────────
    //
    // Persists immediately on change rather than waiting for the Save
    // button — see settings_shell.slint's "Instant-save" section for which
    // fields qualify and why. Does *not* restart the engine — none of these
    // fields feed `run_engine_once` (tailer/parser/pusher), which only
    // cares about the active log profile/player/server credentials. Player
    // name is still a Save-gated text field; log profiles have their own
    // instant-save-with-explicit-restart path via `mutate_log_profiles`.
    // The one exception here is remote-logging, which gates
    // `Config::remote_ready()` directly — its handler below restarts
    // explicitly instead of this helper doing it for every field.
    fn instant_save(handle: &Arc<AppHandle>, mutate: impl FnOnce(&mut Config)) {
        let mut cfg = handle.config.lock().unwrap();
        mutate(&mut cfg);
        cfg.save();
    }

    // ── Save Slint properties -> Config ───────────────────────────────────────

    fn save_config(window: &SettingsWindow, handle: &Arc<AppHandle>, trigger_cfg: &TriggerConfig) {
        fn parse_or<T: std::str::FromStr>(s: &str, default: T) -> T {
            s.trim().parse().unwrap_or(default)
        }

        let mut cfg = handle.config.lock().unwrap();

        // Only these actually change what `run_engine_once` does (which
        // server/credentials to push to) — everything else on this dialog
        // (overlay look, TTS, sounds, window positions) is already read live
        // by the overlay/meter timers and doesn't need the engine to
        // restart. Log profile changes (path/player/server/game/which
        // profile is active) have their own instant-save-with-restart path
        // via `mutate_log_profiles`, since they're no longer part of this
        // Save-gated form. Restarting tears down and rebuilds `combat_state`
        // from scratch, which is exactly what was wiping the DPS meter and
        // history overlay on every Save, even for changes that had nothing
        // to do with logging.
        let old_server_url = cfg.server_url.clone();
        let old_remote_enabled = cfg.remote_logging_enabled;

        cfg.server_url = {
            let u = window.get_server_url().to_string();
            if u.is_empty() {
                None
            } else {
                Some(u)
            }
        };
        cfg.stream_password = {
            let p = window.get_password().to_string();
            if p.is_empty() {
                None
            } else {
                Some(p)
            }
        };
        cfg.remote_logging_enabled = window.get_remote_logging();

        cfg.overlay_font = window.get_overlay_font().to_string();
        cfg.overlay_start_font_size =
            parse_or(&window.get_start_font_size(), cfg.overlay_start_font_size);
        cfg.overlay_max_font_size =
            parse_or(&window.get_max_font_size(), cfg.overlay_max_font_size);
        cfg.overlay_fly_ms = parse_or(&window.get_fly_ms(), cfg.overlay_fly_ms);
        cfg.overlay_hold_secs = parse_or(&window.get_hold_secs(), cfg.overlay_hold_secs);
        cfg.overlay_alpha = parse_or(&window.get_alpha(), cfg.overlay_alpha);
        cfg.overlay_history_font_size = parse_or(
            &window.get_history_font_size(),
            cfg.overlay_history_font_size,
        );
        cfg.overlay_history_idle_secs = parse_or(
            &window.get_history_idle_secs(),
            cfg.overlay_history_idle_secs,
        );
        cfg.overlay_history_max_entries = parse_or(
            &window.get_history_max_entries(),
            cfg.overlay_history_max_entries,
        );
        cfg.overlay_history_width =
            parse_or(&window.get_history_width(), cfg.overlay_history_width);
        cfg.overlay_merged_start_font_size = parse_or(
            &window.get_merged_start_font_size(),
            cfg.overlay_merged_start_font_size,
        );
        cfg.overlay_merged_max_font_size = parse_or(
            &window.get_merged_max_font_size(),
            cfg.overlay_merged_max_font_size,
        );
        cfg.overlay_merged_fly_ms =
            parse_or(&window.get_merged_fly_ms(), cfg.overlay_merged_fly_ms);
        cfg.overlay_merged_hold_secs =
            parse_or(&window.get_merged_hold_secs(), cfg.overlay_merged_hold_secs);
        cfg.overlay_merged_alpha = parse_or(&window.get_merged_alpha(), cfg.overlay_merged_alpha);
        cfg.overlay_merged_history_font_size = parse_or(
            &window.get_merged_history_font_size(),
            cfg.overlay_merged_history_font_size,
        );
        cfg.overlay_merged_history_idle_secs = parse_or(
            &window.get_merged_history_idle_secs(),
            cfg.overlay_merged_history_idle_secs,
        );
        cfg.overlay_merged_history_max_entries = parse_or(
            &window.get_merged_history_max_entries(),
            cfg.overlay_merged_history_max_entries,
        );

        cfg.meter_max_rows = parse_or(&window.get_meter_max_rows(), cfg.meter_max_rows);
        cfg.meter_idle_secs = parse_or(&window.get_meter_idle_secs(), cfg.meter_idle_secs);
        cfg.meter_font_size = parse_or(&window.get_meter_font_size(), cfg.meter_font_size);
        cfg.meter_width = parse_or(&window.get_meter_width(), cfg.meter_width);

        cfg.tts_enabled = window.get_tts_enabled();
        cfg.tts_speed = match window.get_tts_speed().as_str() {
            "Fast (1.2x)" => TtsSpeed::Fast,
            "Faster (1.5x)" => TtsSpeed::Faster,
            "Fastest (2x)" => TtsSpeed::Fastest,
            _ => TtsSpeed::Normal,
        };
        cfg.tts_audio_mode = match window.get_tts_audio_mode() {
            1 => TtsAudioMode::QueueAll,
            2 => TtsAudioMode::InterruptConstantly,
            _ => TtsAudioMode::SmartPriority,
        };
        cfg.tts_read_emergency = window.get_tts_read_emergency();
        cfg.tts_read_operational = window.get_tts_read_operational();
        cfg.tts_read_ambient = window.get_tts_read_ambient();
        let voice_display = window.get_tts_voice().to_string();
        if let Some(id) = tts_voice_id_for_display(window, &voice_display) {
            cfg.tts_voice = id;
        }

        cfg.overlay_alert.enabled = window.get_win_overlay_enabled();
        cfg.overlay_alert.locked = window.get_win_overlay_locked();
        cfg.overlay_alert.x = parse_or(&window.get_win_overlay_pos_x(), cfg.overlay_alert.x);
        cfg.overlay_alert.y = parse_or(&window.get_win_overlay_pos_y(), cfg.overlay_alert.y);
        cfg.overlay_history.enabled = window.get_win_history_enabled();
        cfg.overlay_history.locked = window.get_win_history_locked();
        cfg.overlay_history.x = parse_or(&window.get_win_history_pos_x(), cfg.overlay_history.x);
        cfg.overlay_history.y = parse_or(&window.get_win_history_pos_y(), cfg.overlay_history.y);
        cfg.overlay_meter.enabled = window.get_win_meter_enabled();
        cfg.overlay_meter.locked = window.get_win_meter_locked();
        cfg.overlay_meter.x = parse_or(&window.get_win_meter_pos_x(), cfg.overlay_meter.x);
        cfg.overlay_meter.y = parse_or(&window.get_win_meter_pos_y(), cfg.overlay_meter.y);
        cfg.alert_style = match window.get_alert_style() {
            1 => AlertStyle::Merged,
            _ => AlertStyle::Separate,
        };
        cfg.overlay_merged.enabled = window.get_win_merged_enabled();
        cfg.overlay_merged.locked = window.get_win_merged_locked();
        cfg.overlay_merged.x = parse_or(&window.get_win_merged_pos_x(), cfg.overlay_merged.x);
        cfg.overlay_merged.y = parse_or(&window.get_win_merged_pos_y(), cfg.overlay_merged.y);

        cfg.sound_enabled = window.get_sound_enabled();
        cfg.sound_volume = window.get_sound_volume().round().clamp(0.0, 100.0) as u8;
        cfg.sound_package = window.get_sound_package().to_string();
        crate::overlay::overlay::set_sound_enabled(cfg.sound_enabled);
        crate::overlay::overlay::set_sound_volume_percent(cfg.sound_volume);
        crate::overlay::overlay::set_active_sound_package(&cfg.sound_package);

        let restart_needed =
            cfg.server_url != old_server_url || cfg.remote_logging_enabled != old_remote_enabled;

        cfg.save();
        trigger_cfg.save();
        // Reload the *existing* shared engine in place rather than
        // installing a fresh `TriggerEngine` here: `TriggerEngine` is
        // `Arc<Mutex<EngineInner>>`-backed, and main.rs's `eq-triggers`
        // thread holds its own clone of the engine created at startup,
        // captured directly in its closure — it never re-reads
        // `handle.trigger_engine` after that. Swapping this `Option` to a
        // brand-new `TriggerEngine::new(..)` would only be visible to
        // other readers of `handle.trigger_engine` (e.g. the "test
        // trigger" button), leaving the actual log-processing thread
        // running against the old triggers until restart. `.reload()`
        // mutates the shared `EngineInner` behind the existing `Arc`, so
        // both clones observe the change immediately.
        match handle.trigger_engine.lock().unwrap().as_ref() {
            Some(engine) => engine.reload(trigger_cfg),
            None => {
                tracing::warn!(
                    "Settings saved before trigger engine existed — trigger changes won't take effect until restart"
                );
            }
        }
        if restart_needed {
            tracing::info!("Settings saved — server-url/remote-logging changed, restarting engine");
            handle.restart.store(true, Ordering::Relaxed);
        } else {
            tracing::info!("Settings saved — no engine-relevant fields changed, not restarting");
        }
    }

    // Shared by every path that actually closes Settings (the titlebar X
    // when not dirty, and the pending-changes dialog's Save & Close /
    // Discard buttons when it is) — everything except the window's own
    // `.hide()`/the native `HideWindow` response, since the titlebar-X path
    // gets that for free by returning it to Slint instead of calling it
    // directly.
    fn finish_close_bookkeeping(handle: &Arc<AppHandle>) {
        handle.settings_open.store(false, Ordering::Relaxed);
        // Show All Windows is a Settings-session convenience for
        // positioning overlays, not a state meant to persist once Settings
        // itself is gone — turn it back off (and let the overlays return to
        // their normal enabled/idle-driven visibility) whenever Settings
        // closes, however it closes.
        handle.force_show_windows.store(false, Ordering::Relaxed);
        SETTINGS_WINDOW.with(|c| {
            c.borrow_mut().take();
        });
    }

    // ── Callback wiring ────────────────────────────────────────────────────────

    fn wire_callbacks(
        window: &SettingsWindow,
        handle: &Arc<AppHandle>,
        trigger_cfg: &Rc<RefCell<TriggerConfig>>,
        panel_stack: &Rc<RefCell<Vec<PanelFrame>>>,
    ) {
        // TEMPORARY diagnostic — see settings_shell.slint's `dirty-reason`
        // doc comment for what this is chasing and why it should come back
        // out once the culprit's found.
        window.on_dirty_changed(|dirty, reason| {
            tracing::info!("settings dirty-changed: dirty={dirty} last-field={reason:?}");
        });
        window.on_drawer_back({
            let weak = window.as_weak();
            let stack = panel_stack.clone();
            move || {
                let w = weak.upgrade().unwrap();
                pop_panel(&w, &stack);
            }
        });
        window.on_drawer_close_all({
            let weak = window.as_weak();
            let stack = panel_stack.clone();
            move || {
                let w = weak.upgrade().unwrap();
                stack.borrow_mut().clear();
                sync_drawer_to_top(&w, &stack);
            }
        });
        window.on_drawer_jump({
            let weak = window.as_weak();
            let stack = panel_stack.clone();
            move |index| {
                let w = weak.upgrade().unwrap();
                jump_to_panel(&w, &stack, index);
            }
        });

        // ── Drawer: panel "OK" handlers ─────────────────────────────────
        // One per panel (Cancel/back is generic — see on_drawer_back above
        // — but each panel commits its own fields differently).
        window.on_trigger_panel_ok({
            let weak = window.as_weak();
            let stack = panel_stack.clone();
            let trigger_cfg = trigger_cfg.clone();
            move || {
                let w = weak.upgrade().unwrap();
                let (edit_index, conditions, actions) = match stack.borrow().last() {
                    Some(PanelFrame::Trigger {
                        edit_index,
                        conditions,
                        actions,
                    }) => (*edit_index, conditions.clone(), actions.clone()),
                    _ => return,
                };
                let def = TriggerDef {
                    name: w.get_trigger_name().to_string(),
                    enabled: w.get_trigger_enabled(),
                    condition_logic: if w.get_condition_logic() == "ANY (OR)" {
                        ConditionLogic::Any
                    } else {
                        ConditionLogic::All
                    },
                    conditions: conditions.borrow().clone(),
                    actions: actions.borrow().clone(),
                };
                let mut tc = trigger_cfg.borrow_mut();
                match edit_index {
                    Some(i) if i < tc.triggers.len() => tc.triggers[i] = def,
                    _ => tc.triggers.push(def),
                }
                drop(tc);
                refresh_trigger_rows(&w, &trigger_cfg);
                pop_panel(&w, &stack);
            }
        });
        window.on_condition_panel_ok({
            let weak = window.as_weak();
            let stack = panel_stack.clone();
            move || {
                let w = weak.upgrade().unwrap();
                let (conditions, edit_index) = match stack.borrow().last() {
                    Some(PanelFrame::Condition {
                        conditions,
                        edit_index,
                    }) => (conditions.clone(), *edit_index),
                    _ => return,
                };
                let cond = if w.get_cond_type() == "Match (log line)" {
                    let match_type = match w.get_match_type().as_str() {
                        "Exact (substring)" => MatchType::Exact,
                        "Glob  (* ? {name})" => MatchType::Glob,
                        _ => MatchType::Regex,
                    };
                    Condition::Match {
                        match_type,
                        pattern: w.get_pattern().to_string(),
                    }
                } else if w.get_cond_type() == "Chat message" {
                    let match_type = match w.get_match_type().as_str() {
                        "Exact (substring)" => MatchType::Exact,
                        "Glob  (* ? {name})" => MatchType::Glob,
                        _ => MatchType::Regex,
                    };
                    Condition::Chat {
                        channel: chat_channel_from_label(&w.get_chat_channel()),
                        custom_channel: w.get_chat_custom_channel().to_string(),
                        match_type,
                        pattern: w.get_pattern().to_string(),
                    }
                } else {
                    let op = match w.get_var_op().as_str() {
                        "equals" => VarOp::Equals,
                        "gt (>)" => VarOp::Gt,
                        "gte (\u{2265})" => VarOp::Gte,
                        "lt (<)" => VarOp::Lt,
                        "lte (\u{2264})" => VarOp::Lte,
                        "matches" => VarOp::Matches,
                        _ => VarOp::Isset,
                    };
                    Condition::Var {
                        var_name: w.get_cond_var_name().to_string(),
                        op,
                        value: w.get_cond_var_value().to_string(),
                    }
                };
                match edit_index {
                    Some(i) if i < conditions.borrow().len() => conditions.borrow_mut()[i] = cond,
                    _ => conditions.borrow_mut().push(cond),
                }
                refresh_condition_rows(&w, &conditions);
                pop_panel(&w, &stack);
            }
        });
        window.on_action_panel_ok({
            let weak = window.as_weak();
            let stack = panel_stack.clone();
            move || {
                let w = weak.upgrade().unwrap();
                let (actions, edit_index) = match stack.borrow().last() {
                    Some(PanelFrame::Action {
                        actions,
                        edit_index,
                    }) => (actions.clone(), *edit_index),
                    _ => return,
                };
                let icon_items = build_icon_items();
                let action = match w.get_action_type().as_str() {
                    "Store variable" => Action::StoreVar {
                        var_name: w.get_action_var_name().to_string(),
                        value: w.get_action_var_value().to_string(),
                    },
                    "Voice Alert (TTS)" => Action::VoiceAlert {
                        tts_text: w.get_tts_text().to_string(),
                        priority: match w.get_voice_priority() {
                            0 => VoicePriority::Emergency,
                            2 => VoicePriority::Ambient,
                            _ => VoicePriority::Operational,
                        },
                    },
                    "Play Sound" => {
                        let slots = w.get_sound_slots();
                        let sounds: Vec<String> = (0..slots.row_count())
                            .filter_map(|j| slots.row_data(j))
                            .map(|s| s.to_string())
                            .filter(|s| !s.is_empty())
                            .collect();
                        Action::PlaySound {
                            sound: None,
                            sounds,
                            mode: if w.get_sound_mode() == "Sequential" {
                                SoundMode::Sequential
                            } else {
                                SoundMode::Random
                            },
                            delay_secs: w.get_delay_secs().trim().parse().unwrap_or(0.0),
                        }
                    }
                    _ => {
                        let icon = match w.get_icon_mode().as_str() {
                            "none" => String::new(),
                            "colorbox" => "colorbox".to_string(),
                            _ => icon_items
                                .get(w.get_icon_index().max(0) as usize)
                                .map(|i| i.key.clone())
                                .unwrap_or_default(),
                        };
                        Action::Overlay {
                            icon,
                            color: color_to_hex(w.get_icon_color_preview()),
                            message: w.get_message().to_string(),
                            message_color: color_to_hex(w.get_message_color_preview()),
                            border_color: color_to_hex(w.get_border_color_preview()),
                            delay_secs: w.get_delay_secs().trim().parse().unwrap_or(0.0),
                            treatment: match w.get_treatment().as_str() {
                                "Glow" => Treatment::Glow,
                                "Vibrate" => Treatment::Vibrate,
                                "Pulse" => Treatment::Pulse,
                                _ => Treatment::None,
                            },
                            priority: match w.get_overlay_priority().as_str() {
                                "Emergency (interrupts)" => VoicePriority::Emergency,
                                "Ambient (may drop)" => VoicePriority::Ambient,
                                _ => VoicePriority::Operational,
                            },
                        }
                    }
                };
                match edit_index {
                    Some(i) if i < actions.borrow().len() => actions.borrow_mut()[i] = action,
                    _ => actions.borrow_mut().push(action),
                }
                refresh_action_rows(&w, &actions);
                pop_panel(&w, &stack);
            }
        });
        // Icon (Color Box mode), Text, and Border color each get their own
        // hue/sat/val + sv-image, live-updated as the user drags
        // `SlimColorPicker`. Color changes apply the instant they're made.
        // Icon's Color Box mode (inside `IconComboBoxField`'s own popup) and
        // Text/Border's `ColorSwatchField` popover both now have a ✕/✓
        // cancel/confirm pair, but both are Slint-side only (Cancel restores
        // the pre-open hue/sat/val and re-fires `hsv-changed`, Confirm just
        // closes), so from here every drag still looks the same regardless
        // of how the popup is eventually dismissed. `remember_recent_color`
        // fires on every `hsv-changed` rather than once on close — there's
        // no reliable "closed" signal to hook from the Rust side for a
        // `close-on-click-outside` popup anyway. Slightly more entries than
        // one-per-edit, but `remember_recent_color` already dedupes by hex
        // and moves a repeat to the front, so a drag that crosses the same
        // color twice doesn't produce duplicate swatches (a cancelled drag
        // can leave its in-between colors in the recent list — harmless,
        // same dedupe applies next time).
        window.on_icon_hsv_changed({
            let weak = window.as_weak();
            move || {
                let w = weak.upgrade().unwrap();
                let (r, g, b) = hsv_to_rgb(w.get_icon_hue(), w.get_icon_sat(), w.get_icon_val());
                w.set_icon_color_preview(Color::from_rgb_u8(r, g, b));
                w.set_icon_sv_image(generate_sv_image(w.get_icon_hue()));
                remember_recent_color(&format!("#{r:02X}{g:02X}{b:02X}"));
            }
        });
        window.on_icon_recent_picked({
            let weak = window.as_weak();
            move |color| {
                let w = weak.upgrade().unwrap();
                let (h, s, v) = rgb_to_hsv(color.red(), color.green(), color.blue());
                w.set_icon_hue(h);
                w.set_icon_sat(s);
                w.set_icon_val(v);
                w.set_icon_color_preview(color);
                w.set_icon_sv_image(generate_sv_image(h));
            }
        });
        window.on_message_hsv_changed({
            let weak = window.as_weak();
            move || {
                let w = weak.upgrade().unwrap();
                let (r, g, b) = hsv_to_rgb(
                    w.get_message_hue(),
                    w.get_message_sat(),
                    w.get_message_val(),
                );
                w.set_message_color_preview(Color::from_rgb_u8(r, g, b));
                w.set_message_sv_image(generate_sv_image(w.get_message_hue()));
                remember_recent_color(&format!("#{r:02X}{g:02X}{b:02X}"));
            }
        });
        window.on_message_recent_picked({
            let weak = window.as_weak();
            move |color| {
                let w = weak.upgrade().unwrap();
                let (h, s, v) = rgb_to_hsv(color.red(), color.green(), color.blue());
                w.set_message_hue(h);
                w.set_message_sat(s);
                w.set_message_val(v);
                w.set_message_color_preview(color);
                w.set_message_sv_image(generate_sv_image(h));
            }
        });
        window.on_border_hsv_changed({
            let weak = window.as_weak();
            move || {
                let w = weak.upgrade().unwrap();
                let (r, g, b) =
                    hsv_to_rgb(w.get_border_hue(), w.get_border_sat(), w.get_border_val());
                w.set_border_color_preview(Color::from_rgb_u8(r, g, b));
                w.set_border_sv_image(generate_sv_image(w.get_border_hue()));
                remember_recent_color(&format!("#{r:02X}{g:02X}{b:02X}"));
            }
        });
        window.on_border_recent_picked({
            let weak = window.as_weak();
            move |color| {
                let w = weak.upgrade().unwrap();
                let (h, s, v) = rgb_to_hsv(color.red(), color.green(), color.blue());
                w.set_border_hue(h);
                w.set_border_sat(s);
                w.set_border_val(v);
                w.set_border_color_preview(color);
                w.set_border_sv_image(generate_sv_image(h));
            }
        });
        window.on_icon_search_changed({
            let weak = window.as_weak();
            move |_text| {
                let w = weak.upgrade().unwrap();
                recompute_icon_visible_indices(&w);
            }
        });
        window.on_icon_source_changed({
            let weak = window.as_weak();
            move |_label| {
                let w = weak.upgrade().unwrap();
                recompute_icon_visible_indices(&w);
            }
        });
        window.on_sound_label_panel_ok({
            let weak = window.as_weak();
            let stack = panel_stack.clone();
            move || {
                let w = weak.upgrade().unwrap();
                let name = w.get_label_name().to_string();
                let path = w.get_sound_file_path().to_string();
                let frame = stack.borrow_mut().pop();
                if let Some(PanelFrame::SoundLabel { on_ok }) = frame {
                    if !name.is_empty() && !path.is_empty() {
                        on_ok(name, path);
                    }
                }
                sync_drawer_to_top(&w, &stack);
            }
        });
        window.on_log_profile_panel_ok({
            let weak = window.as_weak();
            let stack = panel_stack.clone();
            move || {
                let w = weak.upgrade().unwrap();
                let name = w.get_profile_name().to_string();
                let path = w.get_log_file_path().to_string();
                let player = w.get_player().to_string();
                if !player.is_empty() && !player.chars().all(|c| c.is_alphanumeric()) {
                    msgbox_simple("Log Profile", "Player name must be alphanumeric.");
                    return;
                }
                let server = w.get_server().to_string();
                let game = w.get_game().to_string();
                let public_stream = w.get_public_streaming();
                let frame = stack.borrow_mut().pop();
                if let Some(PanelFrame::LogProfile { on_ok }) = frame {
                    if !name.is_empty() && !path.is_empty() {
                        on_ok(LogProfile {
                            name,
                            path,
                            game: Some(label_to_game_id(&game).to_string()),
                            server: (!server.is_empty()).then_some(server),
                            player: (!player.is_empty()).then_some(player),
                            public_stream,
                        });
                    }
                }
                sync_drawer_to_top(&w, &stack);
            }
        });
        window.on_browse_sound_file({
            let weak = window.as_weak();
            move || {
                let weak = weak.clone();
                pick_sound_file_async(move |path| {
                    let Some(path) = path else { return };
                    let Some(w) = weak.upgrade() else { return };
                    if w.get_label_name().is_empty() {
                        w.set_label_name(
                            crate::sound_packages::sound_packages::label_from_stem(&path).into(),
                        );
                    }
                    w.set_sound_file_path(path.into());
                });
            }
        });
        window.on_test({
            let weak = window.as_weak();
            move || {
                let path = weak.upgrade().unwrap().get_sound_file_path().to_string();
                if !path.is_empty() {
                    crate::overlay::overlay::preview_sound_path(&path);
                }
            }
        });
        window.on_browse_log_file({
            let weak = window.as_weak();
            move || {
                let weak = weak.clone();
                pick_log_file_async(move |path| {
                    let Some(path) = path else { return };
                    let Some(w) = weak.upgrade() else { return };
                    let player = player_name_from_path(&path);
                    let server = server_name_from_path(&path);
                    if w.get_profile_name().is_empty() {
                        let name = match (&player, &server) {
                            (Some(p), Some(s)) => format!("{p}  ({s})"),
                            _ => std::path::Path::new(&path)
                                .file_stem()
                                .and_then(|f| f.to_str())
                                .unwrap_or("Log Profile")
                                .to_string(),
                        };
                        w.set_profile_name(name.into());
                    }
                    if w.get_server().is_empty() {
                        w.set_server(server.unwrap_or_default().into());
                    }
                    if w.get_player().is_empty() {
                        w.set_player(player.unwrap_or_default().into());
                    }
                    w.set_log_file_path(path.into());
                });
            }
        });

        // ── Drawer: nested condition/action editing ─────────────────────
        // Wired once here (not per-push, unlike the old per-window
        // closures) and look up the currently-open Trigger session's
        // `conditions`/`actions` Rc from the stack top at call time — safe
        // because only the topmost panel's buttons are ever visible/
        // clickable, so TriggerPanel's own buttons can only fire while a
        // `PanelFrame::Trigger` is genuinely on top.
        window.on_add_condition({
            let weak = window.as_weak();
            let stack = panel_stack.clone();
            move || {
                let w = weak.upgrade().unwrap();
                let conditions = match stack.borrow().last() {
                    Some(PanelFrame::Trigger { conditions, .. }) => conditions.clone(),
                    _ => return,
                };
                push_condition_panel(&w, &stack, conditions, None);
            }
        });
        window.on_edit_condition({
            let weak = window.as_weak();
            let stack = panel_stack.clone();
            move || {
                let w = weak.upgrade().unwrap();
                let idx = w.get_selected_condition_index();
                if idx < 0 {
                    return;
                }
                let conditions = match stack.borrow().last() {
                    Some(PanelFrame::Trigger { conditions, .. }) => conditions.clone(),
                    _ => return,
                };
                push_condition_panel(&w, &stack, conditions, Some(idx as usize));
            }
        });
        window.on_delete_condition({
            let weak = window.as_weak();
            let stack = panel_stack.clone();
            move || {
                let w = weak.upgrade().unwrap();
                let idx = w.get_selected_condition_index();
                let conditions = match stack.borrow().last() {
                    Some(PanelFrame::Trigger { conditions, .. }) => conditions.clone(),
                    _ => return,
                };
                if idx >= 0 && (idx as usize) < conditions.borrow().len() {
                    conditions.borrow_mut().remove(idx as usize);
                    w.set_selected_condition_index(-1);
                    refresh_condition_rows(&w, &conditions);
                }
            }
        });
        window.on_move_condition_up({
            let weak = window.as_weak();
            let stack = panel_stack.clone();
            move || {
                let w = weak.upgrade().unwrap();
                let idx = w.get_selected_condition_index();
                let conditions = match stack.borrow().last() {
                    Some(PanelFrame::Trigger { conditions, .. }) => conditions.clone(),
                    _ => return,
                };
                if idx > 0 {
                    conditions.borrow_mut().swap(idx as usize, idx as usize - 1);
                    w.set_selected_condition_index(idx - 1);
                    refresh_condition_rows(&w, &conditions);
                }
            }
        });
        window.on_move_condition_down({
            let weak = window.as_weak();
            let stack = panel_stack.clone();
            move || {
                let w = weak.upgrade().unwrap();
                let idx = w.get_selected_condition_index();
                let conditions = match stack.borrow().last() {
                    Some(PanelFrame::Trigger { conditions, .. }) => conditions.clone(),
                    _ => return,
                };
                let len = conditions.borrow().len() as i32;
                if idx >= 0 && idx < len - 1 {
                    conditions.borrow_mut().swap(idx as usize, idx as usize + 1);
                    w.set_selected_condition_index(idx + 1);
                    refresh_condition_rows(&w, &conditions);
                }
            }
        });
        window.on_add_action({
            let weak = window.as_weak();
            let stack = panel_stack.clone();
            let trigger_cfg = trigger_cfg.clone();
            move || {
                let w = weak.upgrade().unwrap();
                let actions = match stack.borrow().last() {
                    Some(PanelFrame::Trigger { actions, .. }) => actions.clone(),
                    _ => return,
                };
                push_action_panel(&w, &stack, actions, None, &trigger_cfg.borrow());
            }
        });
        window.on_edit_action({
            let weak = window.as_weak();
            let stack = panel_stack.clone();
            let trigger_cfg = trigger_cfg.clone();
            move || {
                let w = weak.upgrade().unwrap();
                let idx = w.get_selected_action_index();
                if idx < 0 {
                    return;
                }
                let actions = match stack.borrow().last() {
                    Some(PanelFrame::Trigger { actions, .. }) => actions.clone(),
                    _ => return,
                };
                push_action_panel(
                    &w,
                    &stack,
                    actions,
                    Some(idx as usize),
                    &trigger_cfg.borrow(),
                );
            }
        });
        window.on_delete_action({
            let weak = window.as_weak();
            let stack = panel_stack.clone();
            move || {
                let w = weak.upgrade().unwrap();
                let idx = w.get_selected_action_index();
                let actions = match stack.borrow().last() {
                    Some(PanelFrame::Trigger { actions, .. }) => actions.clone(),
                    _ => return,
                };
                if idx >= 0 && (idx as usize) < actions.borrow().len() {
                    actions.borrow_mut().remove(idx as usize);
                    w.set_selected_action_index(-1);
                    refresh_action_rows(&w, &actions);
                }
            }
        });
        window.on_move_action_up({
            let weak = window.as_weak();
            let stack = panel_stack.clone();
            move || {
                let w = weak.upgrade().unwrap();
                let idx = w.get_selected_action_index();
                let actions = match stack.borrow().last() {
                    Some(PanelFrame::Trigger { actions, .. }) => actions.clone(),
                    _ => return,
                };
                if idx > 0 {
                    actions.borrow_mut().swap(idx as usize, idx as usize - 1);
                    w.set_selected_action_index(idx - 1);
                    refresh_action_rows(&w, &actions);
                }
            }
        });
        window.on_move_action_down({
            let weak = window.as_weak();
            let stack = panel_stack.clone();
            move || {
                let w = weak.upgrade().unwrap();
                let idx = w.get_selected_action_index();
                let actions = match stack.borrow().last() {
                    Some(PanelFrame::Trigger { actions, .. }) => actions.clone(),
                    _ => return,
                };
                let len = actions.borrow().len() as i32;
                if idx >= 0 && idx < len - 1 {
                    actions.borrow_mut().swap(idx as usize, idx as usize + 1);
                    w.set_selected_action_index(idx + 1);
                    refresh_action_rows(&w, &actions);
                }
            }
        });
        window.on_set_sound_slot({
            let weak = window.as_weak();
            move |i, s| {
                // Deferred: this callback runs from inside the sound-slots
                // Repeater's own event dispatch (a ComboBoxField row click),
                // so reassigning `sound-slots` synchronously here re-enters
                // that same Repeater's RefCell and panics
                // ("RefCell already mutably borrowed", i-slint-core
                // repeater.rs). Posting to the event loop lets the click
                // dispatch unwind first.
                let weak = weak.clone();
                let _ = slint::invoke_from_event_loop(move || {
                    let Some(w) = weak.upgrade() else { return };
                    let mut v = read_sound_slots(&w);
                    if let Some(slot) = v.get_mut(i as usize) {
                        *slot = s.to_string();
                    }
                    w.set_sound_slots(str_model(&v));
                });
            }
        });
        window.on_add_sound_slot({
            let weak = window.as_weak();
            move || {
                // Deferred for the same re-entrancy reason as on_set_sound_slot.
                let weak = weak.clone();
                let _ = slint::invoke_from_event_loop(move || {
                    let Some(w) = weak.upgrade() else { return };
                    let mut v = read_sound_slots(&w);
                    // Blank, not preselected — an unpicked row is filtered out
                    // on OK (see on_action_panel_ok's "Play Sound" arm) rather
                    // than silently saving whatever sound happened to be first
                    // in the list.
                    v.push(String::new());
                    w.set_sound_slots(str_model(&v));
                });
            }
        });
        window.on_remove_sound_slot({
            let weak = window.as_weak();
            move |i| {
                // Deferred for the same re-entrancy reason as on_set_sound_slot.
                let weak = weak.clone();
                let _ = slint::invoke_from_event_loop(move || {
                    let Some(w) = weak.upgrade() else { return };
                    let mut v = read_sound_slots(&w);
                    if v.len() > 1 && (i as usize) < v.len() {
                        v.remove(i as usize);
                    }
                    w.set_sound_slots(str_model(&v));
                });
            }
        });
        window.on_test_sound_slot({
            let weak = window.as_weak();
            move |i| {
                let w = weak.upgrade().unwrap();
                if let Some(label) = w.get_sound_slots().row_data(i as usize) {
                    if !label.is_empty() {
                        crate::overlay::overlay::preview_sound_label(&label);
                    }
                }
            }
        });
        window.on_test_tts({
            let weak = window.as_weak();
            move || crate::tts::tts::preview_speak(&weak.upgrade().unwrap().get_tts_text())
        });
        window.on_test_voice(|| {
            // tts-voice/tts-speed are instant-saved to Config on change (see
            // on_instant_save_tts_voice/-speed below), so preview_speak's
            // fresh Config::load() already reflects the pulldown's current
            // selection without needing it threaded through here.
            const TEST_PHRASES: &[&str] = &[
                "Froklog: From the depths of Guk to the top of the parse.",
                "Froklog: Hopping reflexes, pinpoint parses, never miss a croak.",
                "Froklog: Blessed by Marr, tuned for the parse.",
                "Froklog: A giant leap for raid awareness.",
            ];
            static NEXT_PHRASE: std::sync::atomic::AtomicUsize =
                std::sync::atomic::AtomicUsize::new(0);
            let i =
                NEXT_PHRASE.fetch_add(1, std::sync::atomic::Ordering::Relaxed) % TEST_PHRASES.len();
            crate::tts::tts::preview_speak(TEST_PHRASES[i]);
        });
        window.on_manage_voices({
            let weak = window.as_weak();
            let stack = panel_stack.clone();
            move || {
                let w = weak.upgrade().unwrap();
                push_voice_manager_panel(&w, &stack);
            }
        });
        window.on_voice_catalog_download({
            let weak = window.as_weak();
            move |name| {
                tracing::warn!("voice catalog: download requested for {name:?}");
                let w = weak.upgrade().unwrap();
                start_voice_download(&w, name.to_string());
            }
        });
        window.on_voice_catalog_delete({
            let weak = window.as_weak();
            move |name| {
                tracing::warn!("voice catalog: delete requested for {name:?}");
                delete_voice(&name);
                let w = weak.upgrade().unwrap();
                refresh_voice_catalog(&w);
                refresh_voice_dropdown_preserving_selection(&w);
            }
        });

        window.on_save({
            let weak = window.as_weak();
            let handle = Arc::clone(handle);
            let trigger_cfg = trigger_cfg.clone();
            move || {
                let w = weak.upgrade().unwrap();
                save_config(&w, &handle, &trigger_cfg.borrow());
                // Save persists the whole form at once, so it's back in
                // sync with Config regardless of which field(s) triggered
                // `dirty` — unlike closing, Save doesn't close the window;
                // the user closes Settings from its own titlebar X.
                tracing::info!("on_save: save_config done, resetting dirty to false");
                w.set_dirty(false);
            }
        });
        // Pending-changes dialog's two closing choices (see
        // settings_shell.slint's doc comment on that dialog) — "Keep
        // Editing" is handled Slint-side only, it never reaches Rust.
        window.on_save_and_close_settings({
            let weak = window.as_weak();
            let handle = Arc::clone(handle);
            let trigger_cfg = trigger_cfg.clone();
            move || {
                let w = weak.upgrade().unwrap();
                save_config(&w, &handle, &trigger_cfg.borrow());
                w.set_dirty(false);
                finish_close_bookkeeping(&handle);
                let _ = w.hide();
            }
        });
        window.on_discard_and_close_settings({
            let weak = window.as_weak();
            let handle = Arc::clone(handle);
            move || {
                let w = weak.upgrade().unwrap();
                finish_close_bookkeeping(&handle);
                let _ = w.hide();
            }
        });

        // ── Instant-save ─────────────────────────────────────────────────
        // See settings_shell.slint's "Instant-save" section for which
        // fields these cover and why.
        window.on_instant_save_remote_logging({
            let weak = window.as_weak();
            let handle = Arc::clone(handle);
            move || {
                let w = weak.upgrade().unwrap();
                instant_save(&handle, |cfg| {
                    cfg.remote_logging_enabled = w.get_remote_logging();
                });
                // Unlike every other instant-saved field, this one gates
                // Config::remote_ready() — the pusher only starts/stops
                // when the engine monitor restarts and re-reads it.
                handle.restart.store(true, Ordering::Relaxed);
            }
        });
        window.on_instant_save_overlay_font({
            let weak = window.as_weak();
            let handle = Arc::clone(handle);
            move || {
                let w = weak.upgrade().unwrap();
                instant_save(&handle, |cfg| {
                    cfg.overlay_font = w.get_overlay_font().to_string();
                });
            }
        });
        window.on_instant_save_tts_enabled({
            let weak = window.as_weak();
            let handle = Arc::clone(handle);
            move || {
                let w = weak.upgrade().unwrap();
                instant_save(&handle, |cfg| {
                    cfg.tts_enabled = w.get_tts_enabled();
                });
            }
        });
        window.on_instant_save_tts_speed({
            let weak = window.as_weak();
            let handle = Arc::clone(handle);
            move || {
                let w = weak.upgrade().unwrap();
                instant_save(&handle, |cfg| {
                    cfg.tts_speed = match w.get_tts_speed().as_str() {
                        "Fast (1.2x)" => TtsSpeed::Fast,
                        "Faster (1.5x)" => TtsSpeed::Faster,
                        "Fastest (2x)" => TtsSpeed::Fastest,
                        _ => TtsSpeed::Normal,
                    };
                });
            }
        });
        window.on_instant_save_tts_audio_mode({
            let weak = window.as_weak();
            let handle = Arc::clone(handle);
            move || {
                let w = weak.upgrade().unwrap();
                instant_save(&handle, |cfg| {
                    cfg.tts_audio_mode = match w.get_tts_audio_mode() {
                        1 => TtsAudioMode::QueueAll,
                        2 => TtsAudioMode::InterruptConstantly,
                        _ => TtsAudioMode::SmartPriority,
                    };
                });
            }
        });
        window.on_instant_save_tts_verbosity({
            let weak = window.as_weak();
            let handle = Arc::clone(handle);
            move || {
                let w = weak.upgrade().unwrap();
                instant_save(&handle, |cfg| {
                    cfg.tts_read_emergency = w.get_tts_read_emergency();
                    cfg.tts_read_operational = w.get_tts_read_operational();
                    cfg.tts_read_ambient = w.get_tts_read_ambient();
                });
            }
        });
        window.on_instant_save_tts_voice({
            let weak = window.as_weak();
            let handle = Arc::clone(handle);
            move || {
                let w = weak.upgrade().unwrap();
                let voice_display = w.get_tts_voice().to_string();
                if let Some(id) = tts_voice_id_for_display(&w, &voice_display) {
                    instant_save(&handle, |cfg| {
                        cfg.tts_voice = id;
                    });
                }
            }
        });
        window.on_instant_save_win_overlay({
            let weak = window.as_weak();
            let handle = Arc::clone(handle);
            move || {
                let w = weak.upgrade().unwrap();
                instant_save(&handle, |cfg| {
                    cfg.overlay_alert.enabled = w.get_win_overlay_enabled();
                    cfg.overlay_alert.locked = w.get_win_overlay_locked();
                });
            }
        });
        window.on_instant_save_win_history({
            let weak = window.as_weak();
            let handle = Arc::clone(handle);
            move || {
                let w = weak.upgrade().unwrap();
                instant_save(&handle, |cfg| {
                    cfg.overlay_history.enabled = w.get_win_history_enabled();
                    cfg.overlay_history.locked = w.get_win_history_locked();
                });
            }
        });
        window.on_instant_save_win_meter({
            let weak = window.as_weak();
            let handle = Arc::clone(handle);
            move || {
                let w = weak.upgrade().unwrap();
                instant_save(&handle, |cfg| {
                    cfg.overlay_meter.enabled = w.get_win_meter_enabled();
                    cfg.overlay_meter.locked = w.get_win_meter_locked();
                });
            }
        });
        window.on_instant_save_win_merged({
            let weak = window.as_weak();
            let handle = Arc::clone(handle);
            move || {
                let w = weak.upgrade().unwrap();
                instant_save(&handle, |cfg| {
                    cfg.overlay_merged.enabled = w.get_win_merged_enabled();
                    cfg.overlay_merged.locked = w.get_win_merged_locked();
                });
            }
        });
        window.on_instant_save_alert_style({
            let weak = window.as_weak();
            let handle = Arc::clone(handle);
            move || {
                let w = weak.upgrade().unwrap();
                instant_save(&handle, |cfg| {
                    cfg.alert_style = match w.get_alert_style() {
                        1 => AlertStyle::Merged,
                        _ => AlertStyle::Separate,
                    };
                });
            }
        });
        window.on_instant_save_sound_enabled({
            let weak = window.as_weak();
            let handle = Arc::clone(handle);
            move || {
                let w = weak.upgrade().unwrap();
                let enabled = w.get_sound_enabled();
                instant_save(&handle, |cfg| cfg.sound_enabled = enabled);
                crate::overlay::overlay::set_sound_enabled(enabled);
            }
        });
        window.on_instant_save_sound_volume({
            let weak = window.as_weak();
            let handle = Arc::clone(handle);
            move || {
                let w = weak.upgrade().unwrap();
                let volume = w.get_sound_volume().round().clamp(0.0, 100.0) as u8;
                instant_save(&handle, |cfg| cfg.sound_volume = volume);
                crate::overlay::overlay::set_sound_volume_percent(volume);
            }
        });
        window.on_instant_save_sound_package({
            let weak = window.as_weak();
            let handle = Arc::clone(handle);
            move || {
                let w = weak.upgrade().unwrap();
                let package = w.get_sound_package().to_string();
                instant_save(&handle, |cfg| cfg.sound_package = package.clone());
                crate::overlay::overlay::set_active_sound_package(&package);
                refresh_sound_labels(&w, &package);
            }
        });

        // ── General tab ──────────────────────────────────────────────────
        window.on_import_spell_icons({
            let weak = window.as_weak();
            let handle = Arc::clone(handle);
            move || {
                let w = weak.upgrade().unwrap();
                let log_path = handle
                    .config
                    .lock()
                    .unwrap()
                    .resolve_active_log_path()
                    .unwrap_or_default();
                let Some(eq_dir) = (!log_path.is_empty())
                    .then(|| crate::spell_icons::spell_icons::eq_dir_from_log_path(&log_path))
                    .flatten()
                else {
                    w.set_import_status(
                        "Set a log file first (needs DIR\\Logs\\... to find DIR\\uifiles\\).".into(),
                    );
                    return;
                };
                w.set_import_in_progress(true);
                let weak2 = weak.clone();
                std::thread::spawn(move || {
                    let icons_dir = crate::assets::icons_dir();
                    let result = crate::spell_icons::spell_icons::extract_spell_icons(
                        &eq_dir,
                        &icons_dir,
                        crate::spell_icons::spell_icons::DEFAULT_CELL_SIZE,
                    );
                    let _ = slint::invoke_from_event_loop(move || {
                        if let Some(w) = weak2.upgrade() {
                            w.set_import_in_progress(false);
                            let status = if result.sheets_found.is_empty() {
                                if !result.searched_dir_exists {
                                    format!("No uifiles folder found at {}", result.searched_dir.display())
                                } else if result.dirs_scanned.is_empty() {
                                    format!(
                                        "No SpellsNN.tga sheets found under any uifiles subfolder ({})",
                                        result.searched_dir.display()
                                    )
                                } else {
                                    "No spell sheets could be read".into()
                                }
                            } else {
                                let naming = if result.spells_file_found {
                                    format!(", {} named from spells_us.txt", result.named)
                                } else {
                                    "; spells_us.txt not found in EQ dir, icons unnamed".to_string()
                                };
                                format!(
                                    "{} extracted, {} duplicates skipped ({} UI folder(s) scanned){naming}",
                                    result.extracted,
                                    result.duplicates_skipped,
                                    result.dirs_scanned.len()
                                )
                            };
                            w.set_import_status(status.into());
                        }
                    });
                });
            }
        });

        window.on_download_dynamic_config({
            let weak = window.as_weak();
            move || {
                let w = weak.upgrade().unwrap();
                w.set_dynamic_config_in_progress(true);
                w.set_dynamic_config_status("Downloading…".into());
                let weak2 = weak.clone();
                std::thread::spawn(move || {
                    let result = crate::trigger_presets::download_dynamic_config();
                    let _ = slint::invoke_from_event_loop(move || {
                        if let Some(w) = weak2.upgrade() {
                            w.set_dynamic_config_in_progress(false);
                            match result {
                                Ok(count) => {
                                    w.set_pattern_presets(ModelRc::new(VecModel::from(
                                        build_pattern_preset_rows(),
                                    )));
                                    w.set_dynamic_config_status(
                                        format!("Up to date — {count} presets loaded.").into(),
                                    );
                                }
                                Err(e) => {
                                    w.set_dynamic_config_status(
                                        format!("Download failed: {e}").into(),
                                    );
                                }
                            }
                        }
                    });
                });
            }
        });

        // ── Logging tab ──────────────────────────────────────────────────
        window.on_add_log_profile({
            let weak = window.as_weak();
            let stack = panel_stack.clone();
            let handle = Arc::clone(handle);
            move || {
                let w = weak.upgrade().unwrap();
                let handle = Arc::clone(&handle);
                let weak = weak.clone();
                push_log_profile_panel(&w, &stack, None, move |profile| {
                    let cfg = mutate_log_profiles(&handle, |cfg| {
                        cfg.log_profiles.push(profile);
                    });
                    if let Some(w) = weak.upgrade() {
                        refresh_log_profiles(&w, &cfg);
                    }
                });
            }
        });
        window.on_edit_log_profile({
            let weak = window.as_weak();
            let stack = panel_stack.clone();
            let handle = Arc::clone(handle);
            move || {
                let w = weak.upgrade().unwrap();
                let idx = w.get_selected_log_profile_index();
                let cfg = handle.config.lock().unwrap();
                let Some(old) = cfg.log_profiles.get(idx.max(0) as usize).cloned() else {
                    return;
                };
                drop(cfg);
                let handle = Arc::clone(&handle);
                let weak2 = weak.clone();
                let old_name = old.name.clone();
                push_log_profile_panel(&w, &stack, Some(old.clone()), move |profile| {
                    let cfg = mutate_log_profiles(&handle, |cfg| {
                        if let Some(p) = cfg.log_profiles.iter_mut().find(|p| p.name == old_name) {
                            *p = profile.clone();
                        }
                        if cfg.log_watch_mode == LogWatchMode::Pinned(old_name.clone()) {
                            cfg.log_watch_mode = LogWatchMode::Pinned(profile.name.clone());
                        }
                    });
                    if let Some(w) = weak2.upgrade() {
                        refresh_log_profiles(&w, &cfg);
                    }
                });
            }
        });
        window.on_delete_log_profile({
            let weak = window.as_weak();
            let handle = Arc::clone(handle);
            move || {
                let w = weak.upgrade().unwrap();
                let idx = w.get_selected_log_profile_index();
                if idx < 0 {
                    return;
                }
                let cfg = mutate_log_profiles(&handle, |cfg| {
                    if (idx as usize) >= cfg.log_profiles.len() {
                        return;
                    }
                    let removed = cfg.log_profiles.remove(idx as usize);
                    if cfg.log_watch_mode == LogWatchMode::Pinned(removed.name) {
                        cfg.log_watch_mode = LogWatchMode::Auto;
                    }
                });
                refresh_log_profiles(&w, &cfg);
            }
        });
        window.on_set_active_log_profile({
            let weak = window.as_weak();
            let handle = Arc::clone(handle);
            move || {
                let w = weak.upgrade().unwrap();
                let idx = w.get_selected_log_profile_index();
                if idx < 0 {
                    return;
                }
                let cfg = mutate_log_profiles(&handle, |cfg| {
                    if let Some(p) = cfg.log_profiles.get(idx as usize) {
                        cfg.log_watch_mode = LogWatchMode::Pinned(p.name.clone());
                    }
                });
                refresh_log_profiles(&w, &cfg);
            }
        });
        window.on_instant_save_auto_detect_log({
            let weak = window.as_weak();
            let handle = Arc::clone(handle);
            move || {
                let w = weak.upgrade().unwrap();
                let auto = w.get_auto_detect_log();
                let idx = w.get_selected_log_profile_index();
                let cfg = mutate_log_profiles(&handle, |cfg| {
                    cfg.log_watch_mode = if auto {
                        LogWatchMode::Auto
                    } else {
                        // Switching off Auto pins whatever was selected (falls
                        // back to the first profile if nothing was selected).
                        let pin = cfg
                            .log_profiles
                            .get(idx.max(0) as usize)
                            .or_else(|| cfg.log_profiles.first());
                        match pin {
                            Some(p) => LogWatchMode::Pinned(p.name.clone()),
                            None => LogWatchMode::Auto,
                        }
                    };
                });
                refresh_log_profiles(&w, &cfg);
            }
        });
        window.on_test_url({
            let weak = window.as_weak();
            move || {
                let w = weak.upgrade().unwrap();
                let url = w.get_server_url().to_string();
                if url.is_empty() {
                    w.set_url_status("Enter a server URL first.".into());
                    return;
                }
                w.set_url_status("Testing…".into());
                w.set_url_test_in_progress(true);
                let weak2 = weak.clone();
                std::thread::spawn(move || {
                    let result = test_url(&url);
                    let _ = slint::invoke_from_event_loop(move || {
                        if let Some(w) = weak2.upgrade() {
                            w.set_url_test_in_progress(false);
                            w.set_url_status(
                                match result {
                                    UrlTestResult::Connected {
                                        requires_password: false,
                                    } => "Connected — open registration".to_string(),
                                    UrlTestResult::Connected {
                                        requires_password: true,
                                    } => "Connected — password required".to_string(),
                                    UrlTestResult::Failed(e) => {
                                        format!("Could not reach server: {e}")
                                    }
                                }
                                .into(),
                            );
                        }
                    });
                });
            }
        });
        window.on_register_clicked({
            let weak = window.as_weak();
            let handle = Arc::clone(handle);
            move || {
                let w = weak.upgrade().unwrap();
                if w.get_is_registered() {
                    let mut cfg = handle.config.lock().unwrap();
                    cfg.stream_id = None;
                    cfg.stream_token = None;
                    cfg.view_token = None;
                    cfg.save();
                    drop(cfg);
                    handle.restart.store(true, Ordering::Relaxed);
                    w.set_is_registered(false);
                    w.set_stream_id("Not registered".into());
                    return;
                }
                let url = w.get_server_url().to_string();
                let active_profile = handle.config.lock().unwrap().resolve_active_profile().cloned();
                let Some(active_profile) = active_profile else {
                    msgbox_simple("Register", "Add a log profile first.");
                    return;
                };
                let player = active_profile.effective_player();
                if url.is_empty() || player.is_empty() {
                    msgbox_simple(
                        "Register",
                        "Enter a server URL (and test it); the active log profile needs a player name too.",
                    );
                    return;
                }
                let server = active_profile.effective_server();
                let game = active_profile
                    .game
                    .clone()
                    .unwrap_or_else(|| "eql".to_string());
                let password = w.get_password().to_string();
                let is_public = active_profile.public_stream;
                w.set_register_in_progress(true);
                let weak2 = weak.clone();
                let handle2 = Arc::clone(&handle);
                std::thread::spawn(move || {
                    let result = do_register(&url, &player, &server, &game, &password, is_public);
                    let _ = slint::invoke_from_event_loop(move || {
                        let Some(w) = weak2.upgrade() else { return };
                        w.set_register_in_progress(false);
                        match result {
                            RegisterResult::Ok {
                                stream_id,
                                stream_token,
                                view_token,
                            } => {
                                w.set_is_registered(true);
                                w.set_stream_id(stream_id.clone().into());
                                let mut cfg = handle2.config.lock().unwrap();
                                cfg.stream_id = Some(stream_id);
                                cfg.stream_token = Some(stream_token);
                                cfg.view_token = Some(view_token);
                                if !url.is_empty() {
                                    cfg.server_url = Some(url.clone());
                                }
                                cfg.save();
                                handle2.restart.store(true, Ordering::Relaxed);
                            }
                            RegisterResult::Err(e) => msgbox_simple("Registration failed", &e),
                        }
                    });
                });
            }
        });
        window.on_copy_stream_id({
            let weak = window.as_weak();
            move || copy_to_clipboard(&weak.upgrade().unwrap().get_stream_id())
        });

        // ── Windows tab reset buttons ──────────────────────────────────────
        window.on_overlay_reset_position({
            let weak = window.as_weak();
            move || {
                let w = weak.upgrade().unwrap();
                w.set_win_overlay_pos_x("-1".into());
                w.set_win_overlay_pos_y("-1".into());
            }
        });
        window.on_history_reset_position({
            let weak = window.as_weak();
            move || {
                let w = weak.upgrade().unwrap();
                w.set_win_history_pos_x("-1".into());
                w.set_win_history_pos_y("-1".into());
            }
        });
        window.on_meter_reset_position({
            let weak = window.as_weak();
            move || {
                let w = weak.upgrade().unwrap();
                w.set_win_meter_pos_x("-1".into());
                w.set_win_meter_pos_y("-1".into());
            }
        });
        window.on_merged_reset_position({
            let weak = window.as_weak();
            move || {
                let w = weak.upgrade().unwrap();
                w.set_win_merged_pos_x("-1".into());
                w.set_win_merged_pos_y("-1".into());
            }
        });
        window.on_show_all_windows({
            let weak = window.as_weak();
            let handle = Arc::clone(handle);
            move || {
                let w = weak.upgrade().unwrap();
                let now_active = !handle.force_show_windows.load(Ordering::Relaxed);
                handle
                    .force_show_windows
                    .store(now_active, Ordering::Relaxed);
                w.set_force_show_active(now_active);
                tracing::info!("show-all-windows: force_show_windows now={now_active}");
            }
        });

        // ── Sounds tab ──────────────────────────────────────────────────
        window.on_new_sound_package({
            let weak = window.as_weak();
            move || {
                let w = weak.upgrade().unwrap();
                // No text-entry dialog in this pass (Slint has no built-in
                // prompt-for-text primitive) — creates a uniquely-named
                // package rather than prompting for a custom name.
                let name =
                    crate::sound_packages::sound_packages::unique_package_name("New Package");
                if crate::sound_packages::sound_packages::clone_package("default", &name).is_ok() {
                    refresh_sound_packages(&w, &name);
                    refresh_sound_labels(&w, &name);
                }
            }
        });
        window.on_rename_sound_package({
            move || {
                msgbox_simple(
                    "Rename Package",
                    "Renaming isn't wired up to a text-entry dialog yet — use the sound package files directly for now.",
                );
            }
        });
        window.on_delete_sound_package({
            let weak = window.as_weak();
            move || {
                let w = weak.upgrade().unwrap();
                let pkg = w.get_sound_package().to_string();
                if pkg == "default" {
                    msgbox_simple("Delete Package", "The default package can't be deleted.");
                    return;
                }
                if crate::sound_packages::sound_packages::delete_package(&pkg).is_ok() {
                    refresh_sound_packages(&w, "default");
                    refresh_sound_labels(&w, "default");
                }
            }
        });
        window.on_export_sound_package({
            let weak = window.as_weak();
            move || {
                let w = weak.upgrade().unwrap();
                let pkg = w.get_sound_package().to_string();
                pick_save_zip_async(&format!("{pkg}.zip"), move |dest| {
                    let Some(dest) = dest else { return };
                    if let Err(e) = crate::sound_packages::sound_packages::export_package_zip(
                        &pkg,
                        std::path::Path::new(&dest),
                    ) {
                        msgbox_simple("Export failed", &e);
                    }
                });
            }
        });
        window.on_import_sound_package({
            let weak = window.as_weak();
            move || {
                let weak = weak.clone();
                pick_open_zip_async(move |zip_path| {
                    let Some(zip_path) = zip_path else { return };
                    let Some(w) = weak.upgrade() else { return };
                    match crate::sound_packages::sound_packages::import_package_zip(
                        std::path::Path::new(&zip_path),
                    ) {
                        Ok(name) => {
                            refresh_sound_packages(&w, &name);
                            refresh_sound_labels(&w, &name);
                        }
                        Err(e) => msgbox_simple("Import failed", &e),
                    }
                });
            }
        });
        window.on_add_sound_label({
            let weak = window.as_weak();
            let stack = panel_stack.clone();
            move || {
                let w = weak.upgrade().unwrap();
                let pkg = w.get_sound_package().to_string();
                let weak = weak.clone();
                push_sound_label_panel(&w, &stack, None, move |name, path| {
                    if crate::sound_packages::sound_packages::add_or_replace_label(
                        &pkg,
                        &name,
                        std::path::Path::new(&path),
                    )
                    .is_ok()
                    {
                        if let Some(w) = weak.upgrade() {
                            refresh_sound_labels(&w, &pkg);
                        }
                    }
                });
            }
        });
        window.on_edit_sound_label({
            let weak = window.as_weak();
            let stack = panel_stack.clone();
            move || {
                let w = weak.upgrade().unwrap();
                let pkg = w.get_sound_package().to_string();
                let idx = w.get_selected_label_index();
                if idx < 0 {
                    return;
                }
                let Some(entry) = selected_label_entry(&pkg, idx) else {
                    return;
                };
                let old_name = entry.name;
                let current_path = crate::sound_packages::sound_packages::package_dir(&pkg)
                    .join(&entry.file)
                    .to_string_lossy()
                    .into_owned();
                let weak = weak.clone();
                push_sound_label_panel(
                    &w,
                    &stack,
                    Some((old_name.clone(), current_path)),
                    move |name, path| {
                        if name != old_name {
                            let _ = crate::sound_packages::sound_packages::rename_label(
                                &pkg, &old_name, &name,
                            );
                        }
                        if crate::sound_packages::sound_packages::add_or_replace_label(
                            &pkg,
                            &name,
                            std::path::Path::new(&path),
                        )
                        .is_ok()
                        {
                            if let Some(w) = weak.upgrade() {
                                refresh_sound_labels(&w, &pkg);
                            }
                        }
                    },
                );
            }
        });
        window.on_delete_sound_label({
            let weak = window.as_weak();
            move || {
                let w = weak.upgrade().unwrap();
                let pkg = w.get_sound_package().to_string();
                let idx = w.get_selected_label_index();
                if idx < 0 {
                    return;
                }
                if let Some(entry) = selected_label_entry(&pkg, idx) {
                    crate::sound_packages::sound_packages::delete_label(&pkg, &entry.name);
                    refresh_sound_labels(&w, &pkg);
                }
            }
        });

        // ── Triggers tab ──────────────────────────────────────────────────
        window.on_add_trigger({
            let weak = window.as_weak();
            let stack = panel_stack.clone();
            let trigger_cfg = trigger_cfg.clone();
            move || {
                let w = weak.upgrade().unwrap();
                push_trigger_panel(&w, &stack, &trigger_cfg, None);
            }
        });
        window.on_edit_trigger({
            let weak = window.as_weak();
            let stack = panel_stack.clone();
            let trigger_cfg = trigger_cfg.clone();
            move || {
                let w = weak.upgrade().unwrap();
                let idx = w.get_selected_trigger_index();
                if idx >= 0 {
                    push_trigger_panel(&w, &stack, &trigger_cfg, Some(idx as usize));
                }
            }
        });
        window.on_delete_trigger({
            let weak = window.as_weak();
            let trigger_cfg = trigger_cfg.clone();
            move || {
                let w = weak.upgrade().unwrap();
                let idx = w.get_selected_trigger_index();
                if idx >= 0 && (idx as usize) < trigger_cfg.borrow().triggers.len() {
                    trigger_cfg.borrow_mut().triggers.remove(idx as usize);
                    w.set_selected_trigger_index(-1);
                    refresh_trigger_rows(&w, &trigger_cfg);
                }
            }
        });
        window.on_move_trigger_up({
            let weak = window.as_weak();
            let trigger_cfg = trigger_cfg.clone();
            move || {
                let w = weak.upgrade().unwrap();
                let idx = w.get_selected_trigger_index();
                if idx > 0 {
                    trigger_cfg
                        .borrow_mut()
                        .triggers
                        .swap(idx as usize, idx as usize - 1);
                    w.set_selected_trigger_index(idx - 1);
                    refresh_trigger_rows(&w, &trigger_cfg);
                }
            }
        });
        window.on_move_trigger_down({
            let weak = window.as_weak();
            let trigger_cfg = trigger_cfg.clone();
            move || {
                let w = weak.upgrade().unwrap();
                let idx = w.get_selected_trigger_index();
                let len = trigger_cfg.borrow().triggers.len() as i32;
                if idx >= 0 && idx < len - 1 {
                    trigger_cfg
                        .borrow_mut()
                        .triggers
                        .swap(idx as usize, idx as usize + 1);
                    w.set_selected_trigger_index(idx + 1);
                    refresh_trigger_rows(&w, &trigger_cfg);
                }
            }
        });
        window.on_toggle_trigger({
            let weak = window.as_weak();
            let trigger_cfg = trigger_cfg.clone();
            move || {
                let w = weak.upgrade().unwrap();
                let idx = w.get_selected_trigger_index();
                if idx >= 0 {
                    let mut tc = trigger_cfg.borrow_mut();
                    if let Some(t) = tc.triggers.get_mut(idx as usize) {
                        t.enabled = !t.enabled;
                    }
                    drop(tc);
                    refresh_trigger_rows(&w, &trigger_cfg);
                }
            }
        });
        window.on_test_trigger({
            let weak = window.as_weak();
            let trigger_cfg = trigger_cfg.clone();
            let handle = handle.clone();
            move || {
                let w = weak.upgrade().unwrap();
                let idx = w.get_selected_trigger_index();
                if idx < 0 {
                    tracing::info!("test trigger: no row selected (idx={}), doing nothing", idx);
                    return;
                }
                let actions = trigger_cfg
                    .borrow()
                    .triggers
                    .get(idx as usize)
                    .map(|t| t.actions.clone());
                match actions {
                    Some(actions) => {
                        tracing::info!(
                            "test trigger: firing {} action(s) for row {}",
                            actions.len(),
                            idx
                        );
                        match handle.trigger_engine.lock().unwrap().as_ref() {
                            Some(engine) => engine.fire_actions_for_test(&actions),
                            None => tracing::info!(
                                "test trigger: no trigger engine present, doing nothing"
                            ),
                        }
                    }
                    None => tracing::info!(
                        "test trigger: row {} not found in trigger_cfg, doing nothing",
                        idx
                    ),
                }
            }
        });
    }

    fn selected_label_entry(
        pkg: &str,
        idx: i32,
    ) -> Option<crate::sound_packages::sound_packages::LabelEntry> {
        let mut labels = crate::sound_packages::sound_packages::load_manifest(pkg).labels;
        labels.sort_by(|a, b| a.name.cmp(&b.name));
        labels.into_iter().nth(idx as usize)
    }

    // ── Trigger row formatting ────────────────────────────────────────────────

    fn format_trigger_row(t: &TriggerDef) -> String {
        let logic = match t.condition_logic {
            ConditionLogic::All => "ALL",
            ConditionLogic::Any => "ANY",
        };
        format!(
            "[{}] {}  ({} {} cond, {} act)",
            if t.enabled { "✓" } else { " " },
            t.name,
            logic,
            t.conditions.len(),
            t.actions.len(),
        )
    }

    fn refresh_trigger_rows(window: &SettingsWindow, trigger_cfg: &Rc<RefCell<TriggerConfig>>) {
        let rows: Vec<String> = trigger_cfg
            .borrow()
            .triggers
            .iter()
            .map(format_trigger_row)
            .collect();
        window.set_trigger_rows(str_model(&rows));
    }

    fn chat_channel_to_label(c: &ChatChannel) -> &'static str {
        match c {
            ChatChannel::Any => "Any",
            ChatChannel::Say => "Say",
            ChatChannel::Tell => "Tell",
            ChatChannel::Ooc => "OOC",
            ChatChannel::Shout => "Shout",
            ChatChannel::Guild => "Guild",
            ChatChannel::Group => "Group",
            ChatChannel::Raid => "Raid",
            ChatChannel::Auction => "Auction",
            ChatChannel::Custom => "Custom channel",
        }
    }

    fn chat_channel_from_label(label: &str) -> ChatChannel {
        match label {
            "Say" => ChatChannel::Say,
            "Tell" => ChatChannel::Tell,
            "OOC" => ChatChannel::Ooc,
            "Shout" => ChatChannel::Shout,
            "Guild" => ChatChannel::Guild,
            "Group" => ChatChannel::Group,
            "Raid" => ChatChannel::Raid,
            "Auction" => ChatChannel::Auction,
            "Custom channel" => ChatChannel::Custom,
            _ => ChatChannel::Any,
        }
    }

    // Badge tint per condition/action kind, keyed on the part of the badge
    // before its own "/" (e.g. "match/regex" -> "match") — lets the drawer
    // color-code row kinds at a glance instead of every badge reading in
    // the same muted gray. Picked to sit alongside Theme's existing accent/
    // danger/warning hues (theme.slint) without clashing.
    fn trigger_row_tint(badge: &str) -> Color {
        let kind = badge.split('/').next().unwrap_or(badge);
        match kind {
            "match" => Color::from_rgb_u8(0x9b, 0x7c, 0xd6),
            "chat" => Color::from_rgb_u8(0x53, 0x9b, 0xd6),
            "var" | "store_var" => Color::from_rgb_u8(0xc8, 0xa8, 0x3c),
            "overlay" => Color::from_rgb_u8(0xd6, 0x8a, 0x4a),
            "voice" => Color::from_rgb_u8(0xd9, 0x53, 0x4f),
            "play_sound" => Color::from_rgb_u8(0x5f, 0xa0, 0x50),
            _ => Color::from_rgb_u8(0x9a, 0xa0, 0xa6),
        }
    }

    fn trigger_row(badge: &str, text: String) -> TriggerRow {
        TriggerRow {
            badge: badge.into(),
            text: text.into(),
            tint: trigger_row_tint(badge),
        }
    }

    fn condition_row(c: &Condition) -> TriggerRow {
        match c {
            Condition::Match {
                match_type,
                pattern,
            } => {
                let mt = match match_type {
                    MatchType::Exact => "exact",
                    MatchType::Regex => "regex",
                    MatchType::Glob => "glob",
                };
                trigger_row(&format!("match/{mt}"), pattern.clone())
            }
            Condition::Chat {
                channel,
                custom_channel,
                match_type,
                pattern,
            } => {
                let mt = match match_type {
                    MatchType::Exact => "exact",
                    MatchType::Regex => "regex",
                    MatchType::Glob => "glob",
                };
                let chan = match channel {
                    ChatChannel::Custom => custom_channel.as_str(),
                    other => chat_channel_to_label(other),
                };
                let text = if pattern.is_empty() {
                    format!("{chan}: (any message)")
                } else {
                    format!("{chan}: {pattern}")
                };
                trigger_row(&format!("chat/{mt}"), text)
            }
            Condition::Var {
                var_name,
                op,
                value,
            } => {
                let op_s = match op {
                    VarOp::Isset => "isset".to_string(),
                    VarOp::Equals => format!("== {value}"),
                    VarOp::Gt => format!("> {value}"),
                    VarOp::Gte => format!(">= {value}"),
                    VarOp::Lt => format!("< {value}"),
                    VarOp::Lte => format!("<= {value}"),
                    VarOp::Matches => format!("matches {value}"),
                };
                trigger_row("var", format!("{var_name}  {op_s}"))
            }
        }
    }

    fn action_row(a: &Action) -> TriggerRow {
        match a {
            Action::Overlay {
                icon,
                message,
                delay_secs,
                treatment,
                priority,
                ..
            } => {
                let mut tags = Vec::new();
                match priority {
                    VoicePriority::Emergency => tags.push("emergency"),
                    VoicePriority::Ambient => tags.push("ambient"),
                    VoicePriority::Operational => {}
                }
                match treatment {
                    Treatment::None => {}
                    Treatment::Glow => tags.push("glow"),
                    Treatment::Vibrate => tags.push("vibrate"),
                    Treatment::Pulse => tags.push("pulse"),
                }
                let suffix = if tags.is_empty() {
                    String::new()
                } else {
                    format!(" [{}]", tags.join(", "))
                };
                let text = if *delay_secs > 0.0 {
                    format!("+{delay_secs:.1}s  {message}{suffix}")
                } else {
                    format!("{message}{suffix}")
                };
                trigger_row(&format!("overlay/{icon}"), text)
            }
            Action::VoiceAlert { tts_text, priority } => {
                let prio = match priority {
                    VoicePriority::Emergency => "emergency",
                    VoicePriority::Operational => "operational",
                    VoicePriority::Ambient => "ambient",
                };
                trigger_row(&format!("voice/{prio}"), tts_text.clone())
            }
            Action::PlaySound {
                sounds,
                mode,
                delay_secs,
                ..
            } => {
                let snd = if sounds.is_empty() {
                    "(none)".to_string()
                } else if sounds.len() == 1 {
                    sounds[0].clone()
                } else {
                    let mode_label = match mode {
                        SoundMode::Random => "random",
                        SoundMode::Sequential => "sequential",
                    };
                    format!("{} ({mode_label})", sounds.join(", "))
                };
                let text = if *delay_secs > 0.0 {
                    format!("+{delay_secs:.1}s  {snd}")
                } else {
                    snd
                };
                trigger_row("play_sound", text)
            }
            Action::StoreVar { var_name, value } => {
                trigger_row("store_var", format!("{var_name} = {value}"))
            }
        }
    }

    // ── Trigger panel ─────────────────────────────────────────────────────────

    fn push_trigger_panel(
        window: &SettingsWindow,
        stack: &Rc<RefCell<Vec<PanelFrame>>>,
        trigger_cfg: &Rc<RefCell<TriggerConfig>>,
        edit_index: Option<usize>,
    ) {
        let starting = edit_index
            .and_then(|i| trigger_cfg.borrow().triggers.get(i).cloned())
            .unwrap_or_else(|| TriggerDef {
                name: "New Trigger".into(),
                enabled: true,
                condition_logic: ConditionLogic::All,
                conditions: Vec::new(),
                actions: Vec::new(),
            });

        window.set_trigger_name(starting.name.clone().into());
        window.set_trigger_enabled(starting.enabled);
        window.set_condition_logic(
            match starting.condition_logic {
                ConditionLogic::All => "ALL (AND)",
                ConditionLogic::Any => "ANY (OR)",
            }
            .into(),
        );
        // Reset explicitly rather than relying on a fresh window's default
        // — this panel is a permanent, reused widget now (see Risk #1 in
        // the drawer migration plan).
        window.set_selected_condition_index(-1);
        window.set_selected_action_index(-1);

        let conditions: Rc<RefCell<Vec<Condition>>> =
            Rc::new(RefCell::new(starting.conditions.clone()));
        let actions: Rc<RefCell<Vec<Action>>> = Rc::new(RefCell::new(starting.actions.clone()));
        refresh_condition_rows(window, &conditions);
        refresh_action_rows(window, &actions);

        stack.borrow_mut().push(PanelFrame::Trigger {
            edit_index,
            conditions,
            actions,
        });
        sync_drawer_to_top(window, stack);
    }

    fn refresh_condition_rows(window: &SettingsWindow, conditions: &Rc<RefCell<Vec<Condition>>>) {
        let rows: Vec<TriggerRow> = conditions.borrow().iter().map(condition_row).collect();
        window.set_condition_rows(ModelRc::new(VecModel::from(rows)));
    }

    fn refresh_action_rows(window: &SettingsWindow, actions: &Rc<RefCell<Vec<Action>>>) {
        let rows: Vec<TriggerRow> = actions.borrow().iter().map(action_row).collect();
        window.set_action_rows(ModelRc::new(VecModel::from(rows)));
    }

    // ── Condition panel ────────────────────────────────────────────────────────

    // Flattens PATTERN_PRESETS into the picker's row list, inserting one
    // non-clickable header row per category (in the order categories first
    // appear) — see PatternPreset's doc comment in preset-picker.slint for
    // why this is a flat list rather than a nested model.
    fn build_pattern_preset_rows() -> Vec<PatternPreset> {
        let mut rows = Vec::new();
        let mut last_category = String::new();
        for preset in effective_presets() {
            if preset.category != last_category {
                rows.push(PatternPreset {
                    label: preset.category.clone().into(),
                    pattern: "".into(),
                    is_header: true,
                });
                last_category = preset.category.clone();
            }
            rows.push(PatternPreset {
                label: preset.label.into(),
                pattern: preset.pattern.into(),
                is_header: false,
            });
        }
        rows
    }

    fn push_condition_panel(
        window: &SettingsWindow,
        stack: &Rc<RefCell<Vec<PanelFrame>>>,
        conditions: Rc<RefCell<Vec<Condition>>>,
        edit_index: Option<usize>,
    ) {
        let starting = edit_index
            .and_then(|i| conditions.borrow().get(i).cloned())
            .unwrap_or(Condition::Match {
                match_type: MatchType::Regex,
                pattern: String::new(),
            });

        // Reset every field to a safe default before applying `starting` —
        // this panel is reused across many condition-editing sessions, not
        // a fresh window each time (Risk #1).
        window.set_cond_type("Match (log line)".into());
        window.set_match_type("Regex".into());
        window.set_pattern("".into());
        window.set_chat_channel("Any".into());
        window.set_chat_custom_channel("".into());
        window.set_cond_var_name("".into());
        window.set_var_op("isset".into());
        window.set_cond_var_value("".into());

        match &starting {
            Condition::Match {
                match_type,
                pattern,
            } => {
                window.set_cond_type("Match (log line)".into());
                window.set_match_type(
                    match match_type {
                        MatchType::Exact => "Exact (substring)",
                        MatchType::Regex => "Regex",
                        MatchType::Glob => "Glob  (* ? {name})",
                    }
                    .into(),
                );
                window.set_pattern(pattern.clone().into());
            }
            Condition::Chat {
                channel,
                custom_channel,
                match_type,
                pattern,
            } => {
                window.set_cond_type("Chat message".into());
                window.set_chat_channel(chat_channel_to_label(channel).into());
                window.set_chat_custom_channel(custom_channel.clone().into());
                window.set_match_type(
                    match match_type {
                        MatchType::Exact => "Exact (substring)",
                        MatchType::Regex => "Regex",
                        MatchType::Glob => "Glob  (* ? {name})",
                    }
                    .into(),
                );
                window.set_pattern(pattern.clone().into());
            }
            Condition::Var {
                var_name,
                op,
                value,
            } => {
                window.set_cond_type("Variable".into());
                window.set_cond_var_name(var_name.clone().into());
                window.set_var_op(
                    match op {
                        VarOp::Isset => "isset",
                        VarOp::Equals => "equals",
                        VarOp::Gt => "gt (>)",
                        VarOp::Gte => "gte (\u{2265})",
                        VarOp::Lt => "lt (<)",
                        VarOp::Lte => "lte (\u{2264})",
                        VarOp::Matches => "matches",
                    }
                    .into(),
                );
                window.set_cond_var_value(value.clone().into());
            }
        }

        stack.borrow_mut().push(PanelFrame::Condition {
            conditions,
            edit_index,
        });
        sync_drawer_to_top(window, stack);
    }

    // ── Action panel ───────────────────────────────────────────────────────────

    /// Converts `hex` (falling back to `default_rgb` if empty/unparseable)
    /// to HSV, generates the matching `sv-image` bitmap, and hands all of
    /// it to `setter` — the one piece of seeding logic shared by Icon,
    /// Text, and Border color, which otherwise only differ in which of the
    /// three parallel sets of Slint properties they write into.
    fn seed_hsv_fields(
        window: &SettingsWindow,
        hex: &str,
        default_rgb: (u8, u8, u8),
        setter: fn(&SettingsWindow, f32, f32, f32, Color, slint::Image),
    ) {
        let color = color_from_hex(hex, default_rgb);
        let (h, s, v) = rgb_to_hsv(color.red(), color.green(), color.blue());
        let sv_image = generate_sv_image(h);
        setter(window, h, s, v, color, sv_image);
    }

    fn set_icon_hsv(
        w: &SettingsWindow,
        h: f32,
        s: f32,
        v: f32,
        color: Color,
        sv_image: slint::Image,
    ) {
        w.set_icon_hue(h);
        w.set_icon_sat(s);
        w.set_icon_val(v);
        w.set_icon_color_preview(color);
        w.set_icon_sv_image(sv_image);
    }

    fn set_message_hsv(
        w: &SettingsWindow,
        h: f32,
        s: f32,
        v: f32,
        color: Color,
        sv_image: slint::Image,
    ) {
        w.set_message_hue(h);
        w.set_message_sat(s);
        w.set_message_val(v);
        w.set_message_color_preview(color);
        w.set_message_sv_image(sv_image);
    }

    fn set_border_hsv(
        w: &SettingsWindow,
        h: f32,
        s: f32,
        v: f32,
        color: Color,
        sv_image: slint::Image,
    ) {
        w.set_border_hue(h);
        w.set_border_sat(s);
        w.set_border_val(v);
        w.set_border_color_preview(color);
        w.set_border_sv_image(sv_image);
    }

    fn push_action_panel(
        window: &SettingsWindow,
        stack: &Rc<RefCell<Vec<PanelFrame>>>,
        actions: Rc<RefCell<Vec<Action>>>,
        edit_index: Option<usize>,
        trigger_cfg: &TriggerConfig,
    ) {
        let starting = edit_index
            .and_then(|i| actions.borrow().get(i).cloned())
            .unwrap_or(Action::Overlay {
                icon: String::new(),
                color: String::new(),
                message: String::new(),
                message_color: String::new(),
                border_color: String::new(),
                delay_secs: 0.0,
                treatment: Treatment::default(),
                priority: VoicePriority::default(),
            });

        let icon_items = build_icon_items();
        let icon_options: Vec<IconOption> = icon_items
            .iter()
            .map(|i| {
                let thumb = load_icon_thumbnail(&i.key);
                IconOption {
                    label: i.label.clone().into(),
                    has_icon: thumb.is_some(),
                    icon: thumb.unwrap_or_default(),
                }
            })
            .collect();
        window.set_icon_options(ModelRc::new(VecModel::from(icon_options)));
        let all_indices: Vec<i32> = (0..icon_items.len() as i32).collect();
        window.set_icon_visible_indices(ModelRc::new(VecModel::from(all_indices)));
        window.set_icon_search_text("".into());
        let sources = build_icon_sources(&icon_items);
        window.set_icon_source_labels(str_model(
            &sources
                .iter()
                .map(|(_, label)| label.clone())
                .collect::<Vec<_>>(),
        ));
        window.set_icon_source_label_value(
            sources
                .first()
                .map(|(_, label)| label.clone())
                .unwrap_or_default()
                .into(),
        );

        window.set_hue_strip_image(hue_strip_image());
        window.set_recent_colors(ModelRc::new(VecModel::from(recent_colors_as_slint(
            trigger_cfg,
        ))));

        // `all_label_options()` prepends a `("", "(none)")` sentinel for the
        // Sounds tab's own dropdown; the per-row Play Sound dropdowns below
        // don't need it since add/remove already covers "no sound here".
        let sound_labels: Vec<String> = crate::sound_packages::sound_packages::all_label_options()
            .into_iter()
            .skip(1)
            .map(|(name, _)| name)
            .collect();
        window.set_sound_options(str_model(&sound_labels));

        // Reset every field to a safe default before applying `starting` —
        // this panel is reused across many action-editing sessions of
        // different types, not a fresh window each time (Risk #1).
        window.set_action_type("Overlay message".into());
        window.set_icon_mode("icon".into());
        window.set_icon_index(0);
        seed_hsv_fields(window, "", DEFAULT_ICON_SWATCH_RGB, set_icon_hsv);
        window.set_message("".into());
        seed_hsv_fields(window, "", DEFAULT_TEXT_RGB, set_message_hsv);
        seed_hsv_fields(window, "", DEFAULT_BORDER_RGB, set_border_hsv);
        window.set_treatment("None".into());
        window.set_overlay_priority("Operational (queues)".into());
        window.set_delay_secs("".into());
        window.set_sound_slots(str_model(&[String::new()]));
        window.set_sound_mode("Random".into());
        window.set_tts_text("".into());
        window.set_voice_priority(1);
        window.set_action_var_name("".into());
        window.set_action_var_value("".into());

        match &starting {
            Action::Overlay {
                icon,
                color,
                message,
                message_color,
                border_color,
                delay_secs,
                treatment,
                priority,
            } => {
                window.set_action_type("Overlay message".into());
                let mode = if icon.is_empty() {
                    "none"
                } else if icon == "colorbox" {
                    "colorbox"
                } else if let Some(idx) = find_icon_index(&icon_items, icon) {
                    window.set_icon_index(idx as i32);
                    "icon"
                } else {
                    // Stored icon file no longer exists on disk — fall
                    // back to "no icon" rather than silently showing
                    // whatever the first real icon happens to be.
                    "none"
                };
                window.set_icon_mode(mode.into());
                seed_hsv_fields(window, color, DEFAULT_ICON_SWATCH_RGB, set_icon_hsv);
                window.set_message(message.clone().into());
                seed_hsv_fields(window, message_color, DEFAULT_TEXT_RGB, set_message_hsv);
                seed_hsv_fields(window, border_color, DEFAULT_BORDER_RGB, set_border_hsv);
                window.set_treatment(
                    match treatment {
                        Treatment::None => "None",
                        Treatment::Glow => "Glow",
                        Treatment::Vibrate => "Vibrate",
                        Treatment::Pulse => "Pulse",
                    }
                    .into(),
                );
                window.set_overlay_priority(
                    match priority {
                        VoicePriority::Emergency => "Emergency (interrupts)",
                        VoicePriority::Operational => "Operational (queues)",
                        VoicePriority::Ambient => "Ambient (may drop)",
                    }
                    .into(),
                );
                window.set_delay_secs(
                    if *delay_secs > 0.0 {
                        delay_secs.to_string()
                    } else {
                        String::new()
                    }
                    .into(),
                );
            }
            Action::StoreVar { var_name, value } => {
                window.set_action_type("Store variable".into());
                window.set_action_var_name(var_name.clone().into());
                window.set_action_var_value(value.clone().into());
            }
            Action::VoiceAlert { tts_text, priority } => {
                window.set_action_type("Voice Alert (TTS)".into());
                window.set_tts_text(tts_text.clone().into());
                window.set_voice_priority(match priority {
                    VoicePriority::Emergency => 0,
                    VoicePriority::Operational => 1,
                    VoicePriority::Ambient => 2,
                });
            }
            Action::PlaySound {
                sounds,
                mode,
                delay_secs,
                ..
            } => {
                window.set_action_type("Play Sound".into());
                let slots: Vec<String> = if sounds.is_empty() {
                    vec![String::new()]
                } else {
                    sounds.clone()
                };
                window.set_sound_slots(str_model(&slots));
                window.set_sound_mode(
                    if *mode == SoundMode::Sequential {
                        "Sequential"
                    } else {
                        "Random"
                    }
                    .into(),
                );
                window.set_delay_secs(
                    if *delay_secs > 0.0 {
                        delay_secs.to_string()
                    } else {
                        String::new()
                    }
                    .into(),
                );
            }
        }

        stack.borrow_mut().push(PanelFrame::Action {
            actions,
            edit_index,
        });
        sync_drawer_to_top(window, stack);
    }

    fn read_sound_slots(w: &SettingsWindow) -> Vec<String> {
        let slots = w.get_sound_slots();
        (0..slots.row_count())
            .map(|j| slots.row_data(j).unwrap_or_default().to_string())
            .collect()
    }

    // ── Icon list ──────────────────────────────────────────────────────────────

    /// One real icon file available to the action editor's picker. No more
    /// "(none)"/"colorbox" synthetic entries mixed in here — those are the
    /// two non-`Icon` modes of `IconComboBoxField` now, tracked separately
    /// (`icon-mode`), so this list is only ever real files on disk.
    #[derive(Clone)]
    struct IconItem {
        key: String,
        label: String,
        /// The exact `uifiles` skin directory this came from (see
        /// spell_icons.rs's manifest sidecar), or empty for a plain
        /// user-added icon that didn't come from "Import Spell Icons".
        source: String,
        /// Every spell name pointing at this icon's global icon id
        /// (lowercased), so a search matches on any of the spells that
        /// could plausibly use this icon, not just the one shown as
        /// `label`. Falls back to `[label.to_lowercase()]` for plain
        /// user-added icons with no manifest entry.
        search_names: Vec<String>,
    }

    /// `filename -> (source, primary spell_name, all spell names)`. Both
    /// name fields are empty/empty-vec when `extract_spell_icons` couldn't
    /// match the icon to a `spells_us.txt` entry (file missing, or the icon
    /// id had no spell pointing at it).
    fn load_icon_source_manifest(
    ) -> std::collections::HashMap<String, (String, String, Vec<String>)> {
        let mut map = std::collections::HashMap::new();
        if let Ok(text) = std::fs::read_to_string(
            crate::assets::icons_dir().join(crate::assets::SPELL_ICON_MANIFEST_FILE),
        ) {
            for line in text.lines() {
                let mut parts = line.split('\t');
                let (Some(file), Some(source)) = (parts.next(), parts.next()) else {
                    continue;
                };
                let spell_name = parts.next().unwrap_or_default();
                let all_names: Vec<String> = parts
                    .next()
                    .unwrap_or_default()
                    .split('|')
                    .filter(|s| !s.is_empty())
                    .map(|s| s.to_string())
                    .collect();
                map.insert(
                    file.to_string(),
                    (source.to_string(), spell_name.to_string(), all_names),
                );
            }
        }
        map
    }

    fn build_icon_items() -> Vec<IconItem> {
        let manifest = load_icon_source_manifest();
        let mut items = Vec::new();
        if let Ok(dir) = std::fs::read_dir(crate::assets::icons_dir()) {
            let mut files: Vec<String> = dir
                .filter_map(|e| e.ok())
                .filter(|e| {
                    let p = e.path();
                    matches!(
                        p.extension().and_then(|x| x.to_str()),
                        Some("png") | Some("jpg")
                    )
                })
                .map(|e| e.file_name().to_string_lossy().into_owned())
                .filter(|f| !crate::assets::STOCK_ICON_FILES.contains(&f.as_str()))
                .collect();
            files.sort();
            for f in files {
                let (source, spell_name, all_names) = manifest.get(&f).cloned().unwrap_or_default();
                // Prefer the real spell name (e.g. "Superior Camouflage")
                // over the filename-derived label (e.g.
                // "spell_default_Spells01_r05_c05") so search/sort in the
                // icon picker works on something a user would recognize.
                let label = if spell_name.is_empty() {
                    f.trim_end_matches(".jpg")
                        .trim_end_matches(".png")
                        .to_string()
                } else {
                    spell_name
                };
                // Search should match any spell that could use this icon,
                // not just the one chosen as the display label — falls back
                // to the label itself when there's no manifest entry (e.g.
                // a plain user-added icon).
                let search_names = if all_names.is_empty() {
                    vec![label.to_lowercase()]
                } else {
                    all_names.iter().map(|n| n.to_lowercase()).collect()
                };
                items.push(IconItem {
                    key: f,
                    label,
                    source,
                    search_names,
                });
            }
        }
        items
    }

    fn find_icon_index(items: &[IconItem], key: &str) -> Option<usize> {
        items.iter().position(|it| it.key == key)
    }

    /// `(raw filter key, display label)` pairs for the Source dropdown —
    /// first entry is always the "All sources" catch-all. A source-less
    /// item (a plain user-added icon, not one `build_icon_items` found a
    /// manifest entry for) is bucketed under the synthetic "Custom" key.
    fn build_icon_sources(items: &[IconItem]) -> Vec<(String, String)> {
        use std::collections::BTreeMap;
        let mut counts: BTreeMap<String, usize> = BTreeMap::new();
        for it in items {
            let key = if it.source.is_empty() {
                "Custom".to_string()
            } else {
                it.source.clone()
            };
            *counts.entry(key).or_insert(0) += 1;
        }
        let mut out = vec![(String::new(), format!("All sources ({})", items.len()))];
        for (key, n) in counts {
            let label = format!("{key} ({n})");
            out.push((key, label));
        }
        out
    }

    fn icon_source_matches(item_source: &str, filter_key: &str) -> bool {
        if filter_key.is_empty() {
            return true;
        }
        if filter_key == "Custom" {
            return item_source.is_empty();
        }
        item_source == filter_key
    }

    /// Re-filters the (freshly re-scanned) full icon list by the panel's
    /// current search text + Source selection and pushes the resulting
    /// positions into `icon-visible-indices` — `icon-options` itself (and
    /// therefore `icon-index`, which points into it) is never touched here,
    /// see `IconComboBoxField`'s own doc comment for why that separation
    /// matters. Re-running `build_icon_items()` here (rather than reusing
    /// whatever `push_action_panel` built) is safe as long as the icons
    /// directory doesn't change while the panel is open — a directory scan
    /// is cheap enough to just redo on every keystroke rather than cache.
    fn recompute_icon_visible_indices(window: &SettingsWindow) {
        let items = build_icon_items();
        let sources = build_icon_sources(&items);
        let selected_label = window.get_icon_source_label_value().to_string();
        let filter_key = sources
            .iter()
            .find(|(_, label)| *label == selected_label)
            .map(|(key, _)| key.as_str())
            .unwrap_or("");
        let query = window.get_icon_search_text().to_string().to_lowercase();
        let visible: Vec<i32> = items
            .iter()
            .enumerate()
            .filter(|(_, it)| icon_source_matches(&it.source, filter_key))
            .filter(|(_, it)| {
                query.is_empty() || it.search_names.iter().any(|n| n.contains(&query))
            })
            .map(|(i, _)| i as i32)
            .collect();
        window.set_icon_visible_indices(ModelRc::new(VecModel::from(visible)));
    }

    /// Thumbnail for the action editor's icon picker (`IconComboBoxField`).
    fn load_icon_thumbnail(filename: &str) -> Option<slint::Image> {
        if filename.is_empty() {
            return None;
        }
        slint::Image::load_from_path(&crate::assets::icons_dir().join(filename)).ok()
    }

    // ── Native dialogs / requests ───────────────────────────────────────────
    // Cross-platform via `rfd` (GTK on Linux, common-controls on Windows,
    // AppKit on macOS) — replaces the raw Win32 common dialogs this used to
    // call directly.

    /// Runs a blocking `rfd` dialog call (`.show()`/`.pick_file()`/
    /// `.save_file()`) on a background thread, delivering the result back
    /// via `on_done` on the UI thread (`slint::invoke_from_event_loop`).
    ///
    /// On Linux, rfd's gtk3 backend runs the actual dialog on its own
    /// dedicated GTK thread (`GtkGlobalThread`) and blocks the *calling*
    /// thread on a condvar until that thread finishes. But the tray icon
    /// also owns GTK on the main thread, driving its event loop from a
    /// periodic `gtk_main_iteration()` pump (see tray.rs's `run`). Calling
    /// an rfd dialog synchronously on the UI thread stops that pump dead —
    /// and rfd's own GTK thread can't complete without it, so the two
    /// threads deadlock the whole app with no dialog ever appearing.
    /// Confirmed with a live thread dump (not guessed): the UI thread sat
    /// parked in `GtkGlobalThread::run_blocking`'s condvar wait, reproduced
    /// identically on both a plain Xvfb session and a real xorgxrdp Xorg
    /// session — not specific to either. Keeping the blocking call off the
    /// UI thread leaves the pump running, which avoids the deadlock.
    fn run_rfd_async<T: Send + 'static>(
        dialog: impl FnOnce() -> T + Send + 'static,
        on_done: impl FnOnce(T) + Send + 'static,
    ) {
        std::thread::Builder::new()
            .name("rfd-dialog".into())
            .spawn(move || {
                let result = dialog();
                let _ = slint::invoke_from_event_loop(move || on_done(result));
            })
            .expect("spawn rfd dialog thread");
    }

    fn msgbox_simple(title: &str, msg: &str) {
        let title = title.to_string();
        let msg = msg.to_string();
        run_rfd_async(
            move || {
                rfd::MessageDialog::new()
                    .set_title(&title)
                    .set_description(&msg)
                    .set_level(rfd::MessageLevel::Warning)
                    .set_buttons(rfd::MessageButtons::Ok)
                    .show();
            },
            |()| {},
        );
    }

    fn pick_log_file_async(on_done: impl FnOnce(Option<String>) + Send + 'static) {
        run_rfd_async(
            || {
                rfd::FileDialog::new()
                    .add_filter("Log files", &["txt"])
                    .pick_file()
                    .map(|p| p.to_string_lossy().into_owned())
            },
            on_done,
        );
    }

    fn pick_sound_file_async(on_done: impl FnOnce(Option<String>) + Send + 'static) {
        run_rfd_async(
            || {
                rfd::FileDialog::new()
                    .add_filter("Sound files", &["wav", "mp3"])
                    .pick_file()
                    .map(|p| p.to_string_lossy().into_owned())
            },
            on_done,
        );
    }

    fn pick_open_zip_async(on_done: impl FnOnce(Option<String>) + Send + 'static) {
        run_rfd_async(
            || {
                rfd::FileDialog::new()
                    .add_filter("Zip files", &["zip"])
                    .pick_file()
                    .map(|p| p.to_string_lossy().into_owned())
            },
            on_done,
        );
    }

    fn pick_save_zip_async(
        default_name: &str,
        on_done: impl FnOnce(Option<String>) + Send + 'static,
    ) {
        let default_name = default_name.to_string();
        run_rfd_async(
            move || {
                rfd::FileDialog::new()
                    .add_filter("Zip files", &["zip"])
                    .set_file_name(&default_name)
                    .save_file()
                    .map(|p| p.to_string_lossy().into_owned())
            },
            on_done,
        );
    }

    // Last few colors picked across the whole app session (most-recent
    // first) — shown as clickable swatches in the color picker. Session-
    // only (thread-local, not persisted to Config): "a few last picked
    // colors" is a convenience for the current editing session, not
    // something that needs to survive a restart or clutter the config
    // schema.
    thread_local! {
        static RECENT_COLORS: RefCell<Vec<String>> = const { RefCell::new(Vec::new()) };
    }
    const MAX_RECENT_COLORS: usize = 8;

    fn remember_recent_color(hex: &str) {
        RECENT_COLORS.with(|c| {
            let mut v = c.borrow_mut();
            v.retain(|existing| !existing.eq_ignore_ascii_case(hex));
            v.insert(0, hex.to_string());
            v.truncate(MAX_RECENT_COLORS);
        });
    }

    const MAX_SHOWN_COLORS: usize = 16;

    /// Recent-colors swatches, most-recent-session-pick first, followed by
    /// every distinct color already assigned to an overlay action anywhere
    /// in the current trigger config (icon/message/border) that isn't
    /// already in the session list — "known existing set colors" the user
    /// has already established across their triggers, so picking a new
    /// action's color can match one already in use instead of guessing a
    /// hex value for consistency.
    fn recent_colors_as_slint(trigger_cfg: &TriggerConfig) -> Vec<Color> {
        let mut hexes: Vec<String> = RECENT_COLORS.with(|c| c.borrow().clone());
        for trigger in &trigger_cfg.triggers {
            for action in &trigger.actions {
                if let Action::Overlay {
                    color,
                    message_color,
                    border_color,
                    ..
                } = action
                {
                    for hex in [color, message_color, border_color] {
                        if !hex.is_empty() && !hexes.iter().any(|h| h.eq_ignore_ascii_case(hex)) {
                            hexes.push(hex.clone());
                        }
                    }
                }
            }
        }
        hexes
            .iter()
            .filter_map(|hex| parse_hex_color(hex))
            .map(|c| {
                Color::from_rgb_u8(
                    (c >> 16 & 0xFF) as u8,
                    (c >> 8 & 0xFF) as u8,
                    (c & 0xFF) as u8,
                )
            })
            .take(MAX_SHOWN_COLORS)
            .collect()
    }

    /// RGB (0..255 per channel) -> HSV (hue 0..360 degrees, saturation and
    /// value both 0..1) for seeding the color wheel/brightness slider from
    /// an existing color (dialog open, or a hex edit / recent-swatch pick).
    fn rgb_to_hsv(r: u8, g: u8, b: u8) -> (f32, f32, f32) {
        let (r, g, b) = (r as f32 / 255.0, g as f32 / 255.0, b as f32 / 255.0);
        let max = r.max(g).max(b);
        let min = r.min(g).min(b);
        let delta = max - min;
        let hue = if delta == 0.0 {
            0.0
        } else if max == r {
            60.0 * (((g - b) / delta).rem_euclid(6.0))
        } else if max == g {
            60.0 * ((b - r) / delta + 2.0)
        } else {
            60.0 * ((r - g) / delta + 4.0)
        };
        let sat = if max == 0.0 { 0.0 } else { delta / max };
        (hue, sat, max)
    }

    /// HSV (hue 0..360 degrees, saturation and value both 0..1) -> RGB
    /// (0..255 per channel), the inverse of `rgb_to_hsv` — used any time the
    /// wheel/slider move to keep `preview`/`hex-text` in sync.
    fn hsv_to_rgb(h: f32, s: f32, v: f32) -> (u8, u8, u8) {
        let h = h.rem_euclid(360.0);
        let c = v * s;
        let x = c * (1.0 - ((h / 60.0) % 2.0 - 1.0).abs());
        let m = v - c;
        let (r1, g1, b1) = match (h / 60.0) as u32 {
            0 => (c, x, 0.0),
            1 => (x, c, 0.0),
            2 => (0.0, c, x),
            3 => (0.0, x, c),
            4 => (x, 0.0, c),
            _ => (c, 0.0, x),
        };
        (
            ((r1 + m) * 255.0).round() as u8,
            ((g1 + m) * 255.0).round() as u8,
            ((b1 + m) * 255.0).round() as u8,
        )
    }

    /// Slim picker bitmaps are rendered at this size regardless of the
    /// panel's on-screen display size — `Image` in slim-color-picker.slint
    /// stretches to its container, same as any other `Image`.
    const SV_IMAGE_W: u32 = 200;
    const SV_IMAGE_H: u32 = 90;
    const HUE_STRIP_W: u32 = 200;
    const HUE_STRIP_H: u32 = 16;

    /// The saturation(x)/value(y) plane for one fixed hue — regenerated on
    /// every hue change (see `on_icon_hsv_changed` and friends), which is
    /// cheap enough (a few tens of thousands of pixels) to just redo on
    /// every drag frame rather than cache. A real bitmap rather than a
    /// `@linear-gradient` brush for the same reason the old wheel panel
    /// used one instead of `@conic-gradient`/`@radial-gradient` (see
    /// slim-color-picker.slint's doc comment) — this app's software
    /// renderer doesn't respect `border-radius` for gradient *backgrounds*
    /// at all, only for clipped *children*, and this needs the former.
    fn generate_sv_image(hue: f32) -> slint::Image {
        let mut buf = slint::SharedPixelBuffer::<slint::Rgba8Pixel>::new(SV_IMAGE_W, SV_IMAGE_H);
        let pixels = buf.make_mut_slice();
        for py in 0..SV_IMAGE_H {
            let val = 1.0 - (py as f32 + 0.5) / SV_IMAGE_H as f32;
            for px in 0..SV_IMAGE_W {
                let sat = (px as f32 + 0.5) / SV_IMAGE_W as f32;
                let (r, g, b) = hsv_to_rgb(hue, sat, val);
                pixels[(py * SV_IMAGE_W + px) as usize] = slint::Rgba8Pixel { r, g, b, a: 255 };
            }
        }
        slint::Image::from_rgba8(buf)
    }

    thread_local! {
        // Doesn't depend on hue/sat/val at all (always the same full-sat,
        // full-val rainbow), so it's rendered once ever and reused by every
        // `SlimColorPicker` instance for the rest of the process, rather
        // than regenerated per panel-open like the old wheel bitmaps were.
        static HUE_STRIP_IMAGE: RefCell<Option<slint::Image>> = const { RefCell::new(None) };
    }

    fn hue_strip_image() -> slint::Image {
        HUE_STRIP_IMAGE.with(|cell| {
            cell.borrow_mut()
                .get_or_insert_with(|| {
                    let mut buf = slint::SharedPixelBuffer::<slint::Rgba8Pixel>::new(
                        HUE_STRIP_W,
                        HUE_STRIP_H,
                    );
                    let pixels = buf.make_mut_slice();
                    for px in 0..HUE_STRIP_W {
                        let hue = (px as f32 + 0.5) / HUE_STRIP_W as f32 * 360.0;
                        let (r, g, b) = hsv_to_rgb(hue, 1.0, 1.0);
                        for py in 0..HUE_STRIP_H {
                            pixels[(py * HUE_STRIP_W + px) as usize] =
                                slint::Rgba8Pixel { r, g, b, a: 255 };
                        }
                    }
                    slint::Image::from_rgba8(buf)
                })
                .clone()
        })
    }

    /// Seeds the drawer's Sound Label panel and pushes it. `initial` is
    /// `(name, absolute file path)` when editing an existing label, `None`
    /// when adding a new one. `on_ok` is called with the label name and file
    /// path the user confirmed, if they hit OK — never called on Cancel.
    fn push_sound_label_panel(
        window: &SettingsWindow,
        stack: &Rc<RefCell<Vec<PanelFrame>>>,
        initial: Option<(String, String)>,
        on_ok: impl Fn(String, String) + 'static,
    ) {
        let (name, path) = initial.unwrap_or_default();
        window.set_label_name(name.into());
        window.set_sound_file_path(path.into());

        stack.borrow_mut().push(PanelFrame::SoundLabel {
            on_ok: Box::new(on_ok),
        });
        sync_drawer_to_top(window, stack);
    }

    /// Seeds the drawer's Log Profile panel and pushes it. `initial` is
    /// `Some(profile)` when editing an existing profile, `None` when adding
    /// a new one. `on_ok` is called with the confirmed profile, if the user
    /// hits OK — never called on Cancel.
    fn push_log_profile_panel(
        window: &SettingsWindow,
        stack: &Rc<RefCell<Vec<PanelFrame>>>,
        initial: Option<LogProfile>,
        on_ok: impl Fn(LogProfile) + 'static,
    ) {
        window.set_game_options(str_model(
            &GAMES.iter().map(|s| s.to_string()).collect::<Vec<_>>(),
        ));
        // Reset every field to a safe default before applying `initial` —
        // this panel is reused across many log-profile-editing sessions,
        // not a fresh window each time (Risk #1).
        window.set_profile_name("".into());
        window.set_log_file_path("".into());
        window.set_server("".into());
        window.set_player("".into());
        window.set_public_streaming(false);
        match &initial {
            Some(p) => {
                window.set_profile_name(p.name.clone().into());
                window.set_log_file_path(p.path.clone().into());
                window.set_game(game_id_to_label(p.game.as_deref().unwrap_or("eql")).into());
                window.set_server(p.server.clone().unwrap_or_default().into());
                window.set_player(p.player.clone().unwrap_or_default().into());
                window.set_public_streaming(p.public_stream);
            }
            None => window.set_game(GAMES[0].into()),
        }

        stack.borrow_mut().push(PanelFrame::LogProfile {
            on_ok: Box::new(on_ok),
        });
        sync_drawer_to_top(window, stack);
    }

    /// Curated catalog of additional English piper voices users can
    /// download from Settings, beyond the one bundled by default (see
    /// `config::default_tts_voice`). All are `en_US`, hosted at
    /// huggingface.co/rhasspy/piper-voices — a small hand-picked set
    /// rather than that repo's full catalog (dozens of voices across many
    /// languages) to keep this panel simple. `(piper short name, quality
    /// tier)` — combined into the real voice name as `en_US-{name}-
    /// {quality}`, same convention `default_tts_voice`'s bundled voice
    /// already follows.
    const VOICE_CATALOG: &[(&str, &str)] = &[
        ("ryan", "medium"),
        ("lessac", "medium"),
        ("kristin", "medium"),
        ("joe", "medium"),
        ("danny", "low"),
        ("hfc_female", "medium"),
        ("hfc_male", "medium"),
    ];

    fn catalog_voice_name(short_name: &str, quality: &str) -> String {
        format!("en_US-{short_name}-{quality}")
    }

    fn catalog_voice_urls(short_name: &str, quality: &str) -> (String, String) {
        let base = format!(
            "https://huggingface.co/rhasspy/piper-voices/resolve/main/en/en_US/{short_name}/{quality}/en_US-{short_name}-{quality}"
        );
        (format!("{base}.onnx"), format!("{base}.onnx.json"))
    }

    /// In-progress voice downloads: voice name -> percent complete
    /// (0..100). Entries are removed on completion or failure. Read by
    /// `voice_catalog_rows` to render each row's progress; written by
    /// `start_voice_download`'s background thread.
    static DOWNLOAD_PROGRESS: std::sync::LazyLock<
        std::sync::Mutex<std::collections::HashMap<String, i32>>,
    > = std::sync::LazyLock::new(|| std::sync::Mutex::new(std::collections::HashMap::new()));

    /// Installed (or actively-downloading) catalog voices — drives the
    /// panel's main list. Not-yet-downloaded ones are in
    /// `voice_catalog_available` instead, not here — see
    /// `VoiceManagerPanel`'s doc comment for why the two are split rather
    /// than one list with a Download button on every row.
    fn voice_catalog_installed_rows() -> Vec<VoiceCatalogRow> {
        let downloaded_dir = crate::assets::downloaded_voices_dir();
        let progress = DOWNLOAD_PROGRESS.lock().unwrap();
        VOICE_CATALOG
            .iter()
            .filter_map(|(short_name, quality)| {
                let voice_name = catalog_voice_name(short_name, quality);
                let installed = downloaded_dir.join(format!("{voice_name}.onnx")).is_file();
                let progress = progress.get(&voice_name).copied().unwrap_or(-1);
                (installed || progress >= 0).then(|| VoiceCatalogRow {
                    label: crate::tts::tts::display_name(&voice_name).into(),
                    progress,
                    installed,
                    name: voice_name.into(),
                })
            })
            .collect()
    }

    /// Catalog voices not yet installed and not currently downloading —
    /// populates the panel's "+" add popup. `(display label, internal
    /// name)` pairs, parallel-array shape matching `voice_catalog_installed_rows`
    /// filtered out.
    fn voice_catalog_available() -> Vec<(String, String)> {
        let downloaded_dir = crate::assets::downloaded_voices_dir();
        let progress = DOWNLOAD_PROGRESS.lock().unwrap();
        VOICE_CATALOG
            .iter()
            .filter_map(|(short_name, quality)| {
                let voice_name = catalog_voice_name(short_name, quality);
                let installed = downloaded_dir.join(format!("{voice_name}.onnx")).is_file();
                let downloading = progress.contains_key(&voice_name);
                (!installed && !downloading)
                    .then(|| (crate::tts::tts::display_name(&voice_name), voice_name))
            })
            .collect()
    }

    fn refresh_voice_catalog(window: &SettingsWindow) {
        window.set_voice_catalog(ModelRc::new(VecModel::from(voice_catalog_installed_rows())));
        let available = voice_catalog_available();
        window.set_voice_catalog_available_labels(str_model(
            &available
                .iter()
                .map(|(label, _)| label.clone())
                .collect::<Vec<_>>(),
        ));
        window.set_voice_catalog_available_names(str_model(
            &available
                .iter()
                .map(|(_, name)| name.clone())
                .collect::<Vec<_>>(),
        ));
    }

    /// Seeds and pushes the drawer's voice-download catalog panel.
    fn push_voice_manager_panel(window: &SettingsWindow, stack: &Rc<RefCell<Vec<PanelFrame>>>) {
        refresh_voice_catalog(window);
        stack.borrow_mut().push(PanelFrame::VoiceManager);
        sync_drawer_to_top(window, stack);
    }

    /// Downloads one catalog voice's `.onnx`/`.onnx.json` pair into
    /// `assets::downloaded_voices_dir()` on a background thread, updating
    /// `DOWNLOAD_PROGRESS` (and re-rendering the panel via
    /// `slint::invoke_from_event_loop`) as it goes — same background-
    /// thread-plus-UI-thread-callback shape as `run_rfd_async` above, just
    /// with multiple progress callbacks instead of one final result.
    fn start_voice_download(window: &SettingsWindow, voice_name: String) {
        let Some((short_name, quality)) = VOICE_CATALOG
            .iter()
            .find(|(s, q)| catalog_voice_name(s, q) == voice_name)
        else {
            tracing::warn!("voice catalog: {voice_name:?} not found in VOICE_CATALOG, ignoring");
            return;
        };
        let (onnx_url, json_url) = catalog_voice_urls(short_name, quality);
        tracing::warn!("voice catalog: starting download of {voice_name:?} from {onnx_url}");

        DOWNLOAD_PROGRESS
            .lock()
            .unwrap()
            .insert(voice_name.clone(), 0);
        refresh_voice_catalog(window);

        let weak = window.as_weak();
        std::thread::Builder::new()
            .name("voice-download".into())
            .spawn(move || {
                let progress_name = voice_name.clone();
                let progress_weak = weak.clone();
                let result =
                    download_voice_files(&voice_name, &onnx_url, &json_url, move |percent| {
                        DOWNLOAD_PROGRESS
                            .lock()
                            .unwrap()
                            .insert(progress_name.clone(), percent);
                        let weak = progress_weak.clone();
                        let _ = slint::invoke_from_event_loop(move || {
                            if let Some(w) = weak.upgrade() {
                                refresh_voice_catalog(&w);
                            }
                        });
                    });
                let succeeded = match &result {
                    Ok(()) => {
                        tracing::warn!("voice catalog: {voice_name:?} download complete");
                        true
                    }
                    Err(e) => {
                        tracing::warn!("voice download failed for {voice_name}: {e}");
                        false
                    }
                };
                DOWNLOAD_PROGRESS.lock().unwrap().remove(&voice_name);
                let _ = slint::invoke_from_event_loop(move || {
                    if let Some(w) = weak.upgrade() {
                        refresh_voice_catalog(&w);
                        // Only on success — a failed download shouldn't
                        // touch the dropdown's current selection at all.
                        if succeeded {
                            refresh_voice_dropdown_preserving_selection(&w);
                        }
                    }
                });
            })
            .expect("spawn voice download thread");
    }

    /// Downloads one voice's config (small, fetched whole) then its model
    /// (tens of MB, streamed with `on_progress` called per chunk) into
    /// `assets::downloaded_voices_dir()`. The `.onnx` lands at a `.part`
    /// path first and is only renamed to its final name once fully
    /// written, so a crash or connection drop mid-download can never leave
    /// behind a file `voice_paths()` (src/tts.rs) would mistake for a
    /// complete, loadable voice.
    fn download_voice_files(
        voice_name: &str,
        onnx_url: &str,
        json_url: &str,
        mut on_progress: impl FnMut(i32),
    ) -> Result<(), String> {
        use std::io::{Read, Write};

        let dir = crate::assets::downloaded_voices_dir();
        std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;

        let json_bytes = reqwest::blocking::get(json_url)
            .and_then(|r| r.bytes())
            .map_err(|e| e.to_string())?;
        std::fs::write(dir.join(format!("{voice_name}.onnx.json")), &json_bytes)
            .map_err(|e| e.to_string())?;

        let mut resp = reqwest::blocking::get(onnx_url).map_err(|e| e.to_string())?;
        let total = resp.content_length().unwrap_or(0);
        let tmp_path = dir.join(format!("{voice_name}.onnx.part"));
        let mut file = std::fs::File::create(&tmp_path).map_err(|e| e.to_string())?;
        let mut downloaded: u64 = 0;
        let mut buf = [0u8; 64 * 1024];
        loop {
            let n = resp.read(&mut buf).map_err(|e| e.to_string())?;
            if n == 0 {
                break;
            }
            file.write_all(&buf[..n]).map_err(|e| e.to_string())?;
            downloaded += n as u64;
            if let Some(percent) = (downloaded * 100).checked_div(total) {
                on_progress(percent as i32);
            }
        }
        drop(file);
        std::fs::rename(&tmp_path, dir.join(format!("{voice_name}.onnx")))
            .map_err(|e| e.to_string())?;
        Ok(())
    }

    fn delete_voice(voice_name: &str) {
        let dir = crate::assets::downloaded_voices_dir();
        let _ = std::fs::remove_file(dir.join(format!("{voice_name}.onnx")));
        let _ = std::fs::remove_file(dir.join(format!("{voice_name}.onnx.json")));
    }

    const GAMES: &[&str] = &["Everquest Legends"];
    const GAME_IDS: &[&str] = &["eql"];

    fn label_to_game_id(label: &str) -> &'static str {
        GAMES
            .iter()
            .position(|&g| g == label)
            .and_then(|i| GAME_IDS.get(i).copied())
            .unwrap_or("eql")
    }

    fn game_id_to_label(id: &str) -> &'static str {
        GAME_IDS
            .iter()
            .position(|&g| g == id)
            .and_then(|i| GAMES.get(i).copied())
            .unwrap_or(GAMES[0])
    }

    enum UrlTestResult {
        Connected { requires_password: bool },
        Failed(String),
    }

    fn test_url(url: &str) -> UrlTestResult {
        let health = format!("{}/health", url.trim_end_matches('/'));
        match reqwest::blocking::Client::builder()
            .timeout(std::time::Duration::from_secs(6))
            .build()
            .and_then(|c| c.get(&health).send())
        {
            Ok(resp) => {
                let json = resp.json::<serde_json::Value>().ok();
                let is_froklog = json
                    .as_ref()
                    .and_then(|v| v.get("ok")?.as_bool())
                    .unwrap_or(false);
                if !is_froklog {
                    return UrlTestResult::Failed(
                        "Not a froklog server (wrong URL or port?)".into(),
                    );
                }
                let requires = json
                    .and_then(|v| v.get("requires_password")?.as_bool())
                    .unwrap_or(false);
                UrlTestResult::Connected {
                    requires_password: requires,
                }
            }
            Err(e) => UrlTestResult::Failed(e.to_string()),
        }
    }

    enum RegisterResult {
        Ok {
            stream_id: String,
            stream_token: String,
            view_token: String,
        },
        Err(String),
    }

    fn do_register(
        url: &str,
        player: &str,
        server: &str,
        game: &str,
        password: &str,
        public_stream: bool,
    ) -> RegisterResult {
        let endpoint = format!("{}/stream", url.trim_end_matches('/'));
        let body = serde_json::json!({ "player": player, "server": server, "game": game, "public_stream": public_stream });

        let client = match reqwest::blocking::Client::builder()
            .timeout(std::time::Duration::from_secs(10))
            .build()
        {
            Ok(c) => c,
            Err(e) => return RegisterResult::Err(e.to_string()),
        };

        let mut req = client.post(&endpoint).json(&body);
        if !password.is_empty() {
            req = req.bearer_auth(password);
        }

        let resp = match req.send() {
            Ok(r) => r,
            Err(e) => return RegisterResult::Err(format!("Request failed: {e}")),
        };

        let status = resp.status();
        if status == reqwest::StatusCode::UNAUTHORIZED {
            return RegisterResult::Err(
                "Server requires a password — fill in the Password field.".into(),
            );
        }
        if !status.is_success() {
            return RegisterResult::Err(format!("Server returned {status}"));
        }

        let json: serde_json::Value = match resp.json() {
            Ok(v) => v,
            Err(e) => return RegisterResult::Err(format!("Bad response: {e}")),
        };

        if let Some(err) = json.get("error").and_then(|e| e.as_str()) {
            return RegisterResult::Err(format!("Server error: {err}"));
        }

        let field = |k: &str| -> Result<String, String> {
            json[k]
                .as_str()
                .map(|s| s.to_string())
                .ok_or_else(|| format!("missing {k} in server response"))
        };

        match (
            field("stream_id"),
            field("stream_token"),
            field("view_token"),
        ) {
            (Ok(id), Ok(tok), Ok(vtok)) => RegisterResult::Ok {
                stream_id: id,
                stream_token: tok,
                view_token: vtok,
            },
            (Err(e), _, _) | (_, Err(e), _) | (_, _, Err(e)) => RegisterResult::Err(e),
        }
    }
}
