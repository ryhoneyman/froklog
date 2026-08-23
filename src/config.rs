/// Persistent configuration stored in %APPDATA%\froklog\config.toml (Windows)
/// or ~/.config/froklog/config.toml (other platforms).
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

fn default_true() -> bool {
    true
}

fn default_overlay_alpha() -> u8 {
    255
}

fn default_overlay_start_font_size() -> u32 {
    6
}

fn default_overlay_max_font_size() -> u32 {
    60
}

fn default_overlay_fly_ms() -> u32 {
    1000
}

fn default_overlay_hold_secs() -> f32 {
    2.5
}

fn default_overlay_history_font_size() -> u32 {
    12
}

fn default_overlay_history_idle_secs() -> u32 {
    0
}

fn default_overlay_history_max_entries() -> usize {
    8
}

fn default_overlay_history_width() -> i32 {
    0
}

// ── Combined Alert + History overlay settings ───────────────────────────────
// Mirror the standalone alert/history defaults above — the merged overlay
// used to just read those shared fields directly, but it needs its own
// independent copies (see `Config::overlay_merged_start_font_size` etc.'s
// doc comments).

fn default_overlay_merged_start_font_size() -> u32 {
    6
}

fn default_overlay_merged_max_font_size() -> u32 {
    60
}

fn default_overlay_merged_fly_ms() -> u32 {
    1000
}

fn default_overlay_merged_hold_secs() -> f32 {
    2.5
}

fn default_overlay_merged_alpha() -> u8 {
    255
}

fn default_overlay_merged_history_font_size() -> u32 {
    12
}

fn default_overlay_merged_history_idle_secs() -> u32 {
    0
}

fn default_overlay_merged_history_max_entries() -> usize {
    8
}

fn default_neg_one_i32() -> i32 {
    -1
}

// ── Notice overlay settings ─────────────────────────────────────────────────
// A second, lighter overlay window alongside Alert: fade/slide/fly-in only
// (no Glow/Vibrate/Pulse hold-phase treatment), a single standard font size
// instead of Alert's animated start->max growth. Runs independently of
// `alert_style` — Notice and Alert are meant to be shown together, not as
// alternate presentations of the same thing.

fn default_overlay_notice_font_size() -> u32 {
    20
}

fn default_overlay_notice_transition_ms() -> u32 {
    400
}

fn default_overlay_notice_hold_secs() -> f32 {
    3.0
}

fn default_overlay_notice_alpha() -> u8 {
    255
}

fn default_overlay_notice_window() -> OverlayWindowConfig {
    OverlayWindowConfig {
        enabled: false,
        locked: false,
        x: -1,
        y: -1,
    }
}

fn default_overlay_alert_window() -> OverlayWindowConfig {
    OverlayWindowConfig {
        enabled: false,
        locked: false,
        x: -1,
        y: -1,
    }
}

fn default_overlay_history_window() -> OverlayWindowConfig {
    OverlayWindowConfig {
        enabled: true,
        locked: false,
        x: -1,
        y: -1,
    }
}

fn default_overlay_meter_window() -> OverlayWindowConfig {
    OverlayWindowConfig {
        enabled: false,
        locked: false,
        x: -1,
        y: -1,
    }
}

fn default_overlay_merged_window() -> OverlayWindowConfig {
    OverlayWindowConfig {
        enabled: false,
        locked: false,
        x: -1,
        y: -1,
    }
}

/// Seconds of no new log-file lines before every overlay window is
/// force-hidden, overriding per-overlay Idle Hide settings (even "Never")
/// and Show All Windows alike. 0 = Never (feature disabled). Default 30.
fn default_overlay_hide_inactive_secs() -> u32 {
    30
}

fn default_meter_max_rows() -> usize {
    8
}

fn default_meter_idle_secs() -> u32 {
    0
}

fn default_meter_font_size() -> u32 {
    12
}

fn default_meter_width() -> i32 {
    360
}

fn default_sound_volume() -> u8 {
    100
}

fn default_tts_volume() -> u8 {
    100
}

fn default_sound_package() -> String {
    "default".to_string()
}

/// The one voice bundled with every install (see
/// `assets::bundled_voices_dir()`) — low-quality tier, chosen so TTS works
/// out of the box with no first-run download.
fn default_tts_voice() -> String {
    "en_US-amy-low".to_string()
}

/// Voice speed multiplier for TTS playback.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
#[serde(rename_all = "snake_case")]
pub enum TtsSpeed {
    /// 1.0× — natural speech rate.
    #[default]
    Normal,
    /// 1.2×
    Fast,
    /// 1.5×
    Faster,
    /// 2.0×
    Fastest,
}

impl TtsSpeed {
    pub fn multiplier(&self) -> f32 {
        match self {
            Self::Normal => 1.0,
            Self::Fast => 1.2,
            Self::Faster => 1.5,
            Self::Fastest => 2.0,
        }
    }
}

/// How concurrent TTS alerts are handled when audio is already playing.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
#[serde(rename_all = "snake_case")]
pub enum TtsAudioMode {
    /// Emergency alerts cut in immediately; Operational alerts queue; Ambient alerts are
    /// suppressed while any audio is playing.
    #[default]
    SmartPriority,
    /// Every alert is queued and spoken in order, regardless of priority.
    QueueAll,
    /// Every new alert interrupts whatever is currently playing.
    InterruptConstantly,
}

/// A named EverQuest log file the user has configured — one game
/// install/character's `eqlog_*.txt`. Multiple profiles may exist (e.g. one
/// per EQ variant or character); exactly one is watched at a time, chosen by
/// `Config::log_watch_mode`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct LogProfile {
    pub name: String,
    pub path: String,
    /// Game id, e.g. "eql" for Everquest Legends — see settings_window.rs's
    /// `GAME_IDS`/`GAMES`.
    #[serde(default)]
    pub game: Option<String>,
    /// Explicit server name; overrides filename extraction when set.
    #[serde(default)]
    pub server: Option<String>,
    /// Explicit player name; overrides filename extraction when set.
    #[serde(default)]
    pub player: Option<String>,
    /// Whether this profile exposes a public /player/{game}/{server}/{player}
    /// URL when it's the active stream. Defaults to false (opt-in).
    #[serde(default)]
    pub public_stream: bool,
}

impl LogProfile {
    /// Best-effort player name: explicit field first, then derived from this
    /// profile's log filename.
    pub fn effective_player(&self) -> String {
        self.player
            .clone()
            .filter(|s| !s.is_empty())
            .or_else(|| player_name_from_path(&self.path))
            .unwrap_or_default()
    }

    /// Best-effort server name: explicit field first, then derived from this
    /// profile's log filename (e.g. "Test" from eqlog_Name_Test.txt).
    pub fn effective_server(&self) -> String {
        self.server
            .clone()
            .filter(|s| !s.is_empty())
            .or_else(|| server_name_from_path(&self.path))
            .unwrap_or_default()
    }
}

/// Which log profile is actively tailed.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
#[serde(rename_all = "snake_case")]
pub enum LogWatchMode {
    /// Watch whichever configured profile's file was written to most recently.
    #[default]
    Auto,
    /// Always watch the named profile, regardless of other profiles' activity.
    Pinned(String),
}

/// Shared chrome for an overlay window: whether it's shown, whether it's
/// click-through (locked in place), and its last dragged position. One of
/// these lives per [`crate::overlay_registry::OverlayKind`] on `Config`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct OverlayWindowConfig {
    #[serde(default)]
    pub enabled: bool,
    #[serde(default)]
    pub locked: bool,
    /// -1 = auto-position.
    #[serde(default = "default_neg_one_i32")]
    pub x: i32,
    /// -1 = auto-position.
    #[serde(default = "default_neg_one_i32")]
    pub y: i32,
}

impl Default for OverlayWindowConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            locked: false,
            x: -1,
            y: -1,
        }
    }
}

/// Which alert presentation is active.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Default)]
#[serde(rename_all = "snake_case")]
pub enum AlertStyle {
    /// A standalone flying alert window plus a separate history window.
    #[default]
    Separate,
    /// One combined window where alerts land directly at the top of the
    /// history list instead of flying in a window of their own.
    Merged,
}

/// Entrance/exit animation for the Notice overlay. Unlike Alert, Notice has
/// no hold-phase treatment (no Glow/Vibrate/Pulse) — this is the only visual
/// variation it offers.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Default)]
#[serde(rename_all = "snake_case")]
pub enum NoticeTransition {
    /// Opacity only, no movement.
    #[default]
    Fade,
    /// Rises in from below while fading.
    Slide,
    /// Enters from the side while fading.
    Fly,
}

#[derive(Debug, Default, Clone, Serialize, Deserialize)]
pub struct Config {
    /// Legacy single-logfile path. Only read once, by `load()`'s migration
    /// into `log_profiles`/`log_watch_mode` below — nothing writes to this
    /// field anymore. Kept so old config.toml files still load.
    #[serde(default)]
    pub log_path: Option<String>,
    /// Configured log files, each a distinct EQ install/character. Exactly
    /// one is watched at a time — see `log_watch_mode` and
    /// `resolve_active_log_path()`.
    #[serde(default)]
    pub log_profiles: Vec<LogProfile>,
    /// Which `log_profiles` entry is currently watched.
    #[serde(default)]
    pub log_watch_mode: LogWatchMode,
    /// Base HTTP URL of the froklog-server, e.g. `http://server:8766`.
    pub server_url: Option<String>,
    /// Secret stream token used to authenticate the ingest WebSocket.
    pub stream_token: Option<String>,
    /// Public stream ID (last 16 hex chars of UUID).
    pub stream_id: Option<String>,
    /// Viewer token embedded in the shareable URL.
    pub view_token: Option<String>,

    /// Optional password required by the froklog-server to create streams.
    /// Left empty for public servers; set by the server operator via FROKLOG_STREAM_PASSWORD.
    #[serde(default)]
    pub stream_password: Option<String>,
    /// Legacy global toggle, superseded by the per-profile
    /// `LogProfile::public_stream`. Only read once, by `load()`'s migration
    /// into `log_profiles[].public_stream` below — nothing writes to this
    /// field anymore. Kept so old config.toml files still load.
    #[serde(default)]
    pub public_stream: bool,
    /// Whether the log-tail engine is enabled. Defaults to true.
    #[serde(default = "default_true")]
    pub logging_enabled: bool,
    /// Whether parsed events are pushed to the remote server. Local processing
    /// (DPS meter, triggers, overlays) runs regardless of this setting; it only
    /// gates the network push. `#[serde(default)]` here is for configs saved
    /// before this field existed, when remote push always ran — those users
    /// keep getting `true` on load. Fresh installs get `false` from
    /// `Config::fresh()` instead.
    #[serde(default = "default_true")]
    pub remote_logging_enabled: bool,

    // ── Sound settings ──────────────────────────────────────────────────────
    /// Whether trigger "Play Sound" actions play at all. Defaults to true.
    #[serde(default = "default_true")]
    pub sound_enabled: bool,
    /// Overall volume applied to trigger "Play Sound" actions, 0-100. Default 100.
    #[serde(default = "default_sound_volume")]
    pub sound_volume: u8,
    /// Name of the currently active sound package (a subfolder of `sounds/`
    /// mapping label names to sound files). Default "default".
    #[serde(default = "default_sound_package")]
    pub sound_package: String,

    // ── Overlay window chrome (visibility/lock/position) ───────────────────────
    /// Standalone alert overlay window — used when `alert_style` is `Separate`.
    #[serde(default = "default_overlay_alert_window")]
    pub overlay_alert: OverlayWindowConfig,
    /// Standalone history overlay window — used when `alert_style` is `Separate`.
    #[serde(default = "default_overlay_history_window")]
    pub overlay_history: OverlayWindowConfig,
    /// DPS/Tank/Heal meter window.
    #[serde(default = "default_overlay_meter_window")]
    pub overlay_meter: OverlayWindowConfig,
    /// Combined alert+history overlay window — used when `alert_style` is `Merged`.
    #[serde(default = "default_overlay_merged_window")]
    pub overlay_merged: OverlayWindowConfig,
    /// Notice overlay window — runs independently of `alert_style`.
    #[serde(default = "default_overlay_notice_window")]
    pub overlay_notice: OverlayWindowConfig,
    /// Which alert presentation is active — see [`AlertStyle`].
    #[serde(default)]
    pub alert_style: AlertStyle,
    /// Seconds of no new log-file lines before every overlay window is
    /// force-hidden — detects the game not running/logging (closed,
    /// crashed, character select) so overlays don't linger on screen.
    /// Overrides per-overlay Idle Hide settings (even "Never") and Show
    /// All Windows alike. 0 = Never (feature disabled). Default 30.
    #[serde(default = "default_overlay_hide_inactive_secs")]
    pub overlay_hide_inactive_secs: u32,

    // ── Overlay settings ──────────────────────────────────────────────────────
    /// Window opacity 0–255 (255 = fully opaque). Default 255.
    #[serde(default = "default_overlay_alpha")]
    pub overlay_alpha: u8,
    /// Font family for overlay text. Empty = system default (Segoe UI).
    #[serde(default)]
    pub overlay_font: String,
    /// Font point size at the start of the fly-in animation. Default 6.
    #[serde(default = "default_overlay_start_font_size")]
    pub overlay_start_font_size: u32,
    /// Font point size at the peak / hold of the fly-in animation. Default 60.
    #[serde(default = "default_overlay_max_font_size")]
    pub overlay_max_font_size: u32,
    /// Milliseconds for the fly-in and shrink-out animations. Default 1000.
    #[serde(default = "default_overlay_fly_ms")]
    pub overlay_fly_ms: u32,
    /// Seconds the message holds at peak size before shrinking away. Default 2.5.
    #[serde(default = "default_overlay_hold_secs")]
    pub overlay_hold_secs: f32,

    // ── Overlay history settings ────────────────────────────────────────────────
    /// Font point size for history rows. Default 12.
    #[serde(default = "default_overlay_history_font_size")]
    pub overlay_history_font_size: u32,
    /// Seconds of inactivity before the history overlay auto-hides. 0 = never. Default 0.
    #[serde(default = "default_overlay_history_idle_secs")]
    pub overlay_history_idle_secs: u32,
    /// Maximum number of rows kept in the history overlay. Default 8.
    #[serde(default = "default_overlay_history_max_entries")]
    pub overlay_history_max_entries: usize,
    /// Minutes a history row is kept before aging out, regardless of
    /// `overlay_history_max_entries`. 0 = never. Default 0.
    #[serde(default)]
    pub overlay_history_max_age_mins: u32,
    /// History overlay window width in pixels. Default 0.
    #[serde(default = "default_overlay_history_width")]
    pub overlay_history_width: i32,

    // ── Combined Alert + History overlay settings ───────────────────────────────
    // Independent of the standalone `overlay_*`/`overlay_history_*` fields above
    // — the Combined overlay used to just read those directly, but that meant
    // its appearance couldn't be tuned separately from the Separate-mode
    // windows even though only one of the two is ever visible at a time.
    /// Font point size at the start of the incoming-alert fly-in animation. Default 6.
    #[serde(default = "default_overlay_merged_start_font_size")]
    pub overlay_merged_start_font_size: u32,
    /// Font point size at the peak/hold of the incoming-alert animation. Default 60.
    #[serde(default = "default_overlay_merged_max_font_size")]
    pub overlay_merged_max_font_size: u32,
    /// Milliseconds for the incoming-alert fly-in/shrink-out animations. Default 1000.
    #[serde(default = "default_overlay_merged_fly_ms")]
    pub overlay_merged_fly_ms: u32,
    /// Seconds the incoming alert holds at peak size before shrinking away. Default 2.5.
    #[serde(default = "default_overlay_merged_hold_secs")]
    pub overlay_merged_hold_secs: f32,
    /// Window opacity 0-255 (255 = fully opaque). Default 255.
    #[serde(default = "default_overlay_merged_alpha")]
    pub overlay_merged_alpha: u8,
    /// Font point size for history rows. Default 12.
    #[serde(default = "default_overlay_merged_history_font_size")]
    pub overlay_merged_history_font_size: u32,
    /// Seconds of inactivity before the combined overlay auto-hides. 0 = never. Default 0.
    #[serde(default = "default_overlay_merged_history_idle_secs")]
    pub overlay_merged_history_idle_secs: u32,
    /// Maximum number of history rows kept. Default 8.
    #[serde(default = "default_overlay_merged_history_max_entries")]
    pub overlay_merged_history_max_entries: usize,
    /// Minutes a history row is kept before aging out, regardless of
    /// `overlay_merged_history_max_entries`. 0 = never. Default 0.
    #[serde(default)]
    pub overlay_merged_history_max_age_mins: u32,

    // ── Notice overlay settings ─────────────────────────────────────────────────
    /// Fixed font point size — Notice has no animated start->max growth like
    /// Alert. Default 20.
    #[serde(default = "default_overlay_notice_font_size")]
    pub overlay_notice_font_size: u32,
    /// Entrance/exit animation style. Default `Fade`.
    #[serde(default)]
    pub overlay_notice_transition: NoticeTransition,
    /// Milliseconds for the entrance/exit transition. Default 400.
    #[serde(default = "default_overlay_notice_transition_ms")]
    pub overlay_notice_transition_ms: u32,
    /// Seconds the message holds before the exit transition. Default 3.0.
    #[serde(default = "default_overlay_notice_hold_secs")]
    pub overlay_notice_hold_secs: f32,
    /// Window opacity 0-255 (255 = fully opaque). Default 255.
    #[serde(default = "default_overlay_notice_alpha")]
    pub overlay_notice_alpha: u8,

    // ── DPS meter settings ──────────────────────────────────────────────────────
    /// Maximum number of ranked rows shown per tab. Default 8.
    #[serde(default = "default_meter_max_rows")]
    pub meter_max_rows: usize,
    /// Seconds of inactivity (no new combat events for the active mob) before the
    /// meter auto-hides. 0 = never hide. Default 0.
    #[serde(default = "default_meter_idle_secs")]
    pub meter_idle_secs: u32,
    /// Font point size for meter rows. Default 12.
    #[serde(default = "default_meter_font_size")]
    pub meter_font_size: u32,
    /// Meter window width in pixels, user-resizable via the left/right edges.
    /// Height stays content-driven (row count). Default 360.
    #[serde(default = "default_meter_width")]
    pub meter_width: i32,
    /// DPS meter share bars: proportional class-colored fill + % column per
    /// row (ported from the Linux interim client). Off = classic trimmed
    /// rows.
    #[serde(default)]
    pub meter_share_bars: bool,
    /// Per-column meter widths in px, dragged via the header dividers.
    /// 0 = auto (historical 52px; bar = 5x row font size).
    #[serde(default)]
    pub meter_amount_w: i32,
    #[serde(default)]
    pub meter_rate_w: i32,
    #[serde(default)]
    pub meter_bar_w: i32,
    #[serde(default)]
    pub meter_sec_w: i32,

    // ── TTS / Voice settings ──────────────────────────────────────────────────
    /// Whether Text-to-Speech is enabled globally.
    #[serde(default = "default_true")]
    pub tts_enabled: bool,
    /// Overall volume (0-100) applied to spoken TTS alerts, independent of
    /// `sound_volume` — only affects the voice, not `play_sound`/triggers'
    /// sound-effect actions.
    #[serde(default = "default_tts_volume")]
    pub tts_volume: u8,
    /// Playback speed for TTS speech.
    #[serde(default)]
    pub tts_speed: TtsSpeed,
    /// How concurrent TTS alerts are handled when audio is already playing.
    #[serde(default)]
    pub tts_audio_mode: TtsAudioMode,
    /// Whether Emergency priority voice alerts are spoken.
    #[serde(default = "default_true")]
    pub tts_read_emergency: bool,
    /// Whether Operational priority voice alerts are spoken.
    #[serde(default = "default_true")]
    pub tts_read_operational: bool,
    /// Whether Ambient priority voice alerts are spoken.
    #[serde(default = "default_true")]
    pub tts_read_ambient: bool,
    /// Piper voice name (e.g. `en_US-amy-low`) — the `.onnx`/`.onnx.json`
    /// file stem in `assets::bundled_voices_dir()` or
    /// `assets::downloaded_voices_dir()`. Defaults to the bundled voice so
    /// TTS works out of the box. Configs saved before this replaced the
    /// `tts` crate stored an OS voice id here instead (SAPI/speech-
    /// dispatcher) — there's no sane mapping from that to a piper voice
    /// name, so this is a clean break, not a migration: an old id here
    /// just won't resolve to any installed voice file and `PiperEngine`
    /// silently fails to load until the user picks one again in Settings.
    #[serde(default = "default_tts_voice")]
    pub tts_voice: String,
}

impl Config {
    /// The defaults used when no config exists yet. Single source of truth —
    /// `#[derive(Default)]` alone disagrees with the serde defaults (e.g.
    /// `logging_enabled` would come out false), so always construct fresh
    /// configs through here.
    fn fresh() -> Self {
        Self {
            logging_enabled: true,
            remote_logging_enabled: false,
            overlay_alert: default_overlay_alert_window(),
            overlay_history: default_overlay_history_window(),
            overlay_meter: default_overlay_meter_window(),
            overlay_merged: default_overlay_merged_window(),
            overlay_notice: default_overlay_notice_window(),
            sound_enabled: true,
            sound_volume: 100,
            sound_package: default_sound_package(),
            tts_enabled: true,
            tts_volume: 100,
            tts_voice: default_tts_voice(),
            overlay_alpha: default_overlay_alpha(),
            overlay_start_font_size: default_overlay_start_font_size(),
            overlay_max_font_size: default_overlay_max_font_size(),
            overlay_fly_ms: default_overlay_fly_ms(),
            overlay_hold_secs: default_overlay_hold_secs(),
            overlay_history_font_size: default_overlay_history_font_size(),
            overlay_history_idle_secs: default_overlay_history_idle_secs(),
            overlay_history_max_entries: default_overlay_history_max_entries(),
            overlay_history_max_age_mins: 0,
            overlay_history_width: default_overlay_history_width(),
            overlay_merged_start_font_size: default_overlay_merged_start_font_size(),
            overlay_merged_max_font_size: default_overlay_merged_max_font_size(),
            overlay_merged_fly_ms: default_overlay_merged_fly_ms(),
            overlay_merged_hold_secs: default_overlay_merged_hold_secs(),
            overlay_merged_alpha: default_overlay_merged_alpha(),
            overlay_merged_history_font_size: default_overlay_merged_history_font_size(),
            overlay_merged_history_idle_secs: default_overlay_merged_history_idle_secs(),
            overlay_merged_history_max_entries: default_overlay_merged_history_max_entries(),
            overlay_merged_history_max_age_mins: 0,
            overlay_notice_font_size: default_overlay_notice_font_size(),
            overlay_notice_transition_ms: default_overlay_notice_transition_ms(),
            overlay_notice_hold_secs: default_overlay_notice_hold_secs(),
            overlay_notice_alpha: default_overlay_notice_alpha(),
            meter_max_rows: default_meter_max_rows(),
            meter_idle_secs: default_meter_idle_secs(),
            meter_font_size: default_meter_font_size(),
            meter_width: default_meter_width(),
            meter_share_bars: false,
            meter_amount_w: 0,
            meter_rate_w: 0,
            meter_bar_w: 0,
            meter_sec_w: 0,
            ..Default::default()
        }
    }

    pub fn load() -> Self {
        let path = config_path();
        let Ok(text) = std::fs::read_to_string(&path) else {
            return Self::fresh();
        };
        match toml::from_str(&text) {
            Ok(cfg) => {
                let cfg: Self = cfg;
                let cfg = if cfg.log_profiles.is_empty() && cfg.log_path.is_some() {
                    let migrated = cfg.migrate_legacy_log_path();
                    migrated.save();
                    migrated
                } else {
                    cfg
                };
                if cfg.public_stream {
                    let migrated = cfg.migrate_legacy_public_stream();
                    migrated.save();
                    migrated
                } else {
                    cfg
                }
            }
            Err(e) => {
                // A corrupt config used to be silently replaced with defaults
                // — server URL, tokens and layout gone, then saved over.
                // Preserve the broken file and tell the user.
                let backup = path.with_extension("toml.broken");
                let _ = std::fs::copy(&path, &backup);
                eprintln!(
                    "froklog: config parse error in {}: {e}\nfroklog: the unmodified file was backed up to {} — fix it to recover your settings/tokens",
                    path.display(),
                    backup.display()
                );
                Self::fresh()
            }
        }
    }

    pub fn save(&self) {
        let path = config_path();
        if let Some(parent) = path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        if let Ok(text) = toml::to_string_pretty(self) {
            let _ = std::fs::write(path, text);
        }
    }

    /// Returns the WebSocket ingest URL, e.g. `ws://server:8766/ingest/<id>`.
    pub fn ingest_ws_url(&self) -> Option<String> {
        let base = self.server_url.as_deref()?;
        let id = self.stream_id.as_deref()?;
        let ws_base = base
            .replacen("https://", "wss://", 1)
            .replacen("http://", "ws://", 1);
        Some(format!("{}/ingest/{id}", ws_base.trim_end_matches('/')))
    }

    /// Returns the full shareable viewer URL.
    pub fn viewer_url(&self) -> Option<String> {
        let base = self.server_url.as_deref()?;
        let id = self.stream_id.as_deref()?;
        let vtok = self.view_token.as_deref()?;
        Some(format!(
            "{}/stream/{id}?vtok={vtok}",
            base.trim_end_matches('/')
        ))
    }

    /// Returns the appropriate stream URL to share.
    /// Public streams use the /player/{game}/{server}/{player} route;
    /// private streams use /stream/{id}?vtok={token}.
    pub fn stream_url(&self) -> Option<String> {
        let base = self.server_url.as_deref()?.trim_end_matches('/');
        let profile = self.resolve_active_profile();
        if profile.is_some_and(|p| p.public_stream) {
            let profile = profile?;
            let game = profile.game.as_deref()?;
            let server = profile.effective_server();
            let player = profile.effective_player();
            if server.is_empty() || player.is_empty() {
                return None;
            }
            Some(format!("{base}/player/{game}/{server}/{player}"))
        } else {
            self.viewer_url()
        }
    }

    /// Returns true when there's enough config to run the local engine (tailer,
    /// parser, triggers, DPS meter) regardless of remote server availability.
    /// Deliberately O(1) — no filesystem access — so callers may poll this
    /// as often as they like.
    pub fn local_ready(&self) -> bool {
        !self.log_profiles.is_empty()
    }

    /// Returns true when the config has everything needed to start pushing.
    pub fn is_ready(&self) -> bool {
        self.local_ready() && self.ingest_ws_url().is_some() && self.stream_token.is_some()
    }

    /// Returns true when remote push should actually run: the user hasn't
    /// disabled it and the server credentials are present.
    pub fn remote_ready(&self) -> bool {
        self.remote_logging_enabled && self.is_ready()
    }

    /// Returns true when stream credentials have been obtained from a server.
    pub fn is_registered(&self) -> bool {
        self.stream_id.is_some() && self.stream_token.is_some() && self.view_token.is_some()
    }

    /// Resolves which configured log file is currently the active one. Thin
    /// wrapper over `resolve_active_profile()` for callers that only need
    /// the path.
    pub fn resolve_active_log_path(&self) -> Option<String> {
        self.resolve_active_profile().map(|p| p.path.clone())
    }

    /// Resolves which configured log profile is currently active. In Auto
    /// mode this stats every profile's path to find the most recently
    /// written one — only call this at engine start or from a coarse
    /// background poll, never in a tight loop.
    pub fn resolve_active_profile(&self) -> Option<&LogProfile> {
        match &self.log_watch_mode {
            LogWatchMode::Pinned(name) => self
                .log_profiles
                .iter()
                .find(|p| &p.name == name)
                .or_else(|| self.log_profiles.first()),
            LogWatchMode::Auto => {
                let newest = self
                    .log_profiles
                    .iter()
                    .filter_map(|p| {
                        let modified = std::fs::metadata(&p.path).ok()?.modified().ok()?;
                        Some((modified, p))
                    })
                    .max_by_key(|(t, _)| *t)
                    .map(|(_, p)| p);
                // No file currently stats successfully (e.g. not written yet) —
                // fall back to the first profile so run_engine_once still
                // starts a tailer, which retries a missing file internally
                // rather than the engine bailing out.
                newest.or_else(|| self.log_profiles.first())
            }
        }
    }

    /// One-time migration of the legacy single-`log_path` field into a
    /// `Pinned` log profile. Only called from `load()` when `log_profiles`
    /// is empty and a legacy path is present.
    fn migrate_legacy_log_path(&self) -> Self {
        let mut migrated = self.clone();
        let Some(path) = migrated.log_path.take() else {
            return migrated;
        };
        let player = player_name_from_path(&path);
        let server = server_name_from_path(&path);
        let name = match (&player, &server) {
            (Some(p), Some(s)) => format!("{p}  ({s})"),
            _ => std::path::Path::new(&path)
                .file_name()
                .and_then(|f| f.to_str())
                .unwrap_or("Log Profile")
                .to_string(),
        };
        migrated.log_watch_mode = LogWatchMode::Pinned(name.clone());
        migrated.log_profiles.push(LogProfile {
            name,
            path,
            game: Some("eql".to_string()),
            server,
            player,
            public_stream: false,
        });
        migrated
    }

    /// One-time migration of the legacy global `public_stream` toggle into
    /// every configured profile's own `LogProfile::public_stream`. Only
    /// called from `load()` when the legacy flag is set — it applied to
    /// whichever profile was active, so turning it on everywhere preserves
    /// prior behavior regardless of which profile ends up active next.
    fn migrate_legacy_public_stream(&self) -> Self {
        let mut migrated = self.clone();
        for profile in &mut migrated.log_profiles {
            profile.public_stream = true;
        }
        migrated.public_stream = false;
        migrated
    }
}

/// Derives a player name from an `eqlog_{player}_{server}.txt`-style filename.
pub(crate) fn player_name_from_path(path: &str) -> Option<String> {
    let stem = std::path::Path::new(path).file_stem()?.to_str()?;
    let parts: Vec<&str> = stem.split('_').collect();
    if parts.len() >= 3 {
        Some(parts[1].to_string())
    } else {
        None
    }
}

/// Derives a server name from an `eqlog_{player}_{server}.txt`-style filename.
pub(crate) fn server_name_from_path(path: &str) -> Option<String> {
    let stem = std::path::Path::new(path).file_stem()?.to_str()?;
    // eqlog_{player}_{server}.txt  →  parts[2..] is the server
    let parts: Vec<&str> = stem.split('_').collect();
    if parts.len() >= 3 {
        Some(parts[2..].join("_"))
    } else {
        None
    }
}

/// Sidecar cache of last-seen player classes (see overlay_dps's class
/// cache) — same directory as config.toml. Tray-only, like its caller.
#[cfg(feature = "tray")]
pub(crate) fn classes_cache_path() -> PathBuf {
    config_path().with_file_name("classes-cache.toml")
}

fn config_path() -> PathBuf {
    #[cfg(target_os = "windows")]
    {
        let appdata = std::env::var("APPDATA").unwrap_or_else(|_| ".".into());
        PathBuf::from(appdata).join("froklog").join("config.toml")
    }
    #[cfg(not(target_os = "windows"))]
    {
        let home = std::env::var("HOME").unwrap_or_else(|_| ".".into());
        PathBuf::from(home)
            .join(".config")
            .join("froklog")
            .join("config.toml")
    }
}
