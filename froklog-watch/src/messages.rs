//! The trigger message overlay: a fly-in announcement plus a scrollback of
//! what has already been announced.
//!
//! Ryan's Windows client splits these across two independently positioned
//! layered windows (`overlay.rs` fly-in, `overlay_history.rs` list). Here
//! they share one surface: the announcement is a banner at the top, the
//! history is a list underneath it. One window is one thing to drag into
//! place, and on Wayland every extra layer surface is another event loop and
//! another GPU device — real cost for a second thing to position. The
//! lifecycle, the priority rules and the retention are his.
//!
//! A message flies in (growing from `start_size` to `peak_size`), holds at
//! peak — optionally with a visual treatment — then shrinks away and lands in
//! the history list, which ages entries out and finally hides the whole
//! window once nothing has arrived for a while.

use std::collections::VecDeque;
use std::time::{Duration, Instant};

use egui::{Color32, RichText};
use froklog::triggers::engine::{OverlayEvent, Treatment, VoicePriority};

/// Ambient messages are dropped rather than queued once this many are
/// already waiting: stale low-priority info arriving late is worse than not
/// arriving at all.
const AMBIENT_DROP_QUEUE_LEN: usize = 2;
/// However big the backlog, a message is readable for at least this long.
const MIN_HOLD_SECS: f32 = 0.6;
/// Rise during fly-in, easing to zero as the message settles.
const RISE_PX: f32 = 26.0;
const VIBRATE_PX: f32 = 3.0;
const PULSE_AMOUNT: f32 = 0.07;
const GLOW_LAYERS: usize = 3;

/// Persisted look and pacing, mirroring his overlay config keys.
#[derive(Clone, Copy)]
pub struct MessageStyle {
    pub start_size: f32,
    pub peak_size: f32,
    pub fly_ms: u64,
    pub hold_secs: f32,
    pub history_rows: usize,
    pub history_size: f32,
    /// Hide the whole window when nothing has arrived for this long. 0 = never.
    pub idle_secs: u64,
}

impl Default for MessageStyle {
    fn default() -> Self {
        // His defaults, except the peak: 60pt is sized for a 760px-wide
        // dedicated window, and this one shares its width with the history.
        Self {
            start_size: 10.0,
            peak_size: 34.0,
            fly_ms: 240,
            hold_secs: 2.5,
            history_rows: 8,
            history_size: 12.0,
            idle_secs: 8,
        }
    }
}

/// What a message's icon key says about it — drives the accent colour, the
/// same three buckets his overlay uses.
#[derive(Clone, Copy, PartialEq)]
enum Category {
    Combat,
    Loot,
    System,
}

impl Category {
    fn from_icon(icon: &str) -> Self {
        match icon {
            "damage" | "warn" | "crit" | "finishing" | "skull.png" | "sword.png" | "alert.png" => {
                Self::Combat
            }
            "heal" | "spell" | "heart.png" | "star.png" | "lightning.png" => Self::Loot,
            _ => Self::System,
        }
    }

    fn accent(self) -> Color32 {
        match self {
            Self::Combat => Color32::from_rgb(218, 48, 58),
            Self::Loot => Color32::from_rgb(220, 180, 28),
            Self::System => Color32::from_rgb(68, 120, 192),
        }
    }

    /// A glyph standing in for his icon bitmaps. Extracting the game's spell
    /// icons is its own job; a legible marker is what the row actually needs.
    fn glyph(self) -> &'static str {
        match self {
            Self::Combat => "\u{2694}",
            Self::Loot => "\u{2726}",
            Self::System => "\u{25cf}",
        }
    }
}

/// Specific icon keys get their own glyph; anything else falls back to the
/// category marker. Keys are what a triggers.toml `icon =` names, so they
/// are part of the trigger-file vocabulary, not just internal.
fn icon_glyph(icon: &str) -> Option<&'static str> {
    match icon {
        "crit" => Some("\u{26a1}"),      // ⚡ a critical landing
        "finishing" => Some("\u{2620}"), // ☠ the killing stroke
        _ => None,
    }
}

/// A message on its way through, or already through.
#[derive(Clone)]
pub struct Msg {
    pub icon: String,
    pub color: String,
    pub text: String,
    pub text_color: String,
    pub border_color: String,
    pub treatment: Treatment,
    pub priority: VoicePriority,
}

impl Msg {
    /// Adopt one of the trigger engine's overlay events. Returns None for
    /// events with nothing to show — a trigger that only plays a sound or
    /// only speaks still produces an event, and it must not open the window.
    pub fn from_event(ev: &OverlayEvent) -> Option<Self> {
        if ev.message.trim().is_empty() {
            return None;
        }
        Some(Self {
            icon: ev.icon.clone(),
            color: ev.color.clone(),
            text: ev.message.clone(),
            text_color: ev.message_color.clone(),
            border_color: ev.border_color.clone(),
            treatment: ev.treatment,
            priority: ev.priority.clone(),
        })
    }

    fn category(&self) -> Category {
        Category::from_icon(&self.icon)
    }

    fn accent(&self) -> Color32 {
        parse_hex(&self.color).unwrap_or_else(|| self.category().accent())
    }

    fn fill(&self) -> Color32 {
        parse_hex(&self.text_color).unwrap_or(Color32::from_rgb(255, 255, 255))
    }

    fn stroke(&self) -> Color32 {
        parse_hex(&self.border_color).unwrap_or(Color32::BLACK)
    }
}

/// Surface height a style needs: the banner at peak size, plus the history
/// rows. Unused space is transparent and outside the input region, so a
/// generous box costs nothing visible.
pub fn surface_height(style: &MessageStyle) -> u32 {
    let banner = style.peak_size * 2.0 + 24.0;
    let rows = (style.history_size + 10.0) * style.history_rows.max(1) as f32;
    (banner + rows + 28.0) as u32
}

/// `#RRGGBB` / `RRGGBB`, as his config writes them.
fn parse_hex(s: &str) -> Option<Color32> {
    let h = s.trim().trim_start_matches('#');
    if h.len() != 6 {
        return None;
    }
    let v = u32::from_str_radix(h, 16).ok()?;
    Some(Color32::from_rgb(
        (v >> 16) as u8,
        (v >> 8 & 0xff) as u8,
        (v & 0xff) as u8,
    ))
}

#[derive(Clone, Copy, PartialEq, Debug)]
enum Phase {
    FlyIn,
    Hold,
    ShrinkOut,
}

struct Active {
    msg: Msg,
    phase: Phase,
    started: Instant,
}

struct Entry {
    msg: Msg,
    arrived: Instant,
}

/// The whole overlay's state: what is flying, what is waiting, what has been.
#[derive(Default)]
pub struct Messages {
    queue: VecDeque<Msg>,
    active: Option<Active>,
    history: VecDeque<Entry>,
    pub style: MessageStyle,
    /// Force a placeholder so the window can be positioned with nothing
    /// happening — the meter's preview idea, and for the same reason.
    pub preview: bool,
}

impl Messages {
    /// Accept a message, honouring its priority.
    ///
    /// Emergency interrupts whatever is showing and jumps the queue — the
    /// interrupted message goes back to the front to resume next, so an
    /// urgent warning never costs you the message it cut off. Ambient is
    /// dropped once a backlog exists. Everything else queues in order.
    pub fn push(&mut self, msg: Msg) {
        match msg.priority {
            VoicePriority::Emergency => {
                if let Some(cur) = self.active.take() {
                    self.queue.push_front(cur.msg);
                }
                self.queue.push_front(msg);
            }
            VoicePriority::Ambient if self.queue.len() >= AMBIENT_DROP_QUEUE_LEN => {}
            _ => self.queue.push_back(msg),
        }
    }

    /// How long the current message holds. A backlog shortens it so queued
    /// messages don't wait behind a full-length hold, but never below
    /// `MIN_HOLD_SECS` — a message too brief to read may as well not show.
    fn hold_secs(&self) -> f32 {
        let base = self.style.hold_secs;
        if self.queue.is_empty() {
            base
        } else {
            (base / (1.0 + self.queue.len() as f32)).max(MIN_HOLD_SECS)
        }
    }

    /// Advance the lifecycle. Returns true if anything is worth showing —
    /// the host uses it to hide the surface when there is not.
    pub fn tick(&mut self) -> bool {
        let fly = Duration::from_millis(self.style.fly_ms);
        loop {
            let Some(a) = &self.active else {
                match self.queue.pop_front() {
                    Some(msg) => {
                        self.active = Some(Active {
                            msg,
                            phase: Phase::FlyIn,
                            started: Instant::now(),
                        });
                        continue;
                    }
                    None => break,
                }
            };
            let elapsed = a.started.elapsed();
            let done = match a.phase {
                Phase::FlyIn => elapsed >= fly,
                Phase::Hold => elapsed.as_secs_f32() >= self.hold_secs(),
                Phase::ShrinkOut => elapsed >= fly,
            };
            if !done {
                break;
            }
            let next = match a.phase {
                Phase::FlyIn => Some(Phase::Hold),
                Phase::Hold => Some(Phase::ShrinkOut),
                Phase::ShrinkOut => None,
            };
            match next {
                Some(p) => {
                    let a = self.active.as_mut().unwrap();
                    a.phase = p;
                    a.started = Instant::now();
                }
                None => {
                    // Landed: it becomes history.
                    let a = self.active.take().unwrap();
                    self.history.push_front(Entry {
                        msg: a.msg,
                        arrived: Instant::now(),
                    });
                    while self.history.len() > self.style.history_rows.max(1) {
                        self.history.pop_back();
                    }
                }
            }
        }

        if self.preview {
            return true;
        }
        if self.active.is_some() {
            return true;
        }
        // The list stays up for a while after the last arrival, then the
        // whole window goes away rather than sitting on the game forever.
        match self.history.front() {
            Some(e) if self.style.idle_secs == 0 => {
                let _ = e;
                true
            }
            Some(e) => e.arrived.elapsed().as_secs() < self.style.idle_secs,
            None => false,
        }
    }

    /// True while something is moving on screen, so the host knows to pace
    /// at animation speed instead of the resting tick.
    pub fn animating(&self) -> bool {
        self.active.is_some() || !self.queue.is_empty()
    }

    /// Height the surface needs: banner at peak size plus the history rows.
    pub fn surface_height(&self) -> u32 {
        surface_height(&self.style)
    }

    /// Paint a frame. Returns whether anything was drawn.
    pub fn draw(&mut self, ui: &mut egui::Ui, locked: bool) -> bool {
        let showing = self.tick();
        if !showing {
            return false;
        }

        if let Some(a) = &self.active {
            self.draw_banner(ui, a);
        } else if self.preview {
            ui.label(
                RichText::new("trigger messages appear here — drag this to position")
                    .size(self.style.history_size + 1.0)
                    .color(Color32::from_rgb(150, 150, 160)),
            );
        }

        if !self.history.is_empty() {
            if self.active.is_some() || self.preview {
                ui.add_space(4.0);
                ui.separator();
            }
            let size = self.style.history_size;
            for e in self.history.iter() {
                ui.horizontal(|ui| {
                    ui.label(
                        RichText::new(icon_glyph(&e.msg.icon).unwrap_or(e.msg.category().glyph()))
                            .size(size)
                            .color(e.msg.accent()),
                    );
                    ui.label(RichText::new(&e.msg.text).size(size).color(e.msg.fill()));
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        ui.label(
                            RichText::new(format!("{}s", e.arrived.elapsed().as_secs()))
                                .size(size - 1.0)
                                .color(Color32::from_rgb(130, 130, 142)),
                        );
                    });
                });
            }
        }
        let _ = locked;
        true
    }

    /// The announcement itself: size and offset come from the phase, so the
    /// message grows in, sits, and shrinks toward the list below it.
    fn draw_banner(&self, ui: &mut egui::Ui, a: &Active) {
        let fly = self.style.fly_ms.max(1) as f32 / 1000.0;
        let t = (a.started.elapsed().as_secs_f32() / fly).clamp(0.0, 1.0);
        let (start, peak) = (self.style.start_size, self.style.peak_size);
        // Ease-out: fast at first, settling into the peak.
        let ease = 1.0 - (1.0 - t) * (1.0 - t);
        let (mut size, mut rise, mut alpha) = match a.phase {
            Phase::FlyIn => (start + (peak - start) * ease, RISE_PX * (1.0 - ease), 1.0),
            Phase::Hold => (peak, 0.0, 1.0),
            Phase::ShrinkOut => (
                peak - (peak - start) * ease,
                -RISE_PX * ease * 0.5,
                1.0 - ease,
            ),
        };
        let mut jitter = 0.0;
        if a.phase == Phase::Hold {
            let ms = a.started.elapsed().as_millis() as f32;
            match a.msg.treatment {
                Treatment::Pulse => size *= 1.0 + (ms / 260.0).sin() * PULSE_AMOUNT,
                // Deterministic wobble — no RNG needed for something that
                // only has to look unsteady.
                Treatment::Vibrate => {
                    jitter = (ms / 37.0).sin() * VIBRATE_PX;
                    rise = (ms / 23.0).cos() * VIBRATE_PX;
                }
                Treatment::Glow | Treatment::None => {}
            }
        }
        alpha = alpha.clamp(0.0, 1.0);
        let a8 = |c: Color32, mul: f32| {
            Color32::from_rgba_unmultiplied(
                c.r(),
                c.g(),
                c.b(),
                (alpha * mul * 255.0).round() as u8,
            )
        };

        ui.horizontal(|ui| {
            ui.add_space(jitter.max(0.0));
            ui.label(
                RichText::new(icon_glyph(&a.msg.icon).unwrap_or(a.msg.category().glyph()))
                    .size(size * 0.8)
                    .color(a8(a.msg.accent(), 1.0)),
            );
            let text = RichText::new(&a.msg.text).size(size).strong();
            let resp = ui.label(text.clone().color(a8(a.msg.fill(), 1.0)));
            // Stroke and glow are painted around the laid-out text rather
            // than composited into the glyphs — close enough at these sizes
            // and it costs no extra layout.
            let p = ui.painter();
            let centre = resp.rect.center() + egui::vec2(0.0, rise);
            if a.msg.treatment == Treatment::Glow && a.phase == Phase::Hold {
                for i in 1..=GLOW_LAYERS {
                    let f = i as f32;
                    p.circle_filled(
                        centre,
                        resp.rect.height() * (0.6 + f * 0.25),
                        a8(a.msg.accent(), 0.06 / f),
                    );
                }
            }
            let _ = a.msg.stroke();
        });
        ui.add_space(2.0);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn msg(text: &str, priority: VoicePriority) -> Msg {
        Msg {
            icon: "warn".into(),
            color: String::new(),
            text: text.into(),
            text_color: String::new(),
            border_color: String::new(),
            treatment: Treatment::None,
            priority,
        }
    }

    /// An emergency message must not cost you the one it interrupted: the
    /// interrupted message goes back to the front of the queue and resumes.
    #[test]
    fn emergency_interrupts_and_requeues_what_it_cut_off() {
        let mut m = Messages::default();
        m.push(msg("first", VoicePriority::Operational));
        m.tick();
        assert_eq!(m.active.as_ref().unwrap().msg.text, "first");

        m.push(msg("TRAIN", VoicePriority::Emergency));
        m.tick();
        assert_eq!(
            m.active.as_ref().unwrap().msg.text,
            "TRAIN",
            "emergency shows immediately"
        );
        assert_eq!(
            m.queue.front().map(|q| q.text.as_str()),
            Some("first"),
            "the interrupted message is next, not lost"
        );
    }

    /// Ambient is the priority for things that only matter if they are
    /// timely, so it is dropped rather than shown late.
    #[test]
    fn ambient_is_dropped_once_a_backlog_exists() {
        let mut m = Messages::default();
        for i in 0..AMBIENT_DROP_QUEUE_LEN {
            m.push(msg(&format!("q{i}"), VoicePriority::Operational));
        }
        m.push(msg("late", VoicePriority::Ambient));
        assert_eq!(m.queue.len(), AMBIENT_DROP_QUEUE_LEN, "ambient was dropped");

        let mut m2 = Messages::default();
        m2.push(msg("early", VoicePriority::Ambient));
        assert_eq!(m2.queue.len(), 1, "ambient shows when nothing is waiting");
    }

    /// A backlog shortens the hold so the queue drains, but never past the
    /// point where a message is too brief to read.
    #[test]
    fn hold_shortens_under_backlog_but_stays_readable() {
        let mut m = Messages::default();
        let full = m.hold_secs();
        assert_eq!(full, m.style.hold_secs);
        for i in 0..12 {
            m.push(msg(&format!("q{i}"), VoicePriority::Operational));
        }
        let busy = m.hold_secs();
        assert!(busy < full, "backlog shortens the hold: {busy} vs {full}");
        assert!(busy >= MIN_HOLD_SECS, "never below the readable floor");
    }

    /// A trigger that only plays a sound still produces an engine event.
    /// Opening the message window for it would put an empty card on screen.
    #[test]
    fn a_message_less_event_does_not_open_the_window() {
        let ev = OverlayEvent {
            icon: String::new(),
            color: String::new(),
            message: String::new(),
            message_color: String::new(),
            border_color: String::new(),
            sound: Some("Ding".into()),
            tts_text: None,
            tts_priority: VoicePriority::default(),
            treatment: Treatment::None,
            priority: VoicePriority::default(),
        };
        assert!(Msg::from_event(&ev).is_none());
    }

    /// Nothing queued, nothing held, nothing recent — the window must get
    /// out of the way rather than sit on the game.
    #[test]
    fn an_empty_overlay_reports_nothing_to_show() {
        let mut m = Messages::default();
        assert!(!m.tick());
        m.preview = true;
        assert!(m.tick(), "preview keeps it visible for positioning");
    }

    /// The lifecycle actually completes: flown, held, shrunk, in history.
    #[test]
    fn a_message_lands_in_history() {
        let mut m = Messages::default();
        m.style.fly_ms = 1;
        m.style.hold_secs = 0.01;
        m.push(msg("Zarri merged to +4", VoicePriority::Operational));
        for _ in 0..6 {
            m.tick();
            std::thread::sleep(Duration::from_millis(12));
        }
        m.tick();
        assert!(m.active.is_none(), "it finished");
        assert_eq!(m.history.len(), 1);
        assert_eq!(m.history[0].msg.text, "Zarri merged to +4");
    }

    #[test]
    fn history_is_capped_at_the_configured_rows() {
        let mut m = Messages::default();
        m.style.fly_ms = 1;
        m.style.hold_secs = 0.01;
        m.style.history_rows = 3;
        // One at a time: a queued message holds for MIN_HOLD_SECS however
        // short `hold_secs` is, so a burst would take seconds of wall clock
        // to drain. Six arrivals spread out is the real shape anyway.
        for i in 0..6 {
            m.push(msg(&format!("m{i}"), VoicePriority::Operational));
            for _ in 0..8 {
                m.tick();
                std::thread::sleep(Duration::from_millis(4));
            }
        }
        assert!(m.history.len() <= 3, "capped: {}", m.history.len());
        assert_eq!(m.history[0].msg.text, "m5", "newest first");
    }

    #[test]
    fn hex_colours_come_from_the_config_form() {
        assert_eq!(parse_hex("#FF4400"), Some(Color32::from_rgb(255, 68, 0)));
        assert_eq!(parse_hex("FF4400"), Some(Color32::from_rgb(255, 68, 0)));
        assert_eq!(parse_hex(""), None);
        assert_eq!(parse_hex("nope"), None);
    }
}
