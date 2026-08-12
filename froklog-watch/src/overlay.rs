//! Wayland layer-shell host for the DPS meter overlay.
//!
//! Renders `meter_ui` onto a `zwlr_layer_shell_v1` surface via layershellev +
//! egui_wgpu, on its own thread with its own Wayland connection (the main
//! window stays on eframe/X11 for the hide-to-tray workaround; the two do not
//! share an event loop). The overlay layer stacks above fullscreen windows —
//! including the game under XWayland — which a plain Wayland toplevel cannot
//! do at all. Compositors without layer-shell (GNOME) report a failed spawn
//! and the caller falls back to an X11 always-on-top viewport.
//!
//! Click-through: the surface's *input region* is what the compositor hit-
//! tests, so "locked" sets an empty region (all clicks reach the game) and
//! unlocked sets a region covering just the painted panel — the transparent
//! remainder of the surface never blocks game clicks either way. Keyboard
//! interactivity is `None` always: the game never loses keys to the meter.

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{mpsc as std_mpsc, Arc};
use std::time::{Duration, Instant};

use arc_swap::ArcSwap;
use froklog::state::CombatState;
use layershellev::reexport::wayland_client::{QueueHandle, WlCompositor};
use layershellev::reexport::{Anchor, KeyboardInteractivity, Layer};
use layershellev::{DispatchMessage, LayerShellEvent, ReturnData, WindowState};

use crate::meter_ui::{self, MeterAction, MeterStyle, MeterView};

/// One watched character's live combat feed, cloned out of `engine::Handle`.
#[derive(Clone)]
pub struct Feed {
    pub combat: Arc<ArcSwap<CombatState>>,
    pub reset: Arc<AtomicBool>,
}

/// Which overlay a surface is. Both are layer surfaces that render egui and
/// drag by their top strip; only the contents and the hide rule differ, so
/// they share this whole host rather than duplicating the parts that were
/// hard to get right (GPU init ordering, drag anchoring, input regions).
#[derive(Clone, Copy, PartialEq)]
pub enum Kind {
    Meter,
    Messages,
}

/// Messages from the main app into the overlay's event loop.
pub enum OverlayMsg {
    /// Render pacing — sent by the internal ticker thread.
    Tick,
    /// A trigger fired with something to announce (Messages surface).
    Announce(Box<crate::messages::Msg>),
    /// Look and pacing of the message overlay.
    SetMessageStyle(crate::messages::MessageStyle),
    /// The set of running characters changed.
    Feeds(Vec<Feed>),
    SetLocked(bool),
    SetStyle {
        max_rows: usize,
        font_size: f32,
        idle_secs: u64,
    },
    /// Move the surface (settings "reset position" or restored config).
    SetPosition(i32, i32),
    /// Live-resize the surface (width slider, row/font changes). Layer
    /// surfaces resize in place — never respawn for this: overlapping
    /// teardown/init cycles race and can take the process down.
    SetSize(u32, u32),
    /// While the Meter settings tab is open: force-render a placeholder even
    /// with no combat, so the meter can be seen, dragged, and sized.
    Preview(bool),
    Quit,
}

/// Events the overlay reports back for the main app to persist or act on.
pub enum OverlayEvent {
    /// The surface was dragged to a new top-left margin — persist it.
    Moved(i32, i32),
    /// Copy this text to the clipboard (the main window owns the clipboard).
    Copy(String),
    /// The gear icon: show the main window's meter settings.
    OpenSettings,
    /// The overlay thread ended. `Some(reason)` = abnormal (no layer-shell?);
    /// the caller should fall back to the X11 viewport path.
    Exited(Option<String>),
}

pub struct OverlayHandle {
    pub tx: layershellev::calloop::channel::Sender<OverlayMsg>,
    pub events: std_mpsc::Receiver<OverlayEvent>,
}

/// Create the Vulkan instance once, at startup, BEFORE any event loop
/// exists. Creating it later deadlocks inside the NVIDIA driver: once the
/// process holds both a GLX context (the main window) and a Wayland
/// connection (the overlay), vkCreateInstance never returns. Made early,
/// the instance works fine for surfaces created at any point after.
/// Vulkan-only on purpose — wgpu's GL backend probes EGL, which fights the
/// main window's glow context.
pub fn preflight_instance() -> Arc<egui_wgpu::wgpu::Instance> {
    use egui_wgpu::wgpu;
    Arc::new(wgpu::Instance::new(wgpu::InstanceDescriptor {
        backends: wgpu::Backends::VULKAN,
        ..Default::default()
    }))
}

pub struct OverlaySpawn {
    pub kind: Kind,
    pub instance: Arc<egui_wgpu::wgpu::Instance>,
    /// Compositor connector name to spawn on ("DP-1"); empty = focused
    /// monitor. Changing monitors is a respawn, not a runtime move — layer
    /// surfaces are bound to one output for life.
    pub output: String,
    pub feeds: Vec<Feed>,
    pub locked: bool,
    pub x: i32,
    pub y: i32,
    pub width: u32,
    pub max_rows: usize,
    pub font_size: f32,
    pub idle_secs: u64,
    /// Only read when `kind` is `Messages`.
    pub msg_style: crate::messages::MessageStyle,
}

/// Pick the feed to display: the character whose combat state saw a mob most
/// recently. With one enabled character (the usual case) this is just it.
fn pick_feed(feeds: &[Feed]) -> Option<Feed> {
    feeds
        .iter()
        .max_by_key(|f| f.combat.load().mob_list.iter().map(|m| m.last_seen).max())
        .cloned()
}

/// Surface height that fits the chrome plus `max_rows` rows plus the picker,
/// generously — unused space is transparent and outside the input region, so
/// oversizing costs nothing visible.
pub fn surface_height(max_rows: usize, font_size: f32) -> u32 {
    let row = font_size + 8.0;
    (70.0 + row * (max_rows as f32 + crate::meter_core::MAX_PICKER_ENTRIES as f32 + 2.0)) as u32
}

/// Consecutive surface-acquire failures — the frozen-overlay telltale.
static SURFACE_FAILS: AtomicU64 = AtomicU64::new(0);

const BTN_LEFT: u32 = 0x110;

/// `FROKLOG_METER_DEBUG=1` prints the overlay's lifecycle to stderr — surface
/// configures, GPU init, and why a frame was or wasn't painted.
fn debug_on() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("FROKLOG_METER_DEBUG").is_some())
}

macro_rules! mdbg {
    ($($t:tt)*) => {
        if debug_on() {
            eprintln!("meter: {}", format!($($t)*));
        }
    };
}

struct Gpu {
    device: egui_wgpu::wgpu::Device,
    queue: egui_wgpu::wgpu::Queue,
    surface: egui_wgpu::wgpu::Surface<'static>,
    config: egui_wgpu::wgpu::SurfaceConfiguration,
    renderer: egui_wgpu::Renderer,
}

impl Gpu {
    fn new(
        instance: Arc<egui_wgpu::wgpu::Instance>,
        window: Arc<layershellev::WindowWrapper>,
        width: u32,
        height: u32,
    ) -> Result<Self, String> {
        use egui_wgpu::wgpu;
        mdbg!("gpu: creating surface");
        let surface = instance
            .create_surface(window)
            .map_err(|e| format!("wgpu surface: {e}"))?;
        mdbg!("gpu: requesting adapter");
        let adapter = pollster::block_on(instance.request_adapter(&wgpu::RequestAdapterOptions {
            power_preference: wgpu::PowerPreference::LowPower,
            compatible_surface: Some(&surface),
            force_fallback_adapter: false,
        }))
        .ok_or("no wgpu adapter")?;
        mdbg!("gpu: adapter {:?}", adapter.get_info().name);
        let (device, queue) =
            pollster::block_on(adapter.request_device(&wgpu::DeviceDescriptor::default(), None))
                .map_err(|e| format!("wgpu device: {e}"))?;
        mdbg!("gpu: device ready");

        let caps = surface.get_capabilities(&adapter);
        mdbg!("gpu: caps ok");
        let format = caps
            .formats
            .iter()
            .copied()
            .find(|f| *f == wgpu::TextureFormat::Bgra8Unorm)
            .unwrap_or(caps.formats[0]);
        // Per-pixel transparency is the whole point; PreMultiplied is what
        // egui's output already is, PostMultiplied works too, Inherit is a
        // shrug that usually means premultiplied on Wayland.
        let alpha_mode = [
            wgpu::CompositeAlphaMode::PreMultiplied,
            wgpu::CompositeAlphaMode::PostMultiplied,
            wgpu::CompositeAlphaMode::Inherit,
        ]
        .into_iter()
        .find(|m| caps.alpha_modes.contains(m))
        .unwrap_or(caps.alpha_modes[0]);

        let config = wgpu::SurfaceConfiguration {
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT,
            format,
            width,
            height,
            present_mode: wgpu::PresentMode::Fifo,
            desired_maximum_frame_latency: 2,
            alpha_mode,
            view_formats: vec![],
        };
        surface.configure(&device, &config);
        let renderer = egui_wgpu::Renderer::new(&device, format, None, 1, false);
        Ok(Self {
            device,
            queue,
            surface,
            config,
            renderer,
        })
    }

    fn resize(&mut self, width: u32, height: u32) {
        if width > 0 && height > 0 && (width != self.config.width || height != self.config.height) {
            self.config.width = width;
            self.config.height = height;
            self.surface.configure(&self.device, &self.config);
        }
    }
}

struct App {
    kind: Kind,
    /// The message overlay's state — queue, what is flying, what has been.
    msgs: crate::messages::Messages,
    /// Whether the last frame had anything to show. The meter decides that
    /// from its own idle timer; the message overlay owns its retention rule
    /// (`Messages::tick`), so it reports back through here instead.
    showing: bool,
    /// Render pacing, shared with the ticker thread. An animating message
    /// needs ~60 Hz; a resting overlay does not, and burning the GPU behind
    /// a full-screen game is exactly what an overlay must not do.
    tick_ms: Arc<AtomicU64>,
    instance: Arc<egui_wgpu::wgpu::Instance>,
    feeds: Vec<Feed>,
    view: MeterView,
    style: MeterStyle,
    idle_secs: u64,
    locked: bool,
    preview: bool,
    x: i32,
    y: i32,
    width: u32,
    height: u32,
    gpu: Option<Gpu>,
    /// GPU init runs on its own thread: doing it inside the event callback
    /// deadlocks, because Vulkan's Wayland surface negotiation needs this
    /// very event loop to keep dispatching while we'd be blocking it.
    gpu_rx: Option<std_mpsc::Receiver<Result<Gpu, String>>>,
    egui: egui::Context,
    events: Vec<egui::Event>,
    pointer: egui::Pos2,
    /// Native title-strip drag. egui never sees the gesture: rendering per
    /// motion event blocks on vsync (laggy), and egui's per-frame deltas get
    /// rebased every time the surface moves under the cursor (rubber-band).
    /// Instead the press position is held fixed and each motion applies
    /// `current - press` to the margins — exact under a moving surface,
    /// no repaint needed.
    ///
    /// Press lands in the strip → `drag_press` (tentative). Motion past a
    /// 4 px threshold → `drag_active`. Release without the threshold → the
    /// press+release are forwarded to egui as a click (tabs/icons live in
    /// the strip too).
    drag_press: Option<egui::Pos2>,
    drag_active: bool,
    /// Surface position when the drag began. Verified against cosmic's
    /// actual event stream: the compositor does NOT rebase surface-local
    /// pointer coordinates when the surface moves itself via margins, so
    /// `pointer - press` is the TOTAL gesture displacement in stable
    /// coordinates — position is assigned absolutely from this origin.
    /// (Accumulating the delta re-applies the whole displacement every
    /// step: constant-velocity runaway.)
    drag_origin: (i32, i32),
    /// Small pacing window so a 1000 Hz mouse doesn't send a margin
    /// commit per hardware event.
    drag_settle_until: Option<Instant>,
    started: Instant,
    /// Last moment the meter had something to show — drives idle auto-hide.
    last_content: Option<Instant>,
    /// Input region currently applied, to avoid re-committing every frame.
    applied_region: Option<(i32, i32)>,
    compositor: Option<(WlCompositor, QueueHandle<WindowState<()>>)>,
    out: std_mpsc::Sender<OverlayEvent>,
}

impl App {
    /// Largest allowed top-left position: the surface's own monitor size
    /// minus enough panel to stay grabbable. Falls back to a 4K bound
    /// before the compositor has told us which output we're on.
    fn clamp_pos(&mut self, ev: &WindowState<()>) {
        let (ow, oh) = ev
            .main_window()
            .get_xdgoutput_info()
            .map(|i| i.get_logical_size())
            .filter(|&(w, h)| w > 0 && h > 0)
            .unwrap_or((3840, 2160));
        self.x = self.x.clamp(0, (ow - 120).max(0));
        self.y = self.y.clamp(0, (oh - 60).max(0));
    }

    /// Install the GPU once its init thread delivers it. `Err` = init failed
    /// and the overlay should exit (the caller falls back to the viewport).
    fn poll_gpu(&mut self) -> Result<(), String> {
        if self.gpu.is_some() {
            return Ok(());
        }
        let Some(rx) = &self.gpu_rx else {
            return Ok(());
        };
        match rx.try_recv() {
            Ok(Ok(mut gpu)) => {
                gpu.resize(self.width, self.height);
                mdbg!("wgpu ready: {:?}", gpu.config.alpha_mode);
                self.gpu = Some(gpu);
                self.gpu_rx = None;
                Ok(())
            }
            Ok(Err(e)) => Err(e),
            Err(_) => Ok(()), // still initializing
        }
    }

    fn hidden(&self) -> bool {
        if self.preview {
            return false;
        }
        match self.kind {
            // Retention is the message overlay's own business: it keeps the
            // list up for a while after the last arrival and then gives the
            // screen back.
            Kind::Messages => !self.showing,
            Kind::Meter => match self.last_content {
                None => true,
                Some(t) => self.idle_secs > 0 && t.elapsed().as_secs() > self.idle_secs,
            },
        }
    }

    /// Height of the grab strip. The meter reserves its title bar so the rows
    /// underneath stay clickable; the message overlay has nothing to click,
    /// so all of it drags.
    fn strip_height(&self) -> f32 {
        match self.kind {
            Kind::Meter => self.style.font_size * 1.6 + 14.0,
            Kind::Messages => self.height as f32,
        }
    }

    /// Apply the input region: `None` = empty (click-through), otherwise the
    /// painted panel's size. Committed only when it actually changes.
    fn apply_input_region(&mut self, ev: &WindowState<()>, painted: Option<(i32, i32)>) {
        let want = painted.unwrap_or((0, 0));
        if self.applied_region == Some(want) {
            return;
        }
        let Some((compositor, qh)) = &self.compositor else {
            return;
        };
        let unit = ev.main_window();
        let region = compositor.create_region(qh, ());
        if want != (0, 0) {
            region.add(0, 0, want.0, want.1);
        }
        unit.get_wlsurface().set_input_region(Some(&region));
        region.destroy();
        unit.get_wlsurface().commit();
        self.applied_region = Some(want);
    }

    fn render(&mut self, ev: &WindowState<()>) {
        // egui must not run before the renderer can receive its texture
        // deltas: every egui.run() is stateful, and any delta we drop here
        // (fonts, atlas growth) poisons all later frames.
        if self.gpu.is_none() {
            return;
        }
        match self.kind {
            Kind::Meter => self.render_meter(ev),
            Kind::Messages => self.render_messages(ev),
        }
    }

    /// The message overlay's frame: advance the lifecycle, paint the banner
    /// and the history, and speed the ticker up while something is moving.
    fn render_messages(&mut self, ev: &WindowState<()>) {
        let raw = egui::RawInput {
            screen_rect: Some(egui::Rect::from_min_size(
                egui::Pos2::ZERO,
                egui::vec2(self.width as f32, self.height as f32),
            )),
            time: Some(self.started.elapsed().as_secs_f64()),
            events: std::mem::take(&mut self.events),
            ..Default::default()
        };

        let mut showing = false;
        let msgs = &mut self.msgs;
        let locked = self.locked;
        let full = self.egui.run(raw, |ctx| {
            egui::CentralPanel::default()
                .frame(egui::Frame::none())
                .show(ctx, |ui| {
                    let panel = egui::Frame::none()
                        .fill(egui::Color32::from_rgba_unmultiplied(10, 10, 16, 208))
                        .rounding(8.0)
                        .inner_margin(8.0);
                    let r = panel.show(ui, |ui| {
                        ui.set_width(ui.available_width() - 16.0);
                        showing = msgs.draw(ui, locked);
                    });
                    ui.data_mut(|d| {
                        d.insert_temp(egui::Id::new("panel-h"), r.response.rect.height())
                    });
                });
        });
        self.showing = showing;
        // 60 Hz only while a message is actually flying — the rest of the
        // time this window is a static list and 5 Hz is plenty.
        self.tick_ms.store(
            if self.msgs.animating() { 16 } else { 200 },
            Ordering::Relaxed,
        );
        self.finish_frame(ev, full);
    }

    fn render_meter(&mut self, ev: &WindowState<()>) {
        // With no pipelines at all, preview still needs a surface to drag —
        // feed the UI an empty state.
        let empty_feed;
        let feed = match pick_feed(&self.feeds) {
            Some(f) => f,
            None if self.preview => {
                empty_feed = Feed {
                    combat: Arc::new(ArcSwap::from_pointee(CombatState::default())),
                    reset: Arc::new(AtomicBool::new(false)),
                };
                empty_feed.clone()
            }
            None => {
                mdbg!("render: no feeds");
                self.paint_empty(ev);
                return;
            }
        };
        let cs = feed.combat.load_full();

        let raw = egui::RawInput {
            screen_rect: Some(egui::Rect::from_min_size(
                egui::Pos2::ZERO,
                egui::vec2(self.width as f32, self.height as f32),
            )),
            time: Some(self.started.elapsed().as_secs_f64()),
            events: std::mem::take(&mut self.events),
            ..Default::default()
        };

        let mut actions: Vec<MeterAction> = Vec::new();
        let mut has_content = false;
        let view = &mut self.view;
        let style = self.style;
        let locked = self.locked;
        let preview = self.preview;
        let full = self.egui.run(raw, |ctx| {
            egui::CentralPanel::default()
                .frame(egui::Frame::none())
                .show(ctx, |ui| {
                    let panel = egui::Frame::none()
                        .fill(egui::Color32::from_rgba_unmultiplied(10, 10, 16, 208))
                        .rounding(8.0)
                        .inner_margin(8.0);
                    let r = panel.show(ui, |ui| {
                        ui.set_width(ui.available_width() - 16.0);
                        let (acts, content) = meter_ui::draw(ui, view, &cs, style, locked, preview);
                        actions.extend(acts);
                        has_content = content;
                    });
                    // Remember how tall the painted panel actually is so the
                    // input region can hug it.
                    ui.data_mut(|d| {
                        d.insert_temp(egui::Id::new("panel-h"), r.response.rect.height())
                    });
                });
        });

        if has_content {
            self.last_content = Some(Instant::now());
        }
        mdbg!(
            "render: content={has_content} hidden={} mobs={} gpu={}",
            self.hidden(),
            cs.mob_list.len(),
            self.gpu.is_some()
        );

        for act in actions {
            match act {
                MeterAction::Drag(delta) => {
                    self.x = (self.x + delta.x.round() as i32).max(0);
                    self.y = (self.y + delta.y.round() as i32).max(0);
                    ev.main_window().set_margin((self.y, 0, 0, self.x));
                    let _ = self.out.send(OverlayEvent::Moved(self.x, self.y));
                }
                MeterAction::Copy(text) => {
                    let _ = self.out.send(OverlayEvent::Copy(text));
                }
                MeterAction::Reset => {
                    feed.reset.store(true, std::sync::atomic::Ordering::Relaxed);
                }
                MeterAction::OpenSettings => {
                    let _ = self.out.send(OverlayEvent::OpenSettings);
                }
            }
        }

        self.finish_frame(ev, full);
    }

    /// Present whatever egui just produced: nothing at all when hidden, the
    /// tessellated frame otherwise, with the input region hugging the panel.
    fn finish_frame(&mut self, ev: &WindowState<()>, full: egui::FullOutput) {
        if self.hidden() {
            // Paint nothing — but STILL apply the texture deltas: egui
            // tracks what it has already uploaded, and dropping the initial
            // font-atlas allocation here poisons every later frame ("update
            // a texture that has not been allocated yet").
            self.events.clear();
            self.apply_input_region(ev, None);
            self.paint(&[], &full.textures_delta, full.pixels_per_point);
            return;
        }

        let panel_h = self
            .egui
            .data(|d| d.get_temp::<f32>(egui::Id::new("panel-h")))
            .unwrap_or(self.height as f32);
        let region = if self.locked {
            None
        } else {
            Some((self.width as i32, panel_h.ceil() as i32 + 4))
        };
        self.apply_input_region(ev, region);

        let ppp = full.pixels_per_point;
        let primitives = self.egui.tessellate(full.shapes, ppp);
        self.paint(&primitives, &full.textures_delta, ppp);
    }

    /// Present a fully transparent frame and make the surface click-through —
    /// the no-feeds state, before egui has ever run (so no deltas to lose).
    fn paint_empty(&mut self, ev: &WindowState<()>) {
        self.events.clear();
        self.apply_input_region(ev, None);
        self.paint(&[], &Default::default(), 1.0);
    }

    fn paint(
        &mut self,
        primitives: &[egui::ClippedPrimitive],
        textures: &egui::TexturesDelta,
        ppp: f32,
    ) {
        use egui_wgpu::wgpu;
        let Some(gpu) = &mut self.gpu else { return };
        // Texture deltas FIRST, before anything that can fail — a dropped
        // delta desyncs egui's idea of what lives on the GPU permanently.
        for (id, delta) in &textures.set {
            gpu.renderer
                .update_texture(&gpu.device, &gpu.queue, *id, delta);
        }
        let frame = match gpu.surface.get_current_texture() {
            Ok(f) => {
                // Recovered (or healthy): say so once if we had been failing,
                // so a stuck-then-fixed window leaves a full story in stderr.
                let failed = SURFACE_FAILS.swap(0, Ordering::Relaxed);
                if failed > 0 {
                    eprintln!(
                        "overlay: surface recovered after {failed} failed acquires \
                         (suspend/monitor sleep?)"
                    );
                }
                f
            }
            Err(e) => {
                mdbg!("paint: acquire failed ({e}), reconfiguring");
                gpu.surface.configure(&gpu.device, &gpu.config);
                match gpu.surface.get_current_texture() {
                    Ok(f) => f,
                    Err(e) => {
                        // ALWAYS-ON evidence: a window frozen for hours with
                        // debug off previously left no trace at all. Log the
                        // 1st, 10th, 100th… so stderr records the outage
                        // without becoming the outage.
                        let n = SURFACE_FAILS.fetch_add(1, Ordering::Relaxed) + 1;
                        if n == 1 || n.is_multiple_of(100) {
                            eprintln!(
                                "overlay: cannot acquire surface ({e}) — {n} consecutive \
                                 failures; window is frozen until this recovers"
                            );
                        }
                        for id in &textures.free {
                            gpu.renderer.free_texture(id);
                        }
                        return;
                    }
                }
            }
        };
        let view = frame
            .texture
            .create_view(&wgpu::TextureViewDescriptor::default());
        let mut encoder = gpu
            .device
            .create_command_encoder(&wgpu::CommandEncoderDescriptor { label: None });
        let screen = egui_wgpu::ScreenDescriptor {
            size_in_pixels: [gpu.config.width, gpu.config.height],
            pixels_per_point: ppp,
        };
        let user_cmds =
            gpu.renderer
                .update_buffers(&gpu.device, &gpu.queue, &mut encoder, primitives, &screen);
        {
            let mut pass = encoder
                .begin_render_pass(&wgpu::RenderPassDescriptor {
                    label: Some("meter"),
                    color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                        view: &view,
                        resolve_target: None,
                        ops: wgpu::Operations {
                            load: wgpu::LoadOp::Clear(wgpu::Color::TRANSPARENT),
                            store: wgpu::StoreOp::Store,
                        },
                    })],
                    depth_stencil_attachment: None,
                    timestamp_writes: None,
                    occlusion_query_set: None,
                })
                .forget_lifetime();
            gpu.renderer.render(&mut pass, primitives, &screen);
        }
        for id in &textures.free {
            gpu.renderer.free_texture(id);
        }
        gpu.queue
            .submit(user_cmds.into_iter().chain([encoder.finish()]));
        frame.present();
    }
}

/// Spawn the overlay thread. Returns immediately; if layer-shell is missing
/// the handle's event receiver yields `Exited(Some(..))` shortly after.
pub fn spawn(init: OverlaySpawn) -> OverlayHandle {
    let (tx, rx) = layershellev::calloop::channel::channel::<OverlayMsg>();
    let (out_tx, out_rx) = std_mpsc::channel::<OverlayEvent>();

    // Render pacing: 5 Hz keeps timers/rows fresh without burning GPU. The
    // message overlay raises this to 60 Hz while a message is in flight and
    // drops it straight back.
    let tick_ms = Arc::new(AtomicU64::new(200));
    {
        let tx = tx.clone();
        let tick_ms = Arc::clone(&tick_ms);
        std::thread::Builder::new()
            .name("meter-tick".into())
            .spawn(move || loop {
                std::thread::sleep(Duration::from_millis(
                    tick_ms.load(Ordering::Relaxed).max(8),
                ));
                if tx.send(OverlayMsg::Tick).is_err() {
                    break;
                }
            })
            .ok();
    }

    let out_for_thread = out_tx.clone();
    std::thread::Builder::new()
        .name("meter-overlay".into())
        .spawn(move || {
            // A panic anywhere in the loop must still tell the main app the
            // overlay is gone, or the meter silently stops existing.
            let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                run(init, rx, out_for_thread.clone(), tick_ms)
            }))
            .unwrap_or_else(|p| {
                let msg = p
                    .downcast_ref::<&str>()
                    .map(|s| s.to_string())
                    .or_else(|| p.downcast_ref::<String>().cloned())
                    .unwrap_or_else(|| "panic".into());
                Err(format!("overlay panicked: {msg}"))
            });
            let _ = out_for_thread.send(OverlayEvent::Exited(result.err()));
        })
        .ok();

    OverlayHandle { tx, events: out_rx }
}

fn run(
    init: OverlaySpawn,
    rx: layershellev::calloop::channel::Channel<OverlayMsg>,
    out: std_mpsc::Sender<OverlayEvent>,
    tick_ms: Arc<AtomicU64>,
) -> Result<(), String> {
    let mut msgs = crate::messages::Messages::default();
    msgs.style = init.msg_style;
    let height = match init.kind {
        Kind::Meter => surface_height(init.max_rows, init.font_size),
        Kind::Messages => msgs.surface_height(),
    };
    let name = match init.kind {
        Kind::Meter => "froklog-meter",
        Kind::Messages => "froklog-messages",
    };
    let mut builder = WindowState::new(name)
        .with_size((init.width, height))
        .with_layer(Layer::Overlay)
        .with_anchor(Anchor::Top | Anchor::Left)
        .with_margin((init.y, 0, 0, init.x))
        .with_keyboard_interacivity(KeyboardInteractivity::None)
        .with_use_display_handle(true);
    if !init.output.is_empty() {
        // Falls back to the focused monitor if the name no longer exists
        // (monitor unplugged) — layershellev handles that gracefully.
        builder = builder.with_xdg_output_name(init.output.clone());
    }
    let ev: WindowState<()> = builder
        .build()
        .map_err(|e| format!("layer-shell unavailable: {e}"))?;

    msgs.style = init.msg_style;
    let mut app = App {
        kind: init.kind,
        msgs,
        showing: false,
        tick_ms,
        instance: init.instance,
        feeds: init.feeds,
        view: MeterView::default(),
        style: MeterStyle {
            max_rows: init.max_rows,
            font_size: init.font_size,
        },
        idle_secs: init.idle_secs,
        locked: init.locked,
        preview: false,
        x: init.x,
        y: init.y,
        width: init.width,
        height,
        gpu: None,
        gpu_rx: None,
        egui: egui::Context::default(),
        events: Vec::new(),
        pointer: egui::Pos2::ZERO,
        drag_press: None,
        drag_active: false,
        drag_origin: (0, 0),
        drag_settle_until: None,
        started: Instant::now(),
        last_content: None,
        applied_region: None,
        compositor: None,
        out,
    };

    ev.running_with_proxy(rx, move |event, ev, _id| {
        match event {
            LayerShellEvent::InitRequest => return ReturnData::RequestCompositor,
            LayerShellEvent::CompositorProvide(compositor, qh) => {
                app.compositor = Some((compositor.clone(), qh.clone()));
            }
            LayerShellEvent::RequestMessages(msg) => match msg {
                DispatchMessage::RequestRefresh {
                    width,
                    height,
                    is_created,
                    ..
                } => {
                    mdbg!("configure {width}x{height} created={is_created}");
                    app.width = *width;
                    app.height = *height;
                    // A stale config can hold an off-screen position (a past
                    // runaway saved it) — pull it back inside the output.
                    let (px, py) = (app.x, app.y);
                    app.clamp_pos(ev);
                    if (px, py) != (app.x, app.y) {
                        mdbg!("position clamped ({px},{py}) -> ({},{})", app.x, app.y);
                        ev.main_window().set_margin((app.y, 0, 0, app.x));
                        let _ = app.out.send(OverlayEvent::Moved(app.x, app.y));
                    }
                    // The surface exists once any configure arrives (don't
                    // trust is_created — the first configure reports false).
                    if app.gpu.is_none() && app.gpu_rx.is_none() && *width > 0 && *height > 0 {
                        let wrapper = Arc::new(ev.gen_mainwindow_wrapper());
                        let (gtx, grx) = std_mpsc::channel();
                        let (w, h) = (*width, *height);
                        let instance = Arc::clone(&app.instance);
                        std::thread::Builder::new()
                            .name("meter-gpu-init".into())
                            .spawn(move || {
                                let _ = gtx.send(Gpu::new(instance, wrapper, w, h));
                            })
                            .ok();
                        app.gpu_rx = Some(grx);
                    }
                    if let Err(e) = app.poll_gpu() {
                        eprintln!("meter overlay: {e}");
                        return ReturnData::RequestExit;
                    }
                    if let Some(gpu) = &mut app.gpu {
                        gpu.resize(*width, *height);
                    }
                    app.render(ev);
                }
                DispatchMessage::MouseEnter {
                    pointer,
                    surface_x,
                    surface_y,
                    ..
                } => {
                    if app.drag_active || app.drag_press.is_some() {
                        mdbg!(
                            "drag ENTER at ({surface_x:.0},{surface_y:.0}) — surface moved under cursor"
                        );
                    }
                    app.pointer = egui::pos2(*surface_x as f32, *surface_y as f32);
                    app.events.push(egui::Event::PointerMoved(app.pointer));
                    return ReturnData::RequestSetCursorShape((
                        "default".to_owned(),
                        pointer.clone(),
                    ));
                }
                DispatchMessage::MouseLeave if app.drag_active || app.drag_press.is_some() => {
                    mdbg!("drag LEAVE — cancelling gesture (release would never reach us)");
                    app.drag_press = None;
                    app.drag_active = false;
                    app.drag_settle_until = None;
                    let _ = app.out.send(OverlayEvent::Moved(app.x, app.y));
                    app.events.push(egui::Event::PointerGone);
                }
                DispatchMessage::MouseMotion {
                    surface_x,
                    surface_y,
                    ..
                } => {
                    app.pointer = egui::pos2(*surface_x as f32, *surface_y as f32);
                    if let Some(press) = app.drag_press {
                        if !app.drag_active && app.pointer.distance(press) > 4.0 {
                            app.drag_active = true;
                        }
                    }
                    if app.drag_active {
                        let settled = app
                            .drag_settle_until
                            .map(|t| Instant::now() >= t)
                            .unwrap_or(true);
                        if settled {
                            let press = app.drag_press.unwrap_or(app.pointer);
                            // Total displacement since press, in the stable
                            // (never-rebased) coordinate space.
                            let dx = (app.pointer.x - press.x).round() as i32;
                            let dy = (app.pointer.y - press.y).round() as i32;
                            let (nx, ny) = (app.drag_origin.0 + dx, app.drag_origin.1 + dy);
                            if (nx, ny) != (app.x, app.y) {
                                mdbg!(
                                    "drag: ptr=({:.0},{:.0}) press=({:.0},{:.0}) origin=({},{}) -> ({nx},{ny})",
                                    app.pointer.x,
                                    app.pointer.y,
                                    press.x,
                                    press.y,
                                    app.drag_origin.0,
                                    app.drag_origin.1
                                );
                                app.x = nx;
                                app.y = ny;
                                app.clamp_pos(ev);
                                ev.main_window().set_margin((app.y, 0, 0, app.x));
                                app.drag_settle_until =
                                    Some(Instant::now() + Duration::from_millis(8));
                            }
                        }
                    } else if app.drag_press.is_none() {
                        app.events.push(egui::Event::PointerMoved(app.pointer));
                    }
                }
                DispatchMessage::MouseLeave => {
                    app.events.push(egui::Event::PointerGone);
                }
                DispatchMessage::MouseButton { state, button, .. } => {
                    if *button == BTN_LEFT {
                        use layershellev::reexport::wayland_client::{ButtonState, WEnum};
                        let pressed = matches!(state, WEnum::Value(ButtonState::Pressed));
                        // The title strip (panel margin + tab row) is the
                        // drag handle, like the Windows meter's caption.
                        let strip_h = app.strip_height();
                        if pressed && !app.locked && app.pointer.y <= strip_h {
                            app.drag_press = Some(app.pointer);
                            app.drag_origin = (app.x, app.y);
                            return ReturnData::None;
                        }
                        if !pressed && app.drag_press.is_some() {
                            let was_drag = app.drag_active;
                            let press = app.drag_press.take().unwrap();
                            app.drag_active = false;
                            app.drag_settle_until = None;
                            if was_drag {
                                // Gesture over — persist the final position.
                                let _ = app.out.send(OverlayEvent::Moved(app.x, app.y));
                            } else {
                                // A click after all: replay it for egui.
                                for p in [true, false] {
                                    app.events.push(egui::Event::PointerButton {
                                        pos: press,
                                        button: egui::PointerButton::Primary,
                                        pressed: p,
                                        modifiers: egui::Modifiers::default(),
                                    });
                                }
                                app.render(ev);
                                // What the click changed (the tab, say) was
                                // decided after this frame's rows were built,
                                // so paint once more or the effect shows up a
                                // whole tick late and reads as a dead button.
                                app.render(ev);
                            }
                            return ReturnData::None;
                        }
                        app.events.push(egui::Event::PointerButton {
                            pos: app.pointer,
                            button: egui::PointerButton::Primary,
                            pressed,
                            modifiers: egui::Modifiers::default(),
                        });
                        // Deliver clicks immediately: ticks are 200 ms apart
                        // and a press+release can land inside one gap.
                        app.render(ev);
                    }
                }
                _ => {}
            },
            LayerShellEvent::UserEvent(msg) => match msg {
                OverlayMsg::Tick => {
                    if let Err(e) = app.poll_gpu() {
                        eprintln!("meter overlay: {e}");
                        return ReturnData::RequestExit;
                    }
                    app.render(ev);
                }
                OverlayMsg::Announce(msg) => {
                    app.msgs.push(*msg);
                    // Straight to the screen: waiting for the next tick would
                    // put up to a fifth of a second between the sound and the
                    // words, which is exactly the gap that reads as lag.
                    app.render(ev);
                }
                OverlayMsg::SetMessageStyle(style) => {
                    app.msgs.style = style;
                    let h = app.msgs.surface_height();
                    if h != app.height {
                        app.height = h;
                        ev.main_window().set_size((app.width, h));
                    }
                    app.render(ev);
                }
                OverlayMsg::Feeds(feeds) => app.feeds = feeds,
                OverlayMsg::SetLocked(locked) => {
                    app.locked = locked;
                    app.render(ev);
                }
                OverlayMsg::SetStyle {
                    max_rows,
                    font_size,
                    idle_secs,
                } => {
                    app.style = MeterStyle {
                        max_rows,
                        font_size,
                    };
                    app.idle_secs = idle_secs;
                }
                OverlayMsg::SetPosition(x, y) => {
                    app.x = x;
                    app.y = y;
                    app.clamp_pos(ev);
                    ev.main_window().set_margin((app.y, 0, 0, app.x));
                }
                OverlayMsg::Preview(on) => {
                    app.preview = on;
                    app.msgs.preview = on;
                    app.render(ev);
                }
                OverlayMsg::SetSize(w, h) => {
                    ev.main_window().set_size((w, h));
                    // wgpu reconfigures when the compositor's configure
                    // event delivers the accepted size.
                }
                OverlayMsg::Quit => {
                    // Teardown order matters: the Vulkan surface must die
                    // while the Wayland connection is still alive, and a
                    // GPU init still in flight must finish before we take
                    // the connection away from under it.
                    if let Some(rx) = app.gpu_rx.take() {
                        let _ = rx.recv_timeout(Duration::from_secs(5));
                    }
                    app.gpu = None;
                    return ReturnData::RequestExit;
                }
            },
            _ => {}
        }
        ReturnData::None
    })
    .map_err(|e| format!("overlay loop: {e}"))
}
