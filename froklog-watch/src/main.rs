//! froklog-watch — a tray service that keeps every EverQuest character's log
//! flowing into froklog.
//!
//! The upstream client watches exactly one log file, which is fine until you
//! play more than one character: EQ writes a separate log per character per
//! server, and froklog wants a separate stream for each. This sits in the tray,
//! finds the logs, registers a stream per character, runs one pipeline each,
//! and hands you the links.

mod alerts;
mod autostart;
mod engine;
mod icon;
mod logscan;
mod messages;
mod meter_core;
mod meter_ui;
mod outputs;
mod overlay;
mod registry;

use std::collections::BTreeMap;
use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex};

use eframe::egui;
use registry::Registry;

/// What the tray asks the window to do.
enum TrayMsg {
    Show,
    /// Copy a link. The tray thread has no clipboard of its own, so it hands
    /// the text to the window, which does hold one.
    Copy(String, String),
    ToggleMeter,
    Quit,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Tab {
    Server,
    Characters,
    Triggers,
    Sounds,
    Speech,
    Meter,
    Messages,
}

/// One character as the tray menu needs it.
/// Stream registration outcomes in flight: key -> Ok((id, stream, view)).
type PendingRegs = Arc<Mutex<BTreeMap<String, Result<(String, String, String), String>>>>;
/// A history import's live counters: (finished?, events pushed).
type ImportProgress = (
    Arc<std::sync::atomic::AtomicBool>,
    Arc<std::sync::atomic::AtomicU64>,
);
/// Stream deletions: key -> None while running, Some(outcome) when done.
type Deletions = Arc<Mutex<BTreeMap<String, Option<Result<(), String>>>>>;

#[derive(Clone)]
struct TrayChar {
    label: String,
    view_url: String,
    share_url: String,
    public: bool,
}

/// Tray artwork, decoded once: multiple sizes per state so HiDPI panels
/// pick a crisp pixmap instead of upscaling a favicon.
struct TrayArt {
    green: Vec<ksni::Icon>,
    orange: Vec<ksni::Icon>,
    gray: Vec<ksni::Icon>,
}

impl TrayArt {
    fn load() -> Self {
        let prep = |set: &[&'static [u8]]| {
            set.iter()
                .map(|bytes| {
                    let img = icon::decode(bytes);
                    ksni::Icon {
                        width: img.width as i32,
                        height: img.height as i32,
                        data: icon::to_argb(&img),
                    }
                })
                .collect()
        };
        Self {
            green: prep(&icon::TRAY_GREEN),
            orange: prep(&icon::TRAY_ORANGE),
            gray: prep(&icon::TRAY_GRAY),
        }
    }
}

struct Tray {
    art: TrayArt,
    tx: crossbeam_channel::Sender<TrayMsg>,
    /// registered characters, rebuilt whenever the registry changes
    links: Arc<Mutex<Vec<TrayChar>>>,
    watching: Arc<Mutex<usize>>,
    /// false while any watched pipeline's pusher is disconnected — the
    /// tray goes orange (the Windows client's "wants to push, can't" color)
    connected_ok: Arc<Mutex<bool>>,
    /// whether the DPS meter overlay is on, so the menu label can flip
    meter_on: Arc<Mutex<bool>>,
}

impl ksni::Tray for Tray {
    fn id(&self) -> String {
        "froklog-watch".into()
    }
    fn title(&self) -> String {
        let n = *self.watching.lock().unwrap();
        match n {
            0 => "froklog — not watching".into(),
            1 => "froklog — watching 1 character".into(),
            n => format!("froklog — watching {n} characters"),
        }
    }
    // froklog's own favicon, so the tray icon matches the browser tab. An empty
    // icon_name is what tells the host to use the pixmap instead of a theme icon.
    fn icon_name(&self) -> String {
        String::new()
    }
    fn icon_pixmap(&self) -> Vec<ksni::Icon> {
        let art = if *self.watching.lock().unwrap() == 0 {
            &self.art.gray
        } else if *self.connected_ok.lock().unwrap() {
            &self.art.green
        } else {
            &self.art.orange
        };
        art.clone()
    }
    fn activate(&mut self, _x: i32, _y: i32) {
        let _ = self.tx.send(TrayMsg::Show);
    }
    fn menu(&self) -> Vec<ksni::MenuItem<Self>> {
        use ksni::menu::*;
        let mut items: Vec<ksni::MenuItem<Self>> = vec![StandardItem {
            label: "Open froklog-watch".into(),
            activate: Box::new(|t: &mut Tray| {
                let _ = t.tx.send(TrayMsg::Show);
            }),
            ..Default::default()
        }
        .into()];

        let links = self.links.lock().unwrap().clone();
        if !links.is_empty() {
            items.push(MenuItem::Separator);
            for c in links {
                // View opens it here; Share puts the link on the clipboard for
                // someone else. The Share label says which link you are about
                // to hand out, because one of them cannot be taken back.
                let view = c.view_url.clone();
                let share = c.share_url.clone();
                let share_label = if c.public {
                    "Share — copy public link"
                } else {
                    "Share — copy private link"
                };
                let who = c.label.clone();
                items.push(
                    SubMenu {
                        label: c.label.clone(),
                        submenu: vec![
                            StandardItem {
                                label: "View in browser".into(),
                                activate: Box::new(move |_: &mut Tray| {
                                    let _ = open::that_detached(&view);
                                }),
                                ..Default::default()
                            }
                            .into(),
                            StandardItem {
                                label: share_label.into(),
                                activate: Box::new(move |t: &mut Tray| {
                                    let _ = t.tx.send(TrayMsg::Copy(who.clone(), share.clone()));
                                }),
                                ..Default::default()
                            }
                            .into(),
                        ],
                        ..Default::default()
                    }
                    .into(),
                );
            }
        }

        items.push(MenuItem::Separator);
        items.push(
            StandardItem {
                label: if *self.meter_on.lock().unwrap() {
                    "Hide DPS Meter".into()
                } else {
                    "Show DPS Meter".into()
                },
                activate: Box::new(|t: &mut Tray| {
                    let _ = t.tx.send(TrayMsg::ToggleMeter);
                }),
                ..Default::default()
            }
            .into(),
        );
        items.push(
            StandardItem {
                label: "Quit".into(),
                activate: Box::new(|t: &mut Tray| {
                    let _ = t.tx.send(TrayMsg::Quit);
                }),
                ..Default::default()
            }
            .into(),
        );
        items
    }
}

struct App {
    reg: Registry,
    rt: tokio::runtime::Runtime,
    running: BTreeMap<String, engine::Handle>,
    status: String,
    rx: crossbeam_channel::Receiver<TrayMsg>,
    links: Arc<Mutex<Vec<TrayChar>>>,
    watching: Arc<Mutex<usize>>,
    dirs_buf: String,
    /// registrations in flight, so the UI can say so and not fire twice
    pending: PendingRegs,
    busy: Arc<Mutex<Vec<String>>>,
    /// history imports in flight: key -> (finished?, events pushed)
    imports: BTreeMap<String, ImportProgress>,
    /// publish flips in flight: key -> the new value, or why it failed
    publishing: Arc<Mutex<BTreeMap<String, Result<bool, String>>>>,
    /// stream deletions: key -> None while running, Some(outcome) when done
    deleting: Deletions,
    /// character whose Delete button is armed, waiting for the second click
    delete_arm: Option<String>,
    /// his trigger engine, wired to Linux audio
    alerts: alerts::Alerts,
    /// pipelines must be rebuilt to start or stop feeding the engine
    restart_watching: bool,
    tab: Tab,
    /// scratch text for auditioning a voice or testing a trigger
    phrase: String,
    /// the pronunciation table, edited in place and written on Save
    pron: Vec<(String, String)>,
    /// a log line being turned into a pattern, and which of its words vary
    builder_line: String,
    builder_chosen: std::collections::BTreeSet<usize>,
    /// words that vary but aren't wanted as captures — adjacent ones merge
    builder_wild: std::collections::BTreeSet<usize>,
    /// the pattern as shown — user-editable; regenerated on word clicks
    builder_pattern: String,
    builder_pattern_auto: String,
    /// trigger index whose × is armed, waiting for the second click
    trigger_delete_arm: Option<usize>,
    /// trigger being edited in the builder — Some means Create becomes Save
    edit_index: Option<usize>,
    /// the pattern was loaded or hand-written, so the word picker must not
    /// silently regenerate over it when a line is pasted to test against
    pattern_manual: bool,
    new_name: String,
    new_sound: String,
    new_say: String,
    /// text a trigger puts on the message overlay
    new_show: String,
    /// triggers.toml open in the built-in editor
    editing: bool,
    trigger_text: String,
    trigger_err: Option<String>,
    /// every watched log's lines, merged, on their way to the engine
    lines_tx: crossbeam_channel::Sender<String>,
    lines_rx: crossbeam_channel::Receiver<String>,
    /// started with --hidden and not yet hidden. eframe deliberately shows the
    /// window after its first paint (it hides windows until they have
    /// something to draw), so asking for an invisible window at startup is
    /// overridden. We take it back on the frame after that.
    hide_pending: bool,
    frames: u32,
    /// wgpu instance made at startup, before any event loop exists — creating
    /// it later deadlocks in the NVIDIA driver (see overlay::preflight_instance)
    meter_instance: Option<Arc<egui_wgpu::wgpu::Instance>>,
    /// the DPS meter's layer-shell thread, while the meter is enabled
    overlay: Option<overlay::OverlayHandle>,
    /// layer-shell failed (GNOME, plain X11) — render the meter as an
    /// always-on-top egui viewport instead
    meter_x11: bool,
    meter_view: meter_ui::MeterView,
    /// last drag movement, so position saves are debounced to once per drop
    meter_moved_at: Option<std::time::Instant>,
    meter_on: Arc<Mutex<bool>>,
    /// shared with the tray: all watched pipelines currently connected?
    tray_connected_ok: Arc<Mutex<bool>>,
    /// whether the overlay was last told to preview (Meter tab open)
    meter_preview: bool,
    /// the trigger message overlay's layer-shell thread, while it is enabled
    msg_overlay: Option<overlay::OverlayHandle>,
    /// layer-shell unavailable — the message overlay has no X11 fallback
    /// (unlike the meter, it has nothing to show most of the time, so an
    /// always-on-top viewport would just be an empty window in the way)
    msg_x11: bool,
    msg_preview: bool,
    msg_moved_at: Option<std::time::Instant>,
    /// monitor list for the Meter tab's picker, fetched when the tab opens
    monitors: Option<Vec<(String, String)>>,
    /// Triggers tab scratch: log search text and results, template scan
    log_search: String,
    log_results: Vec<String>,
    log_templates: Vec<(u32, String)>,
    /// Sounds tab scratch: armed package delete, import path, new-label form
    pkg_delete_arm: bool,
    import_zip: String,
    new_label_name: String,
    new_label_file: String,
}

/// The same artwork the tray uses, for the titlebar and dock entry.
fn window_icon() -> egui::IconData {
    let img = icon::decode(icon::WINDOW);
    egui::IconData {
        rgba: img.pixels,
        width: img.width,
        height: img.height,
    }
}

impl App {
    fn sync_tray(&self) {
        let mut links = Vec::new();
        let mut n = 0;
        for c in self.reg.characters.values() {
            if c.enabled {
                n += 1;
            }
            let s = &self.reg.settings;
            if let (Some(view), Some(share)) = (
                c.view_url(&s.server_url),
                c.share_url(&s.server_url, &s.game),
            ) {
                links.push(TrayChar {
                    label: c.key(),
                    view_url: view,
                    share_url: share,
                    public: c.public,
                });
            }
        }
        *self.links.lock().unwrap() = links;
        *self.watching.lock().unwrap() = n;
    }

    fn save(&mut self) {
        if let Err(e) = self.reg.save() {
            self.status = format!("could not save config: {e}");
        }
        self.sync_tray();
    }

    /// This install's front door. Empty until a character is registered:
    /// the page only exists once the server has a stream carrying the token.
    fn home_url(&self) -> String {
        let s = &self.reg.settings;
        if s.home_token.is_empty() || !self.reg.characters.values().any(|c| c.registered()) {
            return String::new();
        }
        format!(
            "{}/home?key={}",
            s.server_url.trim_end_matches('/'),
            s.home_token
        )
    }

    fn overlay_feeds(&self) -> Vec<overlay::Feed> {
        self.running
            .values()
            .map(|h| overlay::Feed {
                combat: Arc::clone(&h.combat),
                reset: Arc::clone(&h.reset),
            })
            .collect()
    }

    /// Bring the overlay thread in line with `meter_enabled` and hand it the
    /// current pipelines. Layer-shell is only attempted on Wayland; anywhere
    /// else the meter renders as an always-on-top viewport of this app.
    fn sync_overlay(&mut self) {
        let s = &self.reg.settings;
        *self.meter_on.lock().unwrap() = s.meter_enabled;
        let wayland = std::env::var_os("WAYLAND_DISPLAY").is_some();
        if s.meter_enabled && self.overlay.is_none() && !self.meter_x11 {
            if let (true, Some(instance)) = (wayland, self.meter_instance.clone()) {
                self.overlay = Some(overlay::spawn(overlay::OverlaySpawn {
                    kind: overlay::Kind::Meter,
                    msg_style: Default::default(),
                    instance,
                    output: s.meter_output.clone(),
                    feeds: self.overlay_feeds(),
                    locked: s.meter_locked,
                    x: s.meter_x,
                    y: s.meter_y,
                    width: s.meter_width,
                    max_rows: s.meter_max_rows,
                    font_size: s.meter_font_size,
                    idle_secs: s.meter_idle_secs,
                }));
                // A fresh surface starts with preview off; let the update
                // loop's change detection re-send it if the tab is open.
                self.meter_preview = false;
            } else {
                self.meter_x11 = true;
            }
        } else if !s.meter_enabled {
            if let Some(o) = self.overlay.take() {
                let _ = o.tx.send(overlay::OverlayMsg::Quit);
            }
        } else if let Some(o) = &self.overlay {
            let _ = o.tx.send(overlay::OverlayMsg::Feeds(self.overlay_feeds()));
        }
    }

    fn msg_style(&self) -> messages::MessageStyle {
        let s = &self.reg.settings;
        messages::MessageStyle {
            peak_size: s.msg_peak_size,
            hold_secs: s.msg_hold_secs,
            history_rows: s.msg_history_rows,
            idle_secs: s.msg_idle_secs,
            ..Default::default()
        }
    }

    /// Same shape as `sync_overlay`, for the trigger message window.
    fn sync_msg_overlay(&mut self) {
        let s = &self.reg.settings;
        let wayland = std::env::var_os("WAYLAND_DISPLAY").is_some();
        if s.msg_enabled && self.msg_overlay.is_none() && !self.msg_x11 {
            if let (true, Some(instance)) = (wayland, self.meter_instance.clone()) {
                self.msg_overlay = Some(overlay::spawn(overlay::OverlaySpawn {
                    kind: overlay::Kind::Messages,
                    msg_style: self.msg_style(),
                    instance,
                    output: s.msg_output.clone(),
                    feeds: Vec::new(),
                    locked: s.msg_locked,
                    x: s.msg_x,
                    y: s.msg_y,
                    width: s.msg_width,
                    max_rows: 0,
                    font_size: 14.0,
                    idle_secs: s.msg_idle_secs,
                }));
                self.msg_preview = false;
            } else {
                self.msg_x11 = true;
            }
        } else if !s.msg_enabled {
            if let Some(o) = self.msg_overlay.take() {
                let _ = o.tx.send(overlay::OverlayMsg::Quit);
            }
        }
    }

    /// Hand one fired trigger's announcement to the message window.
    fn announce(&mut self, msg: messages::Msg) {
        if let Some(o) = &self.msg_overlay {
            let _ = o.tx.send(overlay::OverlayMsg::Announce(Box::new(msg)));
        }
    }

    fn push_msg_settings(&self) {
        if let Some(o) = &self.msg_overlay {
            let _ =
                o.tx.send(overlay::OverlayMsg::SetMessageStyle(self.msg_style()));
            let _ =
                o.tx.send(overlay::OverlayMsg::SetLocked(self.reg.settings.msg_locked));
        }
    }

    /// Drags to persist, and death to notice. The message window has no
    /// clipboard or settings chrome, so those arms cannot occur.
    fn poll_msg_overlay(&mut self) {
        let Some(o) = &self.msg_overlay else { return };
        let mut fell = false;
        while let Ok(ev) = o.events.try_recv() {
            match ev {
                overlay::OverlayEvent::Moved(x, y) => {
                    self.reg.settings.msg_x = x;
                    self.reg.settings.msg_y = y;
                    self.msg_moved_at = Some(std::time::Instant::now());
                }
                overlay::OverlayEvent::Exited(err) => {
                    if let Some(e) = err {
                        self.status = format!("message overlay stopped: {e}");
                    }
                    fell = true;
                }
                overlay::OverlayEvent::Copy(_) | overlay::OverlayEvent::OpenSettings => {}
            }
        }
        if fell {
            self.msg_overlay = None;
            self.msg_x11 = true;
        }
        // Debounced like the meter's: one save per drop, not per motion.
        if let Some(t) = self.msg_moved_at {
            if t.elapsed().as_millis() > 600 {
                self.msg_moved_at = None;
                self.save();
            }
        }
    }

    /// Push the current meter style/lock to a running overlay.
    fn push_overlay_settings(&self) {
        let s = &self.reg.settings;
        if let Some(o) = &self.overlay {
            let _ = o.tx.send(overlay::OverlayMsg::SetStyle {
                max_rows: s.meter_max_rows,
                font_size: s.meter_font_size,
                idle_secs: s.meter_idle_secs,
            });
            let _ = o.tx.send(overlay::OverlayMsg::SetLocked(s.meter_locked));
        }
    }

    /// Service what the overlay reported back: drags to persist, clipboard
    /// copies (the main window owns the clipboard), settings requests, and
    /// death (fall back to the viewport meter).
    fn poll_overlay(&mut self, ctx: &egui::Context) {
        let Some(o) = &self.overlay else { return };
        let mut exited = None;
        let mut fell = false;
        while let Ok(ev) = o.events.try_recv() {
            match ev {
                overlay::OverlayEvent::Moved(x, y) => {
                    self.reg.settings.meter_x = x;
                    self.reg.settings.meter_y = y;
                    self.meter_moved_at = Some(std::time::Instant::now());
                }
                overlay::OverlayEvent::Copy(text) => {
                    ctx.output_mut(|out| out.copied_text = text);
                    self.status = "meter summary copied".into();
                }
                overlay::OverlayEvent::OpenSettings => {
                    self.tab = Tab::Meter;
                    ctx.send_viewport_cmd(egui::ViewportCommand::Visible(true));
                    ctx.send_viewport_cmd(egui::ViewportCommand::Focus);
                }
                overlay::OverlayEvent::Exited(err) => {
                    exited = err;
                    fell = true;
                }
            }
        }
        if fell {
            self.overlay = None;
            if let Some(e) = exited {
                // No layer-shell: keep the meter, change the window backend.
                self.meter_x11 = true;
                self.status = format!("meter: {e} — using fallback window");
            }
        }
        // Persist a finished drag: one write when the mouse has settled, not
        // one per motion event.
        if self
            .meter_moved_at
            .is_some_and(|t| t.elapsed().as_millis() > 700)
        {
            self.meter_moved_at = None;
            self.save();
        }
    }

    /// The meter as an always-on-top egui viewport — the fallback for
    /// sessions without layer-shell (GNOME Wayland via XWayland, plain X11).
    /// Same `meter_ui` as the Wayland surface, different window plumbing.
    fn show_meter_viewport(&mut self, ctx: &egui::Context) {
        let s = self.reg.settings.clone();
        let feeds = self.overlay_feeds();
        let preview = self.tab == Tab::Meter;
        let feed = match feeds
            .iter()
            .max_by_key(|f| f.combat.load().mob_list.iter().map(|m| m.last_seen).max())
            .cloned()
        {
            Some(f) => f,
            None if preview => overlay::Feed {
                combat: Arc::new(arc_swap::ArcSwap::from_pointee(
                    froklog::state::CombatState::default(),
                )),
                reset: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            },
            None => return,
        };
        let cs = feed.combat.load_full();
        let style = meter_ui::MeterStyle {
            max_rows: s.meter_max_rows,
            font_size: s.meter_font_size,
        };
        let vp = egui::ViewportId::from_hash_of("froklog-meter");
        let view = &mut self.meter_view;
        let mut actions = Vec::new();
        let mut has_content = false;
        ctx.show_viewport_immediate(
            vp,
            egui::ViewportBuilder::default()
                .with_title("froklog meter")
                .with_inner_size([s.meter_width as f32, 300.0])
                .with_position([s.meter_x as f32, s.meter_y as f32])
                .with_decorations(false)
                .with_transparent(true)
                .with_always_on_top()
                .with_taskbar(false),
            |ctx, _| {
                ctx.send_viewport_cmd(egui::ViewportCommand::MousePassthrough(s.meter_locked));
                egui::CentralPanel::default()
                    .frame(egui::Frame::none())
                    .show(ctx, |ui| {
                        let panel = egui::Frame::none()
                            .fill(egui::Color32::from_rgba_unmultiplied(10, 10, 16, 208))
                            .rounding(8.0)
                            .inner_margin(8.0);
                        panel.show(ui, |ui| {
                            ui.set_width(ui.available_width() - 16.0);
                            let (acts, content) =
                                meter_ui::draw(ui, view, &cs, style, s.meter_locked, preview);
                            actions = acts;
                            has_content = content;
                        });
                    });
            },
        );
        for act in actions {
            match act {
                meter_ui::MeterAction::Drag(delta) => {
                    let x = (self.reg.settings.meter_x + delta.x.round() as i32).max(0);
                    let y = (self.reg.settings.meter_y + delta.y.round() as i32).max(0);
                    self.reg.settings.meter_x = x;
                    self.reg.settings.meter_y = y;
                    ctx.send_viewport_cmd_to(
                        vp,
                        egui::ViewportCommand::OuterPosition(egui::pos2(x as f32, y as f32)),
                    );
                    self.meter_moved_at = Some(std::time::Instant::now());
                }
                meter_ui::MeterAction::Copy(text) => {
                    ctx.output_mut(|out| out.copied_text = text);
                    self.status = "meter summary copied".into();
                }
                meter_ui::MeterAction::Reset => {
                    feed.reset.store(true, Ordering::Relaxed);
                }
                meter_ui::MeterAction::OpenSettings => {
                    self.tab = Tab::Meter;
                    ctx.send_viewport_cmd(egui::ViewportCommand::Visible(true));
                }
            }
        }
        if self
            .meter_moved_at
            .is_some_and(|t| t.elapsed().as_millis() > 700)
        {
            self.meter_moved_at = None;
            self.save();
        }
        // keep the fallback meter's clocks moving
        ctx.request_repaint_after(std::time::Duration::from_millis(250));
    }

    /// Bring running pipelines in line with what is ticked.
    fn reconcile(&mut self) {
        if std::mem::take(&mut self.restart_watching) {
            // whether a pipeline feeds the trigger engine is fixed when it
            // starts, so toggling triggers means starting them again
            self.running.clear();
        }
        let settings = self.reg.settings.clone();
        for (key, ch) in self.reg.characters.clone() {
            // Enabled is enough: an unregistered character runs local-only
            // (engine::start skips the pusher when there is no stream).
            let should_run = ch.enabled;
            let is_running = self.running.contains_key(&key);
            if should_run && !is_running {
                let sink = self
                    .reg
                    .settings
                    .triggers_enabled
                    .then(|| self.lines_tx.clone());
                match engine::start(self.rt.handle(), &settings, &ch, sink) {
                    Ok(h) => {
                        self.running.insert(key.clone(), h);
                        self.status = format!("watching {key}");
                    }
                    Err(e) => self.status = format!("{key}: {e}"),
                }
            } else if !should_run && is_running {
                self.running.remove(&key); // Drop stops it
                self.status = format!("stopped {key}");
            }
        }
        self.sync_overlay();
        self.sync_msg_overlay();
    }

    fn import(&mut self, key: String) {
        let Some(ch) = self.reg.characters.get(&key).cloned() else {
            return;
        };
        let done = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let progress = Arc::new(std::sync::atomic::AtomicU64::new(0));
        match engine::import_history(
            self.rt.handle(),
            &self.reg.settings,
            &ch,
            Arc::clone(&done),
            Arc::clone(&progress),
        ) {
            Ok(()) => {
                self.imports.insert(key.clone(), (done, progress));
                self.status = format!("importing {key}'s history…");
            }
            Err(e) => self.status = format!("{key}: {e}"),
        }
    }

    /// Mark finished imports so the button does not invite a second run.
    fn collect_imports(&mut self) {
        let finished: Vec<String> = self
            .imports
            .iter()
            .filter(|(_, (done, _))| done.load(Ordering::Relaxed))
            .map(|(k, _)| k.clone())
            .collect();
        if finished.is_empty() {
            return;
        }
        for key in finished {
            if let Some((_, sent)) = self.imports.remove(&key) {
                if let Some(c) = self.reg.characters.get_mut(&key) {
                    c.imported = true;
                }
                self.status = format!("{key}: imported {} events", sent.load(Ordering::Relaxed));
            }
        }
        self.save();
    }

    /// Ask the server to publish or unpublish, and only believe it once it
    /// agrees — the tick box reflects the server, not our intention.
    fn set_public(&mut self, key: String, want: bool) {
        let Some(ch) = self.reg.characters.get(&key).cloned() else {
            return;
        };
        let settings = self.reg.settings.clone();
        let out = Arc::clone(&self.publishing);
        let busy = Arc::clone(&self.busy);
        busy.lock().unwrap().push(key.clone());
        let job_key = key.clone();
        self.rt.spawn(async move {
            let res = engine::set_public(&settings, &ch, want)
                .await
                .map(|()| want)
                .map_err(|e| e.to_string());
            out.lock().unwrap().insert(job_key.clone(), res);
            busy.lock().unwrap().retain(|k| *k != job_key);
        });
        self.status = format!(
            "{} {key}…",
            if want { "publishing" } else { "unpublishing" }
        );
    }

    fn delete(&mut self, key: String) {
        let Some(ch) = self.reg.characters.get(&key).cloned() else {
            return;
        };
        let settings = self.reg.settings.clone();
        let deleting = Arc::clone(&self.deleting);
        self.deleting.lock().unwrap().insert(key.clone(), None);
        self.rt.spawn(async move {
            let res = engine::delete_stream(&settings, &ch)
                .await
                .map_err(|e| e.to_string());
            deleting.lock().unwrap().insert(key, Some(res));
        });
        self.status = "deleting stream…".into();
    }

    fn collect_deletes(&mut self) {
        let done: Vec<(String, Result<(), String>)> = {
            let mut d = self.deleting.lock().unwrap();
            let finished: Vec<String> = d
                .iter()
                .filter(|(_, v)| v.is_some())
                .map(|(k, _)| k.clone())
                .collect();
            finished
                .into_iter()
                .filter_map(|k| d.remove(&k).flatten().map(|r| (k, r)))
                .collect()
        };
        if done.is_empty() {
            return;
        }
        for (key, res) in done {
            match res {
                Ok(()) => {
                    if let Some(c) = self.reg.characters.get_mut(&key) {
                        c.stream_id = None;
                        c.stream_token = None;
                        c.view_token = None;
                        c.enabled = false;
                        c.public = false;
                        c.imported = false;
                    }
                    self.status = format!("{key}: stream and history deleted");
                }
                Err(e) => self.status = format!("{key}: delete failed — {e}"),
            }
        }
        self.save();
        self.reconcile();
    }

    fn collect_publish(&mut self) {
        let done: Vec<(String, Result<bool, String>)> = {
            let mut p = self.publishing.lock().unwrap();
            let all = p.iter().map(|(k, v)| (k.clone(), v.clone())).collect();
            p.clear();
            all
        };
        if done.is_empty() {
            return;
        }
        for (key, res) in done {
            match res {
                Ok(now) => {
                    if let Some(c) = self.reg.characters.get_mut(&key) {
                        c.public = now;
                    }
                    self.status = if now {
                        format!("{key} has a public page")
                    } else {
                        format!("{key} is private again")
                    };
                }
                Err(e) => self.status = format!("{key}: {e}"),
            }
        }
        self.save();
    }

    fn register(&mut self, key: String) {
        let Some(ch) = self.reg.characters.get(&key).cloned() else {
            return;
        };
        let settings = self.reg.settings.clone();
        let pending = Arc::clone(&self.pending);
        let busy = Arc::clone(&self.busy);
        busy.lock().unwrap().push(key.clone());
        let job_key = key.clone();
        self.rt.spawn(async move {
            let outcome = engine::register(&settings, &ch)
                .await
                .map_err(|e| e.to_string());
            pending.lock().unwrap().insert(job_key.clone(), outcome);
            busy.lock().unwrap().retain(|k| *k != job_key);
        });
        self.status = format!("registering {key}…");
    }

    /// Fold finished registrations back into the registry.
    fn collect_registrations(&mut self) {
        type RegOutcome = (String, Result<(String, String, String), String>);
        let done: Vec<RegOutcome> = {
            let mut p = self.pending.lock().unwrap();
            let all = p.iter().map(|(k, v)| (k.clone(), v.clone())).collect();
            p.clear();
            all
        };
        if done.is_empty() {
            return;
        }
        for (key, res) in done {
            match res {
                Ok((id, stok, vtok)) => {
                    if let Some(c) = self.reg.characters.get_mut(&key) {
                        c.stream_id = Some(id);
                        c.stream_token = Some(stok);
                        c.view_token = Some(vtok);
                        c.enabled = true;
                    }
                    self.status = format!("{key} registered");
                }
                Err(e) => self.status = format!("{key}: {e}"),
            }
        }
        self.save();
        self.reconcile();
    }
}

impl eframe::App for App {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        // the tray lives on another thread; poll its requests
        while let Ok(msg) = self.rx.try_recv() {
            match msg {
                TrayMsg::Show => {
                    ctx.send_viewport_cmd(egui::ViewportCommand::Visible(true));
                    ctx.send_viewport_cmd(egui::ViewportCommand::Focus);
                }
                TrayMsg::Copy(who, url) => {
                    let public = url.contains("/player/");
                    ctx.output_mut(|o| o.copied_text = url);
                    self.status = format!(
                        "{who}: {} link copied",
                        if public { "public" } else { "private" }
                    );
                }
                TrayMsg::ToggleMeter => {
                    self.reg.settings.meter_enabled = !self.reg.settings.meter_enabled;
                    self.save();
                    self.sync_overlay();
                }
                TrayMsg::Quit => std::process::exit(0),
            }
        }
        // Feed the tray's state color: orange when any watched pipeline's
        // pusher has lost the server — or when the server is REJECTING
        // batches, which is worse than disconnected: data is being lost
        // while everything otherwise looks healthy.
        {
            let rejects = froklog::pusher::BATCH_REJECTS.load(Ordering::Relaxed);
            let ok = rejects == 0
                && self
                    .running
                    .values()
                    .all(|h| h.connected.load(Ordering::Relaxed));
            *self.tray_connected_ok.lock().unwrap() = ok;
        }
        self.poll_overlay(ctx);
        self.poll_msg_overlay();
        // While the Meter tab is open the overlay force-renders a placeholder
        // — an idle-hidden meter during setup looks exactly like a crash.
        // Windows' "Show All Windows" idea: while EITHER overlay tab is
        // open, force-show BOTH overlays. Arranging them is a relative job —
        // meter here, messages there — and one at a time made it look like
        // they could not coexist at all.
        let arranging = matches!(self.tab, Tab::Meter | Tab::Messages);
        let want_preview = arranging && self.reg.settings.meter_enabled;
        if want_preview != self.meter_preview {
            self.meter_preview = want_preview;
            if let Some(o) = &self.overlay {
                let _ = o.tx.send(overlay::OverlayMsg::Preview(want_preview));
            }
        }
        // Same for the message window: it is hidden whenever nothing has
        // fired, which is most of the time.
        let want_msg_preview = arranging && self.reg.settings.msg_enabled;
        if want_msg_preview != self.msg_preview {
            self.msg_preview = want_msg_preview;
            if let Some(o) = &self.msg_overlay {
                let _ = o.tx.send(overlay::OverlayMsg::Preview(want_msg_preview));
            }
        }
        if self.reg.settings.meter_enabled && self.meter_x11 {
            self.show_meter_viewport(ctx);
        }
        self.frames += 1;
        if self.hide_pending && self.frames > 1 {
            self.hide_pending = false;
            ctx.send_viewport_cmd(egui::ViewportCommand::Visible(false));
        }
        // closing the window keeps the service running — that is the point of a tray
        if ctx.input(|i| i.viewport().close_requested()) {
            ctx.send_viewport_cmd(egui::ViewportCommand::CancelClose);
            ctx.send_viewport_cmd(egui::ViewportCommand::Visible(false));
        }
        // Sync the process-wide audio state from settings so changes apply
        // to the very next sound.
        alerts::VOLUME.store(self.reg.settings.sound_volume, Ordering::Relaxed);
        alerts::MUTED.store(self.reg.settings.sound_muted, Ordering::Relaxed);
        for line in self.lines_rx.try_iter().take(2000) {
            self.alerts.process_line(&line);
        }
        let voice = alerts::Voice::from_settings(
            &self.reg.settings.voice_engine,
            &self.reg.settings.piper_model,
        );
        for m in self.alerts.pump(&self.reg.settings.sound_package, &voice) {
            self.announce(m);
        }

        self.collect_registrations();
        self.collect_imports();
        self.collect_publish();
        self.collect_deletes();

        egui::CentralPanel::default().show(ctx, |ui| {
            ui.heading("froklog watch");
            ui.label(
                egui::RichText::new(
                    "One stream per character. Tick a character to keep its log flowing.",
                )
                .italics()
                .weak(),
            );

            // Three things live here and they are not the same kind of thing:
            // where to send logs, which characters to send, and what to shout
            // about while it happens. Tabs keep each one whole instead of
            // stacking every setting in one column.
            ui.horizontal(|ui| {
                for (tab, label) in [
                    (Tab::Server, "Server"),
                    (Tab::Characters, "Characters"),
                    (Tab::Triggers, "Triggers"),
                    (Tab::Sounds, "Sounds"),
                    (Tab::Speech, "Speech"),
                    (Tab::Meter, "Meter"),
                    (Tab::Messages, "Messages"),
                ] {
                    if ui.selectable_label(self.tab == tab, label).clicked() {
                        self.tab = tab;
                    }
                }
            });
            ui.separator();

            let mut dirty = false;
            let mut to_register: Option<String> = None;
            let mut to_import: Option<String> = None;
            let mut to_publish: Option<(String, bool)> = None;
            let mut to_delete: Option<String> = None;
            let mut to_copy: Option<String> = None;
            let mut open_url: Option<String> = None;

            match self.tab {
                Tab::Server => {
                    egui::Grid::new("settings").num_columns(2).show(ui, |ui| {
                        ui.label("froklog URL");
                        dirty |= ui
                            .text_edit_singleline(&mut self.reg.settings.server_url)
                            .changed();
                        ui.end_row();
                        ui.label("stream password")
                            .on_hover_text("Only if the server asks for one. Most do not.");
                        dirty |= ui
                            .add(
                                egui::TextEdit::singleline(&mut self.reg.settings.stream_password)
                                    .password(true)
                                    .hint_text("usually blank"),
                            )
                            .changed();
                        ui.end_row();
                        ui.label("planner URL");
                        dirty |= ui
                            .text_edit_singleline(&mut self.reg.settings.planner_url)
                            .changed();
                        ui.end_row();

                        ui.label("front door")
                            .on_hover_text(
                                "One page listing every character this install \
                                 streams — bookmark it instead of keeping a link \
                                 per character.",
                            );
                        ui.horizontal(|ui| {
                            let url = self.home_url();
                            ui.label(
                                egui::RichText::new(if url.is_empty() {
                                    "(register a character first)".to_string()
                                } else {
                                    format!(
                                        "{}…",
                                        &url[..url.len().min(46)]
                                    )
                                })
                                .monospace()
                                .small(),
                            );
                            if !url.is_empty() {
                                if ui
                                    .small_button("copy")
                                    .on_hover_text(
                                        "This address IS the key — anyone with it \
                                         sees every character's links. Treat it \
                                         like a password.",
                                    )
                                    .clicked()
                                {
                                    ui.output_mut(|o| o.copied_text = url.clone());
                                    self.status = "front-door link copied".into();
                                }
                                if ui.small_button("open").clicked() {
                                    let _ = open::that_detached(&url);
                                }
                            }
                        });
                        ui.end_row();
                    ui.label("voice");
                    egui::ComboBox::from_id_salt("voice_engine")
                        .selected_text(self.reg.settings.voice_engine.clone())
                        .show_ui(ui, |ui| {
                            dirty |= ui
                                .selectable_value(
                                    &mut self.reg.settings.voice_engine,
                                    "speech-dispatcher".into(),
                                    "speech-dispatcher",
                                )
                                .on_hover_text("Always available. Sounds robotic.")
                                .changed();
                            dirty |= ui
                                .selectable_value(
                                    &mut self.reg.settings.voice_engine,
                                    "piper".into(),
                                    "piper (neural)",
                                )
                                .on_hover_text("Natural voice. Needs piper and a model file.")
                                .changed();
                        });
                    ui.end_row();
                    // choosing piper with nothing selected should just work
                    if self.reg.settings.voice_engine == "piper"
                        && self.reg.settings.piper_model.is_empty()
                    {
                        if let Some((_, path)) = alerts::installed_voices().first() {
                            self.reg.settings.piper_model = path.clone();
                            dirty = true;
                        }
                    }

                    if self.reg.settings.voice_engine == "piper" {
                        ui.label("piper voice");
                        let voices = alerts::installed_voices();
                        if voices.is_empty() {
                            ui.label(
                                egui::RichText::new("none in ~/.local/share/piper/voices")
                                    .weak()
                                    .small(),
                            );
                        } else {
                            // whatever is in that folder, so a voice added later
                            // needs no change here
                            let current = voices
                                .iter()
                                .find(|(_, p)| *p == self.reg.settings.piper_model)
                                .map(|(n, _)| n.clone())
                                .unwrap_or_else(|| "choose a voice".into());
                            egui::ComboBox::from_id_salt("piper_voice")
                                .selected_text(current)
                                .show_ui(ui, |ui| {
                                    for (name, path) in &voices {
                                        dirty |= ui
                                            .selectable_value(
                                                &mut self.reg.settings.piper_model,
                                                path.clone(),
                                                name,
                                            )
                                            .changed();
                                    }
                                });
                        }
                        ui.end_row();
                    }
                    });
                    ui.label(
                        egui::RichText::new(
                            "Most froklog servers let anyone create a stream, so leave the \
                             password blank unless yours refuses. It stays on this machine.",
                        )
                        .weak()
                        .small(),
                    );
                    ui.add_space(8.0);
                    ui.label(egui::RichText::new("Log folders").strong());
                    ui.label("One folder per line — your EverQuest Logs directories.");
                    if ui
                        .add(
                            egui::TextEdit::multiline(&mut self.dirs_buf)
                                .desired_rows(3)
                                .desired_width(f32::INFINITY),
                        )
                        .changed()
                    {
                        self.reg.settings.log_dirs = self
                            .dirs_buf
                            .lines()
                            .map(|l| l.trim().to_string())
                            .filter(|l| !l.is_empty())
                            .collect();
                        dirty = true;
                    }
                    if ui.button("Scan for characters").clicked() {
                        let found = self.reg.scan();
                        self.status = format!("found {found} log file(s)");
                        dirty = true;
                    }
                    ui.add_space(8.0);
                    ui.separator();
                    let mut login = autostart::is_enabled();
                    if ui
                        .checkbox(&mut login, "Start at login")
                        .on_hover_text(
                            "Launch straight to the tray when you log in, so logs \
                             keep flowing without you remembering to start this.",
                        )
                        .changed()
                    {
                        let res = if login { autostart::enable() } else { autostart::disable() };
                        self.status = match res {
                            Ok(()) if login => "will start at login".into(),
                            Ok(()) => "will no longer start at login".into(),
                            Err(e) => format!("could not change autostart: {e}"),
                        };
                    }
                }
                Tab::Triggers => {
                ui.horizontal(|ui| {
                    let mut on = self.reg.settings.triggers_enabled;
                    if ui
                        .checkbox(&mut on, "Sound and voice alerts")
                        .on_hover_text(
                            "Match every watched log against triggers.toml and play \
                             the sounds it asks for. Takes effect on the next watch.",
                        )
                        .changed()
                    {
                        self.reg.settings.triggers_enabled = on;
                        self.alerts.enabled = on;
                        dirty = true;
                        // the pipelines have to be rebuilt to start or stop
                        // feeding lines to the engine
                        self.restart_watching = true;
                    }
                    ui.label(
                        egui::RichText::new(format!("{} loaded", self.alerts.count()))
                            .weak()
                            .small(),
                    );
                });

                ui.horizontal(|ui| {
                    if ui
                        .button("Reload triggers")
                        .on_hover_text(alerts::Alerts::path().display().to_string())
                        .clicked()
                    {
                        self.alerts.reload();
                        self.status = format!("{} triggers loaded", self.alerts.count());
                    }
                    // Edited here rather than handed to xdg-open: launching an
                    // external editor goes through the desktop portal, and on
                    // COSMIC that portal crashes, which looks exactly like this
                    // app crashing. It is also his file format, so the text is
                    // the honest editor — a hand-written trigger from a Windows
                    // player drops straight in.
                    let label = if self.editing { "Close editor" } else { "Edit file" };
                    if ui
                        .button(label)
                        .on_hover_text(alerts::Alerts::path().display().to_string())
                        .clicked()
                    {
                        self.editing = !self.editing;
                        if self.editing {
                            self.trigger_text = alerts::Alerts::read_file();
                            self.trigger_err = None;
                        }
                    }
                    if ui
                        .button("Say a phrase")
                        .on_hover_text("Speak with the voice chosen under Server.")
                        .clicked()
                    {
                        let voice = alerts::Voice::from_settings(
                            &self.reg.settings.voice_engine,
                            &self.reg.settings.piper_model,
                        );
                        self.status = if alerts::speak_forced(
                            &self.phrase,
                            &froklog::triggers::engine::VoicePriority::Emergency,
                            &voice,
                        ) {
                            "spoke".into()
                        } else {
                            "that voice engine did not run \u{2014} check Server".into()
                        };
                    }
                    ui.add(
                        egui::TextEdit::singleline(&mut self.phrase)
                            .desired_width(220.0)
                            .hint_text("something to say"),
                    )
                    .on_hover_text("Try a mob or zone name to hear how it is pronounced.");
                });

                if self.editing {
                    ui.add(
                        egui::TextEdit::multiline(&mut self.trigger_text)
                            .code_editor()
                            .desired_rows(14)
                            .desired_width(f32::INFINITY),
                    );
                    ui.horizontal(|ui| {
                        if ui.button("Save").clicked() {
                            match self.alerts.write_file(&self.trigger_text.clone()) {
                                Ok(n) => {
                                    self.trigger_err = None;
                                    self.status = format!("saved \u{2014} {n} triggers active");
                                }
                                // never write a file the engine cannot read
                                Err(e) => self.trigger_err = Some(e),
                            }
                        }
                        if ui.button("Revert").clicked() {
                            self.trigger_text = alerts::Alerts::read_file();
                            self.trigger_err = None;
                        }
                    });
                    if let Some(e) = &self.trigger_err {
                        ui.label(
                            egui::RichText::new(e)
                                .small()
                                .color(egui::Color32::from_rgb(200, 120, 100)),
                        );
                    }
                    ui.separator();
                }

                // Auditioning every voice from here saves switching the setting
                // back and forth to compare two of them.
                let voices = alerts::installed_voices();
                if self.reg.settings.voice_engine == "piper" && voices.len() > 1 {
                    ui.horizontal_wrapped(|ui| {
                        ui.label(egui::RichText::new("voices").weak().small());
                        for (name, path) in &voices {
                            if ui.small_button(name).clicked() {
                                alerts::speak_forced(
                                    &self.phrase,
                                    &froklog::triggers::engine::VoicePriority::Emergency,
                                    &alerts::Voice::Piper { model: path },
                                );
                                self.status = format!("{name}: spoke");
                            }
                        }
                    });
                }

                // Build a pattern by pointing at what varies.
                //
                // Writing a regex by hand is the wall most people hit here, and
                // the mistakes are always the same two: leaving the timestamp
                // in, and forgetting to escape punctuation. Both are handled
                // for you — paste a line, click the words that change.
                ui.separator();
                // ── Trigger material: mine the real log instead of guessing ──
                egui::CollapsingHeader::new("Find a line in your log")
                    .default_open(false)
                    .show(ui, |ui| {
                        ui.label(
                            egui::RichText::new(
                                "Search your newest log (chat excluded) or list every \
                                 kind of message it contains. Click a line to load it \
                                 into the builder below.",
                            )
                            .weak()
                            .small(),
                        );
                        let newest_log = self
                            .reg
                            .characters
                            .values()
                            .filter_map(|c| {
                                let p = std::path::PathBuf::from(&c.log_path);
                                let t = std::fs::metadata(&p).and_then(|m| m.modified()).ok()?;
                                Some((t, p))
                            })
                            .max_by_key(|(t, _)| *t)
                            .map(|(_, p)| p);
                        ui.horizontal(|ui| {
                            ui.add(
                                egui::TextEdit::singleline(&mut self.log_search)
                                    .hint_text("e.g. Critical, stunned, resisted")
                                    .desired_width(220.0),
                            );
                            if ui.button("Search").clicked() {
                                if let Some(log) = &newest_log {
                                    self.log_results =
                                        logscan::search(log, self.log_search.trim(), 15);
                                    self.status = format!(
                                        "{} unique line(s) in {}",
                                        self.log_results.len(),
                                        log.file_name().unwrap_or_default().to_string_lossy()
                                    );
                                }
                            }
                            if ui
                                .button("Scan for message types")
                                .on_hover_text(
                                    "Every distinct non-chat message shape in the log, \
                                     most frequent first (numbers collapsed)",
                                )
                                .clicked()
                            {
                                if let Some(log) = &newest_log {
                                    self.log_templates = logscan::unique_templates(log, 60);
                                    self.status = format!(
                                        "{} message shapes found",
                                        self.log_templates.len()
                                    );
                                }
                            }
                        });
                        let mut seed: Option<String> = None;
                        if !self.log_results.is_empty() {
                            egui::ScrollArea::vertical()
                                .id_salt("log-search-results")
                                .max_height(140.0)
                                .show(ui, |ui| {
                                    for line in &self.log_results {
                                        if ui
                                            .selectable_label(
                                                false,
                                                egui::RichText::new(line).small().monospace(),
                                            )
                                            .clicked()
                                        {
                                            seed = Some(line.clone());
                                        }
                                    }
                                });
                        }
                        if !self.log_templates.is_empty() {
                            egui::ScrollArea::vertical()
                                .id_salt("log-templates")
                                .max_height(200.0)
                                .show(ui, |ui| {
                                    for (count, example) in &self.log_templates {
                                        if ui
                                            .selectable_label(
                                                false,
                                                egui::RichText::new(format!(
                                                    "{count:>5}×  {example}"
                                                ))
                                                .small()
                                                .monospace(),
                                            )
                                            .clicked()
                                        {
                                            seed = Some(example.clone());
                                        }
                                    }
                                });
                        }
                        if let Some(line) = seed {
                            self.builder_line = line;
                            self.builder_chosen.clear();
                            self.builder_wild.clear();
                            self.status = "line loaded into the builder".into();
                        }
                    });
                egui::CollapsingHeader::new("Message shapes the parser knows")
                    .default_open(false)
                    .show(ui, |ui| {
                        ui.label(
                            egui::RichText::new(
                                "Reference examples for common combat lines — note the \
                                 crit is a suffix on the hit line, not its own message. \
                                 Click one to load it into the builder.",
                            )
                            .weak()
                            .small(),
                        );
                        let mut seed: Option<String> = None;
                        for (label, example) in logscan::KNOWN_SHAPES {
                            ui.horizontal(|ui| {
                                ui.label(
                                    egui::RichText::new(*label).weak().small(),
                                );
                                if ui
                                    .selectable_label(
                                        false,
                                        egui::RichText::new(*example).small().monospace(),
                                    )
                                    .clicked()
                                {
                                    seed = Some(example.to_string());
                                }
                            });
                        }
                        if let Some(line) = seed {
                            self.builder_line = line;
                            self.builder_chosen.clear();
                            self.builder_wild.clear();
                            // picking an example means "build from this"
                            self.pattern_manual = false;
                            self.status = "example loaded into the builder".into();
                        }
                    });
                ui.add_space(4.0);

                match self.edit_index {
                    None => {
                        ui.label(
                            egui::RichText::new("Build a trigger from a log line").strong(),
                        );
                    }
                    Some(_) => {
                        ui.label(
                            egui::RichText::new(format!(
                                "Editing \u{201c}{}\u{201d}",
                                self.new_name
                            ))
                            .strong()
                            .color(egui::Color32::from_rgb(120, 190, 250)),
                        );
                        ui.label(
                            egui::RichText::new(
                                "Edit the pattern below, or paste a line and click \
                                 Try it to check it. Clicking a word rebuilds the \
                                 pattern from that line and replaces what was loaded.",
                            )
                            .small()
                            .weak()
                            .italics(),
                        );
                    }
                }
                if ui
                    .add(
                        egui::TextEdit::singleline(&mut self.builder_line)
                            .desired_width(f32::INFINITY)
                            .hint_text("paste a line from your log"),
                    )
                    .changed()
                {
                    self.builder_chosen.clear();
                            self.builder_wild.clear();
                }

                // A regex pasted into the line box gets escaped into a pattern
                // that matches literal backslashes — it looks plausible and
                // never fires. Say so, and offer the one-click repair.
                if alerts::builder::looks_like_regex(&self.builder_line) {
                    ui.horizontal_wrapped(|ui| {
                        ui.label(
                            egui::RichText::new(
                                "\u{26a0} that looks like a regex, not a log line \u{2014} \
                                 pasted here it would be matched literally and never fire.",
                            )
                            .small()
                            .color(egui::Color32::from_rgb(235, 170, 70)),
                        );
                        if ui
                            .small_button("Use as pattern")
                            .on_hover_text("Move it to the pattern field below, unescaped")
                            .clicked()
                        {
                            self.builder_pattern = self.builder_line.trim().to_string();
                            self.pattern_manual = true;
                            self.builder_line.clear();
                            self.builder_chosen.clear();
                            self.builder_wild.clear();
                            self.status = "pattern set from the pasted regex".into();
                        }
                    });
                }

                let stripped = alerts::builder::strip_timestamp(&self.builder_line).to_string();
                if !stripped.is_empty() {
                    let tokens = alerts::builder::tokenize(&stripped);
                    // While editing an existing trigger, a pasted line is a
                    // line to TEST against, not a line to rebuild from — the
                    // pattern came from the file. Only an actual word click
                    // says "regenerate over what I loaded".
                    let mut word_click = false;
                    ui.horizontal_wrapped(|ui| {
                        ui.label(
                            egui::RichText::new(
                                "click cycles: capture {n} → any (*) → literal:",
                            )
                            .weak()
                            .small(),
                        )
                        .on_hover_text(
                            "capture {n}: the word varies and you want to use it \
                             (speak it, compare it).\n\
                             any (*): the word varies but you don't care — mob \
                             names and damage numbers usually. Neighbouring * \
                             words merge, so 'a rat' and 'a greater skeleton' \
                             both match.\n\
                             literal: must appear exactly.",
                        );
                        for (i, tok) in tokens.iter().enumerate() {
                            if !alerts::builder::is_word(tok) {
                                ui.label(egui::RichText::new(tok).weak().monospace());
                                continue;
                            }
                            let chosen = self.builder_chosen.contains(&i);
                            let wild = self.builder_wild.contains(&i);
                            let label = if wild {
                                egui::RichText::new(format!("{tok} *"))
                                    .weak()
                                    .strikethrough()
                            } else {
                                match alerts::builder::group_of(&self.builder_chosen, i) {
                                    Some(n) => egui::RichText::new(format!("{tok} {{{n}}}")),
                                    None => egui::RichText::new(tok.clone()),
                                }
                            };
                            if ui.selectable_label(chosen || wild, label).clicked() {
                                word_click = true;
                                if chosen {
                                    self.builder_chosen.remove(&i);
                                    self.builder_wild.insert(i);
                                } else if wild {
                                    self.builder_wild.remove(&i);
                                } else {
                                    self.builder_chosen.insert(i);
                                }
                            }
                        }
                    });

                    // The generated pattern is a starting point, not gospel:
                    // the field is editable, so "any line ending (Critical)"
                    // is one edit away from the over-literal auto version.
                    // Clicking words regenerates; manual edits stick until
                    // the next click.
                    let auto = alerts::builder::regex(&tokens, &self.builder_chosen, &self.builder_wild);
                    if auto != self.builder_pattern_auto {
                        self.builder_pattern_auto = auto.clone();
                        // A word click is an explicit "rebuild from this
                        // line" and outranks anything typed or loaded.
                        if !self.pattern_manual || word_click {
                            self.builder_pattern = auto;
                        }
                    }
                    if word_click {
                        self.pattern_manual = false;
                    }
                    ui.horizontal(|ui| {
                        ui.label(egui::RichText::new("pattern").weak().small());
                        if ui
                            .add(
                                egui::TextEdit::singleline(&mut self.builder_pattern)
                                    .font(egui::TextStyle::Monospace)
                                    .desired_width(340.0),
                            )
                            .changed()
                        {
                            // typed here by hand: keep it until a word click
                            self.pattern_manual = true;
                        }
                        if ui.small_button("copy").clicked() {
                            ui.output_mut(|o| o.copied_text = self.builder_pattern.clone());
                            self.status = "pattern copied".into();
                        }
                    });

                    egui::Grid::new("newtrig").num_columns(2).show(ui, |ui| {
                        ui.label("name");
                        ui.add(
                            egui::TextEdit::singleline(&mut self.new_name)
                                .desired_width(240.0)
                                .hint_text("what this alerts you to"),
                        );
                        ui.end_row();
                        ui.label("play");
                        let labels = alerts::labels(&self.reg.settings.sound_package);
                        egui::ComboBox::from_id_salt("new_sound")
                            .selected_text(if self.new_sound.is_empty() {
                                "nothing".to_string()
                            } else {
                                self.new_sound.clone()
                            })
                            .show_ui(ui, |ui| {
                                ui.selectable_value(&mut self.new_sound, String::new(), "nothing");
                                for l in &labels {
                                    ui.selectable_value(&mut self.new_sound, l.clone(), l);
                                }
                            });
                        ui.end_row();
                        ui.label("say");
                        ui.add(
                            egui::TextEdit::singleline(&mut self.new_say)
                                .desired_width(240.0)
                                .hint_text("spoken text, e.g. {1} merged an item to plus {2}"),
                        );
                        ui.end_row();
                        ui.label("show");
                        ui.add(
                            egui::TextEdit::singleline(&mut self.new_show)
                                .desired_width(240.0)
                                .hint_text("on-screen text — needs the Messages overlay"),
                        )
                        .on_hover_text(
                            "Announced in big text over the game, then kept in the \
                             message overlay's list. Same {1} placeholders as the \
                             spoken text.",
                        );
                        ui.end_row();

                        // The captures, spelled out and clickable. That the
                        // picked words come back as {1}, {2} is the one bit of
                        // the format nobody guesses.
                        if !self.builder_chosen.is_empty() {
                            ui.label("insert");
                            ui.horizontal_wrapped(|ui| {
                                for (n, idx) in self.builder_chosen.iter().enumerate() {
                                    let word = tokens.get(*idx).cloned().unwrap_or_default();
                                    let group = n + 1;
                                    if ui
                                        .small_button(format!("{{{group}}} = {word}"))
                                        .on_hover_text("Add this capture to the spoken text")
                                        .clicked()
                                    {
                                        if !self.new_say.is_empty() && !self.new_say.ends_with(' ')
                                        {
                                            self.new_say.push(' ');
                                        }
                                        self.new_say.push_str(&format!("{{{group}}}"));
                                    }
                                }
                            });
                            ui.end_row();
                        }
                    });

                    ui.horizontal(|ui| {
                        let ready = !self.new_name.trim().is_empty()
                            && (!self.new_sound.is_empty()
                                || !self.new_say.trim().is_empty()
                                || !self.new_show.trim().is_empty());
                        let save_label = match self.edit_index {
                            Some(_) => "Save changes",
                            None => "Create trigger",
                        };
                        if ui
                            .add_enabled(ready, egui::Button::new(save_label))
                            .on_hover_text("Writes it to triggers.toml and starts matching now")
                            .on_disabled_hover_text("Needs a name, and a sound, something to say, or something to show")
                            .clicked()
                        {
                            match self.edit_index {
                                Some(i) => {
                                    self.alerts.update_trigger(
                                        i,
                                        self.new_name.trim(),
                                        self.builder_pattern.trim(),
                                        &self.new_sound,
                                        self.new_say.trim(),
                                        self.new_show.trim(),
                                    );
                                    self.status =
                                        format!("saved \"{}\"", self.new_name.trim());
                                }
                                None => {
                                    self.alerts.add_trigger(
                                        self.new_name.trim(),
                                        self.builder_pattern.trim(),
                                        &self.new_sound,
                                        self.new_say.trim(),
                                        self.new_show.trim(),
                                    );
                                    self.status = format!("added \"{}\"", self.new_name.trim());
                                }
                            }
                            self.edit_index = None;
                            self.pattern_manual = false;
                            self.new_name.clear();
                            self.new_say.clear();
                            self.new_show.clear();
                            self.builder_chosen.clear();
                            self.builder_wild.clear();
                            self.builder_line.clear();
                            self.builder_pattern.clear();
                            self.builder_pattern_auto.clear();
                        }
                        let clear_label = match self.edit_index {
                            Some(_) => "Cancel edit",
                            None => "Clear",
                        };
                        if ui
                            .button(clear_label)
                            .on_hover_text("Reset the builder — line, picks, pattern, name, say")
                            .clicked()
                        {
                            let was_editing = self.edit_index.is_some();
                            self.edit_index = None;
                            self.pattern_manual = false;
                            self.builder_line.clear();
                            self.builder_chosen.clear();
                            self.builder_wild.clear();
                            self.builder_pattern.clear();
                            self.builder_pattern_auto.clear();
                            self.new_name.clear();
                            self.new_say.clear();
                            self.new_show.clear();
                            self.status = if was_editing {
                                "edit cancelled — trigger left as it was".into()
                            } else {
                                "builder cleared".into()
                            };
                        }
                        if ui
                            .button("Try it")
                            .on_hover_text(
                                "Test the DRAFT above against this line — plays the \
                                 sound and speaks the text with captures filled in, \
                                 without saving anything",
                            )
                            .clicked()
                        {
                            let line =
                                alerts::builder::strip_timestamp(&self.builder_line).to_string();
                            match regex::Regex::new(&self.builder_pattern) {
                                Err(e) => {
                                    self.status = format!("pattern is not valid regex: {e}")
                                }
                                Ok(re) => match re.captures(&line) {
                                    None => {
                                        self.status =
                                            "pattern does NOT match this line".into();
                                    }
                                    Some(caps) => {
                                        let mut said = self.new_say.trim().to_string();
                                        for i in 1..caps.len() {
                                            said = said.replace(
                                                &format!("{{{i}}}"),
                                                caps.get(i).map(|m| m.as_str()).unwrap_or(""),
                                            );
                                        }
                                        if !self.new_sound.is_empty() {
                                            alerts::play_forced(
                                                &self.new_sound,
                                                &self.reg.settings.sound_package,
                                            );
                                        }
                                        if !said.is_empty() {
                                            let voice = alerts::Voice::from_settings(
                                                &self.reg.settings.voice_engine,
                                                &self.reg.settings.piper_model,
                                            );
                                            alerts::speak_forced(
                                                &said,
                                                &froklog::triggers::engine::VoicePriority::Emergency,
                                                &voice,
                                            );
                                        }
                                        let capture_note = if caps.len() > 1 {
                                            let vals: Vec<String> = (1..caps.len())
                                                .map(|i| {
                                                    format!(
                                                        "{{{i}}}={}",
                                                        caps.get(i)
                                                            .map(|m| m.as_str())
                                                            .unwrap_or("")
                                                    )
                                                })
                                                .collect();
                                            format!(" — {}", vals.join(", "))
                                        } else {
                                            String::new()
                                        };
                                        self.status = format!("MATCH{capture_note}");
                                    }
                                },
                            }
                        }
                    });
                }

                // The triggers themselves: what each one watches for, and a
                // tick to silence one without deleting it.
                ui.separator();
                let mut toggle: Option<(usize, bool)> = None;
                let mut test: Option<(usize, String)> = None;
                let mut delete: Option<usize> = None;
                let mut edit: Option<usize> = None;
                if self.alerts.count() == 0 {
                    ui.label(
                        egui::RichText::new("No triggers yet \u{2014} Edit triggers\u{2026} to write one.")
                            .italics()
                            .weak(),
                    );
                }
                for (i, t) in self.alerts.triggers().iter().enumerate() {
                    ui.horizontal(|ui| {
                        let mut on = t.enabled;
                        if ui
                            .checkbox(&mut on, "")
                            .on_hover_text("Silence this trigger without deleting it")
                            .changed()
                        {
                            toggle = Some((i, on));
                        }
                        let name_txt = egui::RichText::new(&t.name).strong();
                        ui.label(if self.edit_index == Some(i) {
                            name_txt.color(egui::Color32::from_rgb(120, 190, 250))
                        } else {
                            name_txt
                        });
                        if self.edit_index == Some(i) {
                            ui.label(
                                egui::RichText::new("(editing above)")
                                    .small()
                                    .weak()
                                    .italics(),
                            );
                        }
                        if ui
                            .small_button("edit")
                            .on_hover_text(
                                "Load this trigger into the builder above so its \
                                 pattern, sound and speech can be changed and saved \
                                 back over it",
                            )
                            .clicked()
                        {
                            edit = Some(i);
                        }
                        if ui
                            .small_button("test")
                            .on_hover_text(
                                "Play this trigger's own sound and speech right now \
                                 (conditions skipped, captures spoken as 'something')",
                            )
                            .clicked()
                        {
                            test = Some((i, t.name.clone()));
                        }
                        if self.trigger_delete_arm == Some(i) {
                            if ui
                                .small_button(
                                    egui::RichText::new("really delete?")
                                        .color(egui::Color32::from_rgb(230, 90, 90)),
                                )
                                .clicked()
                            {
                                self.trigger_delete_arm = None;
                                delete = Some(i);
                            }
                            if ui.small_button("cancel").clicked() {
                                self.trigger_delete_arm = None;
                            }
                        } else if ui
                            .small_button(egui::RichText::new("×").weak())
                            .on_hover_text(
                                "Delete this trigger from triggers.toml. Asks once \
                                 more. To silence it temporarily, untick it instead.",
                            )
                            .clicked()
                        {
                            self.trigger_delete_arm = Some(i);
                        }
                    });
                    ui.horizontal(|ui| {
                        ui.add_space(24.0);
                        ui.label(
                            egui::RichText::new(alerts::describe(t))
                                .small()
                                .weak()
                                .monospace(),
                        );
                    });
                }
                if let Some((i, on)) = toggle {
                    self.alerts.set_enabled(i, on);
                    self.status = if on { "trigger on".into() } else { "trigger off".into() };
                }
                if let Some((i, name)) = test {
                    let voice = alerts::Voice::from_settings(
                        &self.reg.settings.voice_engine,
                        &self.reg.settings.piper_model,
                    );
                    let shown = self.alerts.fire_trigger_actions(
                        i,
                        &self.reg.settings.sound_package,
                        &voice,
                    );
                    for m in shown {
                        self.announce(m);
                    }
                    self.status = format!("fired \"{name}\"'s actions");
                }
                if let Some(i) = delete {
                    let name = self
                        .alerts
                        .triggers()
                        .get(i)
                        .map(|t| t.name.clone())
                        .unwrap_or_default();
                    self.alerts.delete_trigger(i);
                    self.status = format!("deleted \"{name}\"");
                    if self.edit_index == Some(i) {
                        self.edit_index = None;
                    }
                }
                if let Some(i) = edit {
                    match self.alerts.parts(i) {
                        Some(p) => {
                            let name = p.name.clone();
                            self.edit_index = Some(i);
                            self.new_name = p.name;
                            self.new_sound = p.sound;
                            self.new_say = p.say;
                            self.new_show = p.show;
                            // The word picker starts empty: this pattern came
                            // from the file, not from a line, and letting the
                            // generator run would overwrite it on the next
                            // frame. Parking `_auto` on the empty-line result
                            // keeps it quiet until a word is actually clicked.
                            self.builder_line.clear();
                            self.builder_chosen.clear();
                            self.builder_wild.clear();
                            self.builder_pattern_auto = alerts::builder::regex(
                                &[],
                                &self.builder_chosen,
                                &self.builder_wild,
                            );
                            self.builder_pattern = p.pattern;
                            self.pattern_manual = true;
                            self.status = format!("editing \"{name}\" — Save changes when done");
                        }
                        None => {
                            self.status = "that trigger is richer than the builder \
                                           (several conditions or actions) — use \
                                           Edit triggers\u{2026} instead"
                                .into();
                        }
                    }
                }

                if !self.alerts.recent.is_empty() {
                    ui.separator();
                    for f in self.alerts.recent.iter().rev().take(6) {
                        ui.horizontal(|ui| {
                            let mark = match (f.played.is_some(), f.spoke) {
                                (true, true) => "♪+",
                                (true, false) => "♪",
                                (false, true) => "+",
                                _ => "·",
                            };
                            ui.label(egui::RichText::new(mark).weak().small());
                            ui.label(egui::RichText::new(&f.message).small());
                        });
                    }
                }
                }
                Tab::Sounds => {
                    ui.label(
                        egui::RichText::new(
                            "Triggers ask for a sound by label. A package decides which \
                             file each label is, so switching package re-themes every \
                             trigger at once.",
                        )
                        .italics()
                        .weak(),
                    );
                    ui.add_space(6.0);
                    egui::Grid::new("sounds").num_columns(2).show(ui, |ui| {
                    ui.label("volume");
                    ui.horizontal(|ui| {
                        let mut v = self.reg.settings.sound_volume;
                        if ui
                            .add(egui::Slider::new(&mut v, 0..=100).suffix("%"))
                            .changed()
                        {
                            self.reg.settings.sound_volume = v;
                            dirty = true;
                        }
                        let mut m = self.reg.settings.sound_muted;
                        if ui
                            .checkbox(&mut m, "mute")
                            .on_hover_text(
                                "Silence alerts and voice. The audition buttons \
                                 still play — that is what auditioning is for.",
                            )
                            .changed()
                        {
                            self.reg.settings.sound_muted = m;
                            dirty = true;
                        }
                    });
                    ui.end_row();
                    ui.label("sound package");
                    // one name re-themes every trigger's sound, which is
                    // what packages are for
                    let packages = alerts::packages();
                    if packages.is_empty() {
                        ui.label(
                            egui::RichText::new("none in ~/.local/share/froklog/sounds")
                                .weak()
                                .small(),
                        );
                    } else {
                        egui::ComboBox::from_id_salt("sound_package")
                            .selected_text(self.reg.settings.sound_package.clone())
                            .show_ui(ui, |ui| {
                                for p in &packages {
                                    dirty |= ui
                                        .selectable_value(
                                            &mut self.reg.settings.sound_package,
                                            p.clone(),
                                            p,
                                        )
                                        .changed();
                                }
                            });
                    }
                    ui.end_row();
                    });
                    ui.add_space(4.0);

                    // Package management — everything the Windows client's
                    // Sounds tab does, minus file dialogs (COSMIC's portal
                    // crashes on them): Import takes a typed path instead.
                    {
                        use froklog::sound_packages::sound_packages as sp;
                        let active = self.reg.settings.sound_package.clone();
                        ui.horizontal(|ui| {
                            if ui
                                .button("New")
                                .on_hover_text("Clone the active package under a new name")
                                .clicked()
                            {
                                let name = sp::unique_package_name(&format!("{active} copy"));
                                self.status = match sp::clone_package(&active, &name) {
                                    Ok(()) => {
                                        self.reg.settings.sound_package = name.clone();
                                        dirty = true;
                                        format!("created package {name}")
                                    }
                                    Err(e) => format!("clone failed: {e}"),
                                };
                            }
                            if ui
                                .button("Export")
                                .on_hover_text("Write the active package to a zip in ~/Downloads")
                                .clicked()
                            {
                                let dest = dirs::download_dir()
                                    .unwrap_or_else(|| dirs::home_dir().unwrap_or_default())
                                    .join(format!("froklog-sounds-{active}.zip"));
                                self.status = match sp::export_package_zip(&active, &dest) {
                                    Ok(()) => format!("exported to {}", dest.display()),
                                    Err(e) => format!("export failed: {e}"),
                                };
                            }
                            if active != "default" {
                                if self.pkg_delete_arm {
                                    if ui
                                        .button(
                                            egui::RichText::new("Really delete package?")
                                                .color(egui::Color32::from_rgb(230, 90, 90)),
                                        )
                                        .clicked()
                                    {
                                        self.pkg_delete_arm = false;
                                        self.status = match sp::delete_package(&active) {
                                            Ok(()) => {
                                                self.reg.settings.sound_package =
                                                    "default".into();
                                                dirty = true;
                                                format!("deleted package {active}")
                                            }
                                            Err(e) => format!("delete failed: {e}"),
                                        };
                                    }
                                    if ui.small_button("cancel").clicked() {
                                        self.pkg_delete_arm = false;
                                    }
                                } else if ui
                                    .button("Delete")
                                    .on_hover_text(
                                        "Remove this package and its sounds. Asks once more. \
                                         The default package cannot be deleted.",
                                    )
                                    .clicked()
                                {
                                    self.pkg_delete_arm = true;
                                }
                            }
                        });
                        ui.horizontal(|ui| {
                            ui.label(egui::RichText::new("import zip").weak().small());
                            ui.add(
                                egui::TextEdit::singleline(&mut self.import_zip)
                                    .hint_text("/path/to/package.zip")
                                    .desired_width(260.0),
                            );
                            if ui.button("Import").clicked() {
                                let path = self.import_zip.trim().to_string();
                                self.status = match sp::import_package_zip(std::path::Path::new(
                                    &path,
                                )) {
                                    Ok(name) => {
                                        self.reg.settings.sound_package = name.clone();
                                        dirty = true;
                                        self.import_zip.clear();
                                        format!("imported package {name}")
                                    }
                                    Err(e) => format!("import failed: {e}"),
                                };
                            }
                        });
                        ui.horizontal(|ui| {
                            ui.label(egui::RichText::new("add sound").weak().small());
                            ui.add(
                                egui::TextEdit::singleline(&mut self.new_label_name)
                                    .hint_text("label")
                                    .desired_width(90.0),
                            );
                            ui.add(
                                egui::TextEdit::singleline(&mut self.new_label_file)
                                    .hint_text("/path/to/sound.wav")
                                    .desired_width(220.0),
                            );
                            if ui.button("Add").clicked() {
                                let name = self.new_label_name.trim().to_string();
                                let file = self.new_label_file.trim().to_string();
                                if name.is_empty() || file.is_empty() {
                                    self.status = "give the sound a label and a file".into();
                                } else {
                                    self.status = match sp::add_or_replace_label(
                                        &active,
                                        &name,
                                        std::path::Path::new(&file),
                                    ) {
                                        Ok(()) => {
                                            self.new_label_name.clear();
                                            self.new_label_file.clear();
                                            format!("added {name} to {active}")
                                        }
                                        Err(e) => format!("add failed: {e}"),
                                    };
                                }
                            }
                        });
                    }

                    ui.add_space(6.0);
                // Every sound the active package offers, playable on the spot.
                let labels = alerts::labels(&self.reg.settings.sound_package);
                if !labels.is_empty() {
                    ui.horizontal_wrapped(|ui| {
                        ui.label(egui::RichText::new("sounds").weak().small());
                        for label in &labels {
                            if ui.small_button(label).clicked() {
                                self.status = match alerts::play_forced(
                                    label,
                                    &self.reg.settings.sound_package,
                                ) {
                                    Some(p) => format!("played {p}"),
                                    None => format!("{label}: no file in this package"),
                                };
                            }
                            if ui
                                .small_button(egui::RichText::new("×").weak())
                                .on_hover_text(
                                    "Remove this label from the package (the sound \
                                     file itself is left on disk)",
                                )
                                .clicked()
                            {
                                froklog::sound_packages::sound_packages::delete_label(
                                    &self.reg.settings.sound_package,
                                    label,
                                );
                                self.status = format!("removed {label}");
                            }
                        }
                    });
                }
                    ui.add_space(6.0);
                    // the path rather than a file manager: opening one goes
                    // through the desktop portal, which COSMIC currently
                    // crashes on
                    let dir = dirs::data_dir()
                        .unwrap_or_default()
                        .join("froklog")
                        .join("sounds");
                    ui.horizontal(|ui| {
                        ui.label(
                            egui::RichText::new(dir.display().to_string())
                                .small()
                                .weak()
                                .monospace(),
                        );
                        if ui
                            .small_button("copy")
                            .on_hover_text(
                                "Add a .wav in a package folder and list it in its package.toml.",
                            )
                            .clicked()
                        {
                            ui.output_mut(|o| o.copied_text = dir.display().to_string());
                            self.status = "path copied".into();
                        }
                    });
                }
                Tab::Speech => {
                    ui.label(
                        egui::RichText::new(
                            "Neither voice has met Norrath. Spell a word the way it should \
                             sound and every alert says it that way \u{2014} whole words only, \
                             case-insensitive.",
                        )
                        .italics()
                        .weak(),
                    );
                    ui.add_space(6.0);

                    let mut remove: Option<usize> = None;
                    let mut say: Option<String> = None;
                    egui::ScrollArea::vertical()
                        .max_height(260.0)
                        .show(ui, |ui| {
                            egui::Grid::new("pron").num_columns(4).show(ui, |ui| {
                                ui.label(egui::RichText::new("word").strong().small());
                                ui.label(egui::RichText::new("say it like").strong().small());
                                ui.end_row();
                                for i in 0..self.pron.len() {
                                    ui.add(
                                        egui::TextEdit::singleline(&mut self.pron[i].0)
                                            .desired_width(150.0),
                                    );
                                    ui.add(
                                        egui::TextEdit::singleline(&mut self.pron[i].1)
                                            .desired_width(200.0),
                                    );
                                    if ui.small_button("say").clicked() {
                                        say = Some(self.pron[i].1.clone());
                                    }
                                    if ui.small_button("\u{00d7}").on_hover_text("remove").clicked() {
                                        remove = Some(i);
                                    }
                                    ui.end_row();
                                }
                            });
                        });
                    if let Some(i) = remove {
                        self.pron.remove(i);
                    }

                    ui.horizontal(|ui| {
                        if ui.button("Add word").clicked() {
                            self.pron.push((String::new(), String::new()));
                        }
                        if ui.button("Save").clicked() {
                            self.status = match alerts::save_pronunciations(&self.pron) {
                                Ok(()) => format!("{} pronunciations saved", self.pron.len()),
                                Err(e) => format!("could not save: {e}"),
                            };
                        }
                        if ui.button("Reload").clicked() {
                            self.pron = alerts::pronunciations();
                            self.status = format!("{} pronunciations loaded", self.pron.len());
                        }
                    });

                    // What a voice will actually be handed, after the table is
                    // applied — the quickest way to see an entry working.
                    ui.add_space(6.0);
                    ui.horizontal(|ui| {
                        ui.add(
                            egui::TextEdit::singleline(&mut self.phrase)
                                .desired_width(280.0)
                                .hint_text("a line to check"),
                        );
                        if ui.button("Say it").clicked() {
                            say = Some(self.phrase.clone());
                        }
                    });
                    ui.label(
                        egui::RichText::new(alerts::preview(&self.phrase))
                            .small()
                            .weak()
                            .monospace(),
                    );

                    if let Some(text) = say {
                        let voice = alerts::Voice::from_settings(
                            &self.reg.settings.voice_engine,
                            &self.reg.settings.piper_model,
                        );
                        alerts::speak_forced(
                            &text,
                            &froklog::triggers::engine::VoicePriority::Emergency,
                            &voice,
                        );
                    }
                }
                Tab::Meter => {
                    ui.label(
                        egui::RichText::new(
                            "An on-screen damage meter over the game, fed by the same \
                             parser that streams to froklog. Rendered as a compositor \
                             overlay, so it stays above a fullscreen game.",
                        )
                        .italics()
                        .weak(),
                    );
                    ui.add_space(6.0);

                    let s = &mut self.reg.settings;
                    let mut meter_dirty = false;
                    let mut style_dirty = false;
                    let mut resize = false;

                    if ui
                        .checkbox(&mut s.meter_enabled, "Show the DPS meter")
                        .changed()
                    {
                        meter_dirty = true;
                    }
                    let lock_resp = ui.checkbox(
                        &mut s.meter_locked,
                        "Locked — click-through: the mouse goes to the game, not the meter",
                    );
                    if lock_resp.changed() {
                        meter_dirty = true;
                        style_dirty = true;
                    }
                    lock_resp.on_hover_text(
                        "While locked the meter cannot be dragged or clicked at all. \
                         Unlock it here when you want to move it or use its buttons.",
                    );

                    ui.add_space(6.0);
                    // The picker exists because nothing else can move a layer
                    // surface between monitors: it is bound to one output for
                    // life and invisible to the compositor's window tools.
                    let monitors = match &self.monitors {
                        Some(m) => m.clone(),
                        None => {
                            let m = outputs::list();
                            self.monitors = Some(m.clone());
                            m
                        }
                    };
                    let mut respawn_for_output = false;
                    egui::Grid::new("meter-settings").num_columns(2).show(ui, |ui| {
                        ui.label("monitor");
                        {
                            let current = if s.meter_output.is_empty() {
                                "(focused monitor)".to_string()
                            } else {
                                s.meter_output.clone()
                            };
                            egui::ComboBox::from_id_salt("meter-output")
                                .selected_text(current)
                                .show_ui(ui, |ui| {
                                    if ui
                                        .selectable_label(s.meter_output.is_empty(), "(focused monitor)")
                                        .clicked()
                                        && !s.meter_output.is_empty()
                                    {
                                        s.meter_output.clear();
                                        meter_dirty = true;
                                        respawn_for_output = true;
                                    }
                                    for (name, desc) in &monitors {
                                        let label = if desc.is_empty() {
                                            name.clone()
                                        } else {
                                            format!("{name} — {desc}")
                                        };
                                        if ui
                                            .selectable_label(s.meter_output == *name, label)
                                            .clicked()
                                            && s.meter_output != *name
                                        {
                                            s.meter_output = name.clone();
                                            meter_dirty = true;
                                            respawn_for_output = true;
                                        }
                                    }
                                });
                        }
                        ui.end_row();

                        ui.label("position");
                        ui.horizontal(|ui| {
                            let mut x = s.meter_x;
                            let mut y = s.meter_y;
                            let rx = ui.add(egui::DragValue::new(&mut x).prefix("x "));
                            let ry = ui.add(egui::DragValue::new(&mut y).prefix("y "));
                            if rx.changed() || ry.changed() {
                                s.meter_x = x.max(0);
                                s.meter_y = y.max(0);
                                meter_dirty = true;
                                if let Some(o) = &self.overlay {
                                    let _ = o
                                        .tx
                                        .send(overlay::OverlayMsg::SetPosition(s.meter_x, s.meter_y));
                                }
                            }
                            if ui.button("Reset").clicked() {
                                s.meter_x = 40;
                                s.meter_y = 40;
                                meter_dirty = true;
                                if let Some(o) = &self.overlay {
                                    let _ = o
                                        .tx
                                        .send(overlay::OverlayMsg::SetPosition(s.meter_x, s.meter_y));
                                }
                            }
                        });
                        ui.end_row();

                        ui.label("width");
                        let mut w = s.meter_width;
                        if ui
                            .add(egui::Slider::new(&mut w, 220..=640).suffix(" px"))
                            .changed()
                        {
                            s.meter_width = w;
                            meter_dirty = true;
                            resize = true;
                        }
                        ui.end_row();

                        ui.label("max rows");
                        let mut rows = s.meter_max_rows;
                        if ui.add(egui::Slider::new(&mut rows, 1..=30)).changed() {
                            s.meter_max_rows = rows;
                            meter_dirty = true;
                            style_dirty = true;
                            resize = true; // surface height depends on it
                        }
                        ui.end_row();

                        ui.label("idle hide");
                        let mut idle = s.meter_idle_secs;
                        if ui
                            .add(
                                egui::Slider::new(&mut idle, 0..=120)
                                    .suffix(" s")
                                    .custom_formatter(|v, _| {
                                        if v == 0.0 {
                                            "never".into()
                                        } else {
                                            format!("{v:.0} s")
                                        }
                                    }),
                            )
                            .changed()
                        {
                            s.meter_idle_secs = idle;
                            meter_dirty = true;
                            style_dirty = true;
                        }
                        ui.end_row();

                        ui.label("font size");
                        let mut fsz = s.meter_font_size;
                        if ui
                            .add(egui::Slider::new(&mut fsz, 10.0..=24.0).suffix(" pt"))
                            .changed()
                        {
                            s.meter_font_size = fsz;
                            meter_dirty = true;
                            style_dirty = true;
                            resize = true;
                        }
                        ui.end_row();
                    });

                    ui.add_space(4.0);
                    ui.label(
                        egui::RichText::new(
                            "While this tab is open the meter stays visible for \
                             positioning. In play it appears when a fight starts and \
                             hides after the idle timeout — invisible is normal when \
                             nothing is being fought.",
                        )
                        .weak(),
                    );
                    if self.meter_x11 {
                        ui.add_space(4.0);
                        ui.label(
                            egui::RichText::new(
                                "Compositor overlay not available here — the meter runs \
                                 as a plain always-on-top window instead.",
                            )
                            .weak(),
                        );
                    }

                    if meter_dirty {
                        dirty = true;
                    }
                    if style_dirty {
                        self.push_overlay_settings();
                    }
                    if respawn_for_output {
                        // A layer surface cannot change outputs — controlled
                        // single respawn; reconcile brings it back on the
                        // newly chosen monitor.
                        if let Some(o) = self.overlay.take() {
                            let _ = o.tx.send(overlay::OverlayMsg::Quit);
                        }
                        self.status = "meter moving to the selected monitor…".into();
                    }
                    if resize {
                        // layer surfaces resize in place — never respawn here
                        let s = &self.reg.settings;
                        if let Some(o) = &self.overlay {
                            let _ = o.tx.send(overlay::OverlayMsg::SetSize(
                                s.meter_width,
                                overlay::surface_height(s.meter_max_rows, s.meter_font_size),
                            ));
                        }
                    }
                }
                Tab::Messages => {
                    ui.label(
                        egui::RichText::new(
                            "What a trigger SHOWS, as opposed to what it plays or says. \
                             A message flies in at full size, holds, then drops into a \
                             list of what has fired recently.",
                        )
                        .italics()
                        .weak(),
                    );
                    ui.add_space(6.0);

                    let s = &mut self.reg.settings;
                    let mut msg_dirty = false;
                    let mut style_dirty = false;

                    if ui
                        .checkbox(&mut s.msg_enabled, "Show the message overlay")
                        .changed()
                    {
                        msg_dirty = true;
                    }
                    let lock = ui.checkbox(
                        &mut s.msg_locked,
                        "Locked — click-through: the mouse goes to the game",
                    );
                    if lock.changed() {
                        msg_dirty = true;
                        style_dirty = true;
                    }
                    lock.on_hover_text(
                        "Unlike the meter this window has nothing to click, so lock it \
                         once it is where you want it.",
                    );

                    ui.add_space(6.0);
                    let monitors = match &self.monitors {
                        Some(m) => m.clone(),
                        None => {
                            let m = outputs::list();
                            self.monitors = Some(m.clone());
                            m
                        }
                    };
                    let mut respawn_for_output = false;
                    let mut resize = false;
                    egui::Grid::new("msg-settings").num_columns(2).show(ui, |ui| {
                        ui.label("monitor");
                        {
                            let current = if s.msg_output.is_empty() {
                                "(focused monitor)".to_string()
                            } else {
                                s.msg_output.clone()
                            };
                            egui::ComboBox::from_id_salt("msg-output")
                                .selected_text(current)
                                .show_ui(ui, |ui| {
                                    if ui
                                        .selectable_label(
                                            s.msg_output.is_empty(),
                                            "(focused monitor)",
                                        )
                                        .clicked()
                                        && !s.msg_output.is_empty()
                                    {
                                        s.msg_output.clear();
                                        msg_dirty = true;
                                        respawn_for_output = true;
                                    }
                                    for (name, desc) in &monitors {
                                        let label = if desc.is_empty() {
                                            name.clone()
                                        } else {
                                            format!("{name} — {desc}")
                                        };
                                        if ui
                                            .selectable_label(&s.msg_output == name, label)
                                            .clicked()
                                            && &s.msg_output != name
                                        {
                                            s.msg_output = name.clone();
                                            msg_dirty = true;
                                            respawn_for_output = true;
                                        }
                                    }
                                });
                        }
                        ui.end_row();

                        ui.label("position");
                        ui.horizontal(|ui| {
                            let (mut x, mut y) = (s.msg_x, s.msg_y);
                            let rx = ui.add(egui::DragValue::new(&mut x).prefix("x "));
                            let ry = ui.add(egui::DragValue::new(&mut y).prefix("y "));
                            if rx.changed() || ry.changed() {
                                s.msg_x = x.max(0);
                                s.msg_y = y.max(0);
                                msg_dirty = true;
                                if let Some(o) = &self.msg_overlay {
                                    let _ = o
                                        .tx
                                        .send(overlay::OverlayMsg::SetPosition(s.msg_x, s.msg_y));
                                }
                            }
                        });
                        ui.end_row();

                        ui.label("width");
                        let mut w = s.msg_width;
                        if ui
                            .add(egui::Slider::new(&mut w, 260..=900).suffix(" px"))
                            .changed()
                        {
                            s.msg_width = w;
                            msg_dirty = true;
                            resize = true;
                        }
                        ui.end_row();

                        ui.label("announce size");
                        let mut peak = s.msg_peak_size;
                        if ui
                            .add(egui::Slider::new(&mut peak, 16.0..=72.0).suffix(" pt"))
                            .changed()
                        {
                            s.msg_peak_size = peak;
                            msg_dirty = true;
                            style_dirty = true;
                        }
                        ui.end_row();

                        ui.label("hold");
                        let mut hold = s.msg_hold_secs;
                        if ui
                            .add(egui::Slider::new(&mut hold, 0.5..=8.0).suffix(" s"))
                            .changed()
                        {
                            s.msg_hold_secs = hold;
                            msg_dirty = true;
                            style_dirty = true;
                        }
                        ui.end_row();

                        ui.label("history rows");
                        let mut rows = s.msg_history_rows;
                        if ui.add(egui::Slider::new(&mut rows, 1..=20)).changed() {
                            s.msg_history_rows = rows;
                            msg_dirty = true;
                            style_dirty = true;
                        }
                        ui.end_row();

                        ui.label("idle hide");
                        let mut idle = s.msg_idle_secs;
                        if ui
                            .add(
                                egui::Slider::new(&mut idle, 0..=120)
                                    .suffix(" s")
                                    .custom_formatter(|v, _| {
                                        if v == 0.0 {
                                            "never".into()
                                        } else {
                                            format!("{v:.0} s")
                                        }
                                    }),
                            )
                            .changed()
                        {
                            s.msg_idle_secs = idle;
                            msg_dirty = true;
                            style_dirty = true;
                        }
                        ui.end_row();
                    });

                    ui.add_space(6.0);
                    if ui
                        .button("Show a test message")
                        .on_hover_text("Sends one through the real overlay, so what you \
                                        see is what a trigger will look like")
                        .clicked()
                    {
                        let m = messages::Msg {
                            icon: "warn".into(),
                            color: String::new(),
                            text: "A greater skeleton hits you for 175".into(),
                            text_color: String::new(),
                            border_color: String::new(),
                            treatment: Default::default(),
                            priority: Default::default(),
                        };
                        self.announce(m);
                        self.status = "test message sent".into();
                    }

                    ui.add_space(4.0);
                    ui.label(
                        egui::RichText::new(
                            "A trigger contributes a message here by having a \"show\" \
                             text — set that on the Triggers tab. While this tab is \
                             open the window stays visible for positioning; in play it \
                             appears when something fires and hides again afterwards.",
                        )
                        .weak(),
                    );
                    if self.msg_x11 {
                        ui.add_space(4.0);
                        ui.label(
                            egui::RichText::new(
                                "Compositor overlay not available here — the message \
                                 window cannot run. Sounds and speech still work.",
                            )
                            .weak(),
                        );
                    }

                    if msg_dirty {
                        dirty = true;
                    }
                    if style_dirty {
                        self.push_msg_settings();
                    }
                    if respawn_for_output {
                        if let Some(o) = self.msg_overlay.take() {
                            let _ = o.tx.send(overlay::OverlayMsg::Quit);
                        }
                        self.status = "message overlay moving to the selected monitor…".into();
                    }
                    if resize {
                        let w = self.reg.settings.msg_width;
                        let h = messages::surface_height(&self.msg_style());
                        if let Some(o) = &self.msg_overlay {
                            let _ = o.tx.send(overlay::OverlayMsg::SetSize(w, h));
                        }
                    }
                }
                Tab::Characters => {
                {
                    let rejects = froklog::pusher::BATCH_REJECTS.load(Ordering::Relaxed);
                    if rejects > 0 {
                        ui.label(
                            egui::RichText::new(format!(
                                "\u{26a0} The server has REJECTED {rejects} event \
                                 batch{} — those events are lost. This almost always \
                                 means the server is running an OLDER version than \
                                 this client: update the server, then restart this \
                                 client.",
                                if rejects == 1 { "" } else { "es" }
                            ))
                            .color(egui::Color32::from_rgb(240, 120, 100)),
                        );
                        ui.add_space(6.0);
                    }
                }
                ui.separator();
                ui.horizontal(|ui| {
                    ui.label(egui::RichText::new("Characters").strong());
                    ui.label(
                        egui::RichText::new("— register one to give it a stream, then tick to watch it")
                            .weak()
                            .small(),
                    );
                });

                let keys: Vec<String> = self.reg.characters.keys().cloned().collect();
                if keys.is_empty() {
                    ui.label(
                        egui::RichText::new("None yet — add a log folder above and scan.")
                            .italics()
                            .weak(),
                    );
                }

                let busy = self.busy.lock().unwrap().clone();
                // NB: the deferred-action slots are declared once above, outside
                // the tab match — redeclaring them here shadowed them, so every
                // click set a variable that went out of scope unread.

                egui::ScrollArea::vertical().show(ui, |ui| {
                    for key in keys {
                        let Some(ch) = self.reg.characters.get_mut(&key) else {
                            continue;
                        };
                        let registered = ch.registered();
                        ui.horizontal(|ui| {
                            // Watching never requires the server: unregistered
                            // characters run the same pipeline locally — meter,
                            // triggers, fight history — with nothing streamed.
                            let mut on = ch.enabled;
                            if ui
                                .checkbox(&mut on, "")
                                .on_hover_text(if registered {
                                    "Stream this character's log to froklog"
                                } else {
                                    "Watch locally — meter and triggers work, \
                                     nothing streams. Register to get a web view."
                                })
                                .changed()
                            {
                                ch.enabled = on;
                                dirty = true;
                            }
                            ui.label(egui::RichText::new(&ch.player).strong());
                            ui.label(egui::RichText::new(format!("· {}", ch.server)).weak());
                            if !registered && ch.enabled {
                                ui.label(
                                    egui::RichText::new("local")
                                        .small()
                                        .color(egui::Color32::from_rgb(120, 190, 250)),
                                )
                                .on_hover_text(
                                    "Parsed on this machine only — no stream, no web \
                                     page. Register to publish it.",
                                );
                            }

                            if !registered {
                                if busy.contains(&key) {
                                    ui.label(egui::RichText::new("registering…").weak());
                                } else if ui.button("Register").clicked() {
                                    to_register = Some(key.clone());
                                }
                            } else {
                                if ui.button("View").clicked() {
                                    open_url = ch.view_url(&self.reg.settings.server_url);
                                }
                                if ui
                                    .button("Copy link")
                                    .on_hover_text(
                                        "Copy the link to give someone else. Public characters \
                                         get a clean address; private ones a secret one.",
                                    )
                                    .clicked()
                                {
                                    if let Some(u) = ch
                                        .share_url(&self.reg.settings.server_url, &self.reg.settings.game)
                                    {
                                        ui.output_mut(|o| o.copied_text = u);
                                        to_copy = Some(key.clone());
                                    }
                                }
                                if ui.button("Plan").clicked() {
                                    open_url = Some(self.reg.settings.planner_url.clone());
                                }
                                // watching only sees what happens next; the log already
                                // holds everything before that
                                if self.imports.contains_key(&key) {
                                    ui.label(egui::RichText::new("importing…").weak());
                                } else if !ch.imported
                                    && ui
                                        .button("Import history")
                                        .on_hover_text(
                                            "Push this log's existing contents once. \
                                             Do this before you start watching.",
                                        )
                                        .clicked()
                                {
                                    to_import = Some(key.clone());
                                }
                                // A public page needs no secret link, and can be
                                // taken back; the token link cannot be un-shared.
                                if busy.contains(&key) {
                                    ui.label(egui::RichText::new("publishing…").weak());
                                } else {
                                    let mut on = ch.public;
                                    if ui
                                        .checkbox(&mut on, "public")
                                        .on_hover_text(
                                            "Give this character a page anyone can open, with no \
                                             secret link. Untick to take it down again.",
                                        )
                                        .changed()
                                    {
                                        to_publish = Some((key.clone(), on));
                                    }
                                }
                                // Retiring an old character: two clicks, because this
                                // erases the stream AND its whole server-side history.
                                if self.deleting.lock().unwrap().contains_key(&key) {
                                    ui.label(egui::RichText::new("deleting…").weak());
                                } else if self.delete_arm.as_deref() == Some(key.as_str()) {
                                    if ui
                                        .button(
                                            egui::RichText::new("Really erase history?")
                                                .color(egui::Color32::from_rgb(230, 90, 90)),
                                        )
                                        .on_hover_text(
                                            "Deletes this stream and its entire journal from \
                                             the server. The local log file is untouched.",
                                        )
                                        .clicked()
                                    {
                                        self.delete_arm = None;
                                        to_delete = Some(key.clone());
                                    }
                                    if ui.small_button("cancel").clicked() {
                                        self.delete_arm = None;
                                    }
                                } else if ui
                                    .button("Delete stream")
                                    .on_hover_text(
                                        "Remove this character's stream from the server, \
                                         including all its history. Asks once more.",
                                    )
                                    .clicked()
                                {
                                    self.delete_arm = Some(key.clone());
                                }
                            }
                        });

                        // The link itself, so it is obvious what "Copy link" gives out.
                        if let Some(ch) = self.reg.characters.get(&key) {
                            let s = &self.reg.settings;
                            if let Some(url) = ch.share_url(&s.server_url, &s.game) {
                                ui.horizontal(|ui| {
                                    ui.add_space(24.0);
                                    let shown = if ch.public {
                                        url.clone()
                                    } else {
                                        // the token is long and is the secret — show
                                        // that it exists, not what it is
                                        let head = url.split("?vtok=").next().unwrap_or(&url);
                                        format!("{head}?vtok=…")
                                    };
                                    ui.label(egui::RichText::new(shown).small().weak())
                                        .on_hover_text(if ch.public {
                                            "Anyone can open this."
                                        } else {
                                            "Private: only someone holding this exact link can open it."
                                        });
                                });
                            }
                        }

                        if let Some((_, sent)) = self.imports.get(&key) {
                            ui.horizontal(|ui| {
                                ui.add_space(24.0);
                                ui.label(
                                    egui::RichText::new(format!(
                                        "importing history — {} events",
                                        sent.load(Ordering::Relaxed)
                                    ))
                                    .small()
                                    .weak(),
                                );
                            });
                        }
                        if let Some(h) = self.running.get(&key) {
                            let sent = h.events_sent.load(Ordering::Relaxed);
                            let up = h.connected.load(Ordering::Relaxed);
                            let err = h.last_error.read().ok().and_then(|e| e.clone());
                            ui.horizontal(|ui| {
                                ui.add_space(24.0);
                                ui.label(
                                    egui::RichText::new(if h.local_only {
                                        "● watching (local)"
                                    } else if up {
                                        "● live"
                                    } else {
                                        "○ connecting"
                                    })
                                        .small()
                                        .color(if up {
                                            egui::Color32::from_rgb(120, 200, 160)
                                        } else {
                                            egui::Color32::GRAY
                                        }),
                                );
                                ui.label(
                                    egui::RichText::new(format!("{sent} events sent")).small().weak(),
                                );
                                if let Some(e) = err {
                                    ui.label(egui::RichText::new(e).small().color(egui::Color32::from_rgb(200, 120, 100)));
                                }
                            });
                        }
                    }
                });
                }
            }

            if let Some(k) = to_register {
                self.register(k);
            }
            if let Some(k) = to_import {
                self.import(k);
            }
            if let Some((k, want)) = to_publish {
                self.set_public(k, want);
            }
            if let Some(k) = to_delete {
                self.delete(k);
            }
            if let Some(k) = to_copy {
                self.status = format!("{k}: link copied");
            }
            if let Some(u) = open_url {
                let _ = open::that_detached(u);
            }
            if dirty {
                self.save();
                self.reconcile();
            }

            ui.separator();
            ui.label(egui::RichText::new(&self.status).small().weak());
        });

        // counters tick in the background; keep the numbers honest
        ctx.request_repaint_after(std::time::Duration::from_millis(750));
    }
}

fn main() -> eframe::Result<()> {
    let mut reg = Registry::load();
    // One household key per install, minted on first run: every stream this
    // client registers carries it, which is how the server knows the user's
    // characters are siblings (the viewer's "another character is live" hint).
    if reg.settings.owner_key.is_empty() {
        reg.settings.owner_key = registry::generate_owner_key();
        let _ = reg.save();
    }
    // The front-door secret: same generator, different job. Whoever holds it
    // can list every character this install streams, so it is not the
    // household key — that one is deliberately harmless on its own.
    if reg.settings.home_token.is_empty() {
        reg.settings.home_token = registry::generate_owner_key();
        let _ = reg.save();
    }
    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .expect("tokio runtime");

    // Backfill the key onto streams registered before it existed. Fire and
    // forget: idempotent PATCHes with the per-stream tokens.
    for ch in reg.characters.values().filter(|c| c.registered()) {
        let ch = ch.clone();
        let settings = reg.settings.clone();
        rt.spawn(async move {
            let _ = engine::backfill_owner_key(&settings, &ch).await;
        });
    }

    let (tx, rx) = crossbeam_channel::unbounded();
    let links = Arc::new(Mutex::new(Vec::new()));
    let watching = Arc::new(Mutex::new(0usize));
    let meter_on = Arc::new(Mutex::new(false));
    let connected_ok = Arc::new(Mutex::new(true));

    let tray = Tray {
        art: TrayArt::load(),
        tx,
        links: Arc::clone(&links),
        watching: Arc::clone(&watching),
        connected_ok: Arc::clone(&connected_ok),
        meter_on: Arc::clone(&meter_on),
    };
    // COSMIC hosts StatusNotifierItem, so this lands on the top bar
    ksni::TrayService::new(tray).spawn();

    // Before ANY event loop or GL context exists — see preflight_instance.
    // Only Wayland sessions use the layer-shell overlay; elsewhere the meter
    // is an egui viewport and needs no wgpu at all.
    let meter_instance = std::env::var_os("WAYLAND_DISPLAY")
        .is_some()
        .then(overlay::preflight_instance);

    let hidden = std::env::args().any(|a| a == "--hidden");
    let reg_configured = !reg.characters.is_empty();
    let reg_triggers_enabled = reg.settings.triggers_enabled;
    let (lines_tx, lines_rx) = crossbeam_channel::bounded::<String>(4096);
    let dirs_buf = reg.settings.log_dirs.join("\n");
    let mut app = App {
        reg,
        rt,
        running: BTreeMap::new(),
        status: "ready".into(),
        rx,
        links,
        watching,
        dirs_buf,
        pending: Arc::new(Mutex::new(BTreeMap::new())),
        busy: Arc::new(Mutex::new(Vec::new())),
        imports: BTreeMap::new(),
        publishing: Arc::new(Mutex::new(BTreeMap::new())),
        deleting: Arc::new(Mutex::new(BTreeMap::new())),
        delete_arm: None,
        alerts: alerts::Alerts::load(reg_triggers_enabled),
        restart_watching: false,
        // an unconfigured install has nothing to show on the other two tabs
        tab: if reg_configured {
            Tab::Characters
        } else {
            Tab::Server
        },
        phrase: "Zarri says, 'Hail, Icestorm'".into(),
        pron: alerts::pronunciations(),
        builder_line: String::new(),
        builder_chosen: Default::default(),
        builder_wild: Default::default(),
        builder_pattern: String::new(),
        builder_pattern_auto: String::new(),
        trigger_delete_arm: None,
        edit_index: None,
        pattern_manual: false,
        new_name: String::new(),
        new_sound: "Ding".into(),
        new_say: String::new(),
        new_show: String::new(),
        editing: false,
        trigger_text: String::new(),
        trigger_err: None,
        lines_tx,
        lines_rx,
        hide_pending: hidden,
        frames: 0,
        meter_instance,
        overlay: None,
        meter_x11: false,
        meter_view: meter_ui::MeterView::default(),
        meter_moved_at: None,
        meter_on,
        tray_connected_ok: connected_ok,
        meter_preview: false,
        msg_overlay: None,
        msg_x11: false,
        msg_preview: false,
        msg_moved_at: None,
        monitors: None,
        log_search: String::new(),
        log_results: Vec::new(),
        log_templates: Vec::new(),
        pkg_delete_arm: false,
        import_zip: String::new(),
        new_label_name: String::new(),
        new_label_file: String::new(),
    };
    app.sync_tray();
    app.reconcile(); // pick up whatever was already ticked

    // Close-to-tray only works under X11.
    //
    // Wayland has no concept of hiding a window — winit's set_visible is
    // literally a no-op there ("Not possible on Wayland"), so closing the
    // window would either do nothing or take the whole service down with it.
    // X11 can unmap a window, which is exactly what we want: it vanishes from
    // the dock and lives on as the tray icon. So when we are in a Wayland
    // session that offers XWayland, ask winit for the X11 backend. Without
    // DISPLAY we stay on Wayland and the window simply stays open.
    let force_x11 = std::env::var_os("WAYLAND_DISPLAY").is_some()
        && std::env::var("DISPLAY")
            .map(|d| !d.is_empty())
            .unwrap_or(false);

    eframe::run_native(
        "froklog watch",
        eframe::NativeOptions {
            viewport: egui::ViewportBuilder::default()
                .with_inner_size([560.0, 520.0])
                .with_title("froklog watch")
                .with_icon(window_icon())
                // --hidden is what the autostart entry passes: at login you
                // want the tray icon, not a window in your face. (Only X11
                // can honour this, which is the backend we ask for above.)
                .with_visible(!hidden),
            event_loop_builder: force_x11.then(|| {
                Box::new(|b: &mut eframe::EventLoopBuilder<eframe::UserEvent>| {
                    use winit::platform::x11::EventLoopBuilderExtX11;
                    b.with_x11();
                }) as eframe::EventLoopBuilderHook
            }),
            ..Default::default()
        },
        Box::new(|_cc| Ok(Box::new(app))),
    )
}
