/// Persistent configuration stored in %APPDATA%\froklog\config.toml (Windows)
/// or ~/.config/froklog/config.toml (other platforms).
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

fn default_true() -> bool {
    true
}

fn default_overlay_alpha() -> u8 {
    200
}

fn default_overlay_start_font_size() -> u32 {
    10
}

fn default_overlay_max_font_size() -> u32 {
    60
}

fn default_overlay_fly_ms() -> u32 {
    240
}

fn default_overlay_hold_secs() -> f32 {
    2.5
}

fn default_overlay_history_font_size() -> u32 {
    12
}

fn default_overlay_history_idle_secs() -> u32 {
    8
}

fn default_overlay_history_max_entries() -> usize {
    8
}

fn default_overlay_history_width() -> i32 {
    320
}

fn default_neg_one_i32() -> i32 {
    -1
}

fn default_meter_max_rows() -> usize {
    12
}

fn default_meter_idle_secs() -> u32 {
    10
}

fn default_meter_font_size() -> u32 {
    11
}

fn default_meter_width() -> i32 {
    360
}

fn default_sound_volume() -> u8 {
    100
}

fn default_sound_package() -> String {
    "default".to_string()
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

#[derive(Debug, Default, Clone, Serialize, Deserialize)]
pub struct Config {
    /// Path to the EverQuest log file being tailed.
    pub log_path: Option<String>,
    /// Base HTTP URL of the froklog-server, e.g. `http://server:8766`.
    pub server_url: Option<String>,
    /// Secret stream token used to authenticate the ingest WebSocket.
    pub stream_token: Option<String>,
    /// Public stream ID (last 16 hex chars of UUID).
    pub stream_id: Option<String>,
    /// Viewer token embedded in the shareable URL.
    pub view_token: Option<String>,

    /// Game selection, e.g. "Everquest Legends".
    #[serde(default)]
    pub game: Option<String>,
    /// EQ server name extracted from / matching the log filename, e.g. "Test".
    #[serde(default)]
    pub server_name: Option<String>,
    /// Explicit player name (A-Z only); overrides filename extraction.
    #[serde(default)]
    pub player_name: Option<String>,
    /// Optional password required by the froklog-server to create streams.
    /// Left empty for public servers; set by the server operator via FROKLOG_STREAM_PASSWORD.
    #[serde(default)]
    pub stream_password: Option<String>,
    /// Whether to expose a public /player/{server}/{name} URL for this stream.
    #[serde(default)]
    pub public_stream: bool,
    /// Whether the log-tail engine is enabled. Defaults to true.
    #[serde(default = "default_true")]
    pub logging_enabled: bool,
    /// Whether parsed events are pushed to the remote server. Local processing
    /// (DPS meter, triggers, overlays) runs regardless of this setting; it only
    /// gates the network push. Defaults to true.
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

    // ── Overlay settings ──────────────────────────────────────────────────────
    /// Whether the overlay window is visible.
    #[serde(default)]
    pub overlay_enabled: bool,
    /// Window opacity 0–255 (255 = fully opaque). Default 200.
    #[serde(default = "default_overlay_alpha")]
    pub overlay_alpha: u8,
    /// Font family for overlay text. Empty = system default (Segoe UI).
    #[serde(default)]
    pub overlay_font: String,
    /// Font point size at the start of the fly-in animation. Default 10.
    #[serde(default = "default_overlay_start_font_size")]
    pub overlay_start_font_size: u32,
    /// Font point size at the peak / hold of the fly-in animation. Default 60.
    #[serde(default = "default_overlay_max_font_size")]
    pub overlay_max_font_size: u32,
    /// Milliseconds for the fly-in and shrink-out animations. Default 240.
    #[serde(default = "default_overlay_fly_ms")]
    pub overlay_fly_ms: u32,
    /// Seconds the message holds at peak size before shrinking away. Default 2.5.
    #[serde(default = "default_overlay_hold_secs")]
    pub overlay_hold_secs: f32,
    /// Overlay window X position (pixels from left of screen). Default -1 = auto-centre.
    #[serde(default = "default_neg_one_i32")]
    pub overlay_x: i32,
    /// Overlay window Y position (pixels from top of screen). Default -1 = auto-centre.
    #[serde(default = "default_neg_one_i32")]
    pub overlay_y: i32,
    /// When true, the alert overlay window is click-through (locked in place).
    #[serde(default)]
    pub overlay_locked: bool,

    // ── Overlay history settings ──────────────────────────────────────────────
    /// Whether the history overlay window can be shown at all, independent of
    /// the alert overlay's own enable toggle. Defaults to true.
    #[serde(default = "default_true")]
    pub overlay_history_enabled: bool,
    /// Font point size for history rows. Default 12.
    #[serde(default = "default_overlay_history_font_size")]
    pub overlay_history_font_size: u32,
    /// Seconds of inactivity before the history overlay auto-hides. 0 = never. Default 8.
    #[serde(default = "default_overlay_history_idle_secs")]
    pub overlay_history_idle_secs: u32,
    /// Maximum number of rows kept in the history overlay. Default 8.
    #[serde(default = "default_overlay_history_max_entries")]
    pub overlay_history_max_entries: usize,
    /// History overlay window X position. Default -1 = auto-position.
    #[serde(default = "default_neg_one_i32")]
    pub overlay_history_x: i32,
    /// History overlay window Y position. Default -1 = auto-position.
    #[serde(default = "default_neg_one_i32")]
    pub overlay_history_y: i32,
    /// History overlay window width in pixels. Default 320.
    #[serde(default = "default_overlay_history_width")]
    pub overlay_history_width: i32,
    /// When true, the history overlay window is click-through (locked in place).
    #[serde(default)]
    pub overlay_history_locked: bool,

    // ── DPS meter settings ──────────────────────────────────────────────────────
    /// Whether the live DPS/Tank/Heal meter window is visible.
    #[serde(default)]
    pub meter_enabled: bool,
    /// Meter window X position (pixels from left of screen). Default -1 = auto-position.
    #[serde(default = "default_neg_one_i32")]
    pub meter_x: i32,
    /// Meter window Y position (pixels from top of screen). Default -1 = auto-position.
    #[serde(default = "default_neg_one_i32")]
    pub meter_y: i32,
    /// When true, the meter window is click-through (locked in place).
    #[serde(default)]
    pub meter_locked: bool,
    /// Maximum number of ranked rows shown per tab. Default 12.
    #[serde(default = "default_meter_max_rows")]
    pub meter_max_rows: usize,
    /// Seconds of inactivity (no new combat events for the active mob) before the
    /// meter auto-hides. 0 = never hide. Default 10.
    #[serde(default = "default_meter_idle_secs")]
    pub meter_idle_secs: u32,
    /// Font point size for meter rows. Default 11.
    #[serde(default = "default_meter_font_size")]
    pub meter_font_size: u32,
    /// Meter window width in pixels, user-resizable via the left/right edges.
    /// Height stays content-driven (row count). Default 360.
    #[serde(default = "default_meter_width")]
    pub meter_width: i32,

    // ── TTS / Voice settings ──────────────────────────────────────────────────
    /// Whether Text-to-Speech is enabled globally.
    #[serde(default)]
    pub tts_enabled: bool,
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
    /// SAPI voice token key name (e.g. `TTS_MS_EN-US_DAVID_11.0`).  Empty = system default.
    #[serde(default)]
    pub tts_voice: String,
}

impl Config {
    pub fn load() -> Self {
        let path = config_path();
        let Ok(text) = std::fs::read_to_string(&path) else {
            return Self {
                logging_enabled: true,
                remote_logging_enabled: true,
                overlay_history_enabled: true,
                sound_enabled: true,
                sound_volume: 100,
                sound_package: default_sound_package(),
                ..Default::default()
            };
        };
        toml::from_str(&text).unwrap_or_else(|_| Self {
            logging_enabled: true,
            remote_logging_enabled: true,
            overlay_history_enabled: true,
            sound_enabled: true,
            sound_volume: 100,
            sound_package: default_sound_package(),
            ..Default::default()
        })
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
        if self.public_stream {
            let game = self.game.as_deref()?;
            let server = self
                .server_name
                .clone()
                .or_else(|| self.server_name_from_log())?;
            let player = self.effective_player_name();
            if player.is_empty() {
                return None;
            }
            Some(format!("{base}/player/{game}/{server}/{player}"))
        } else {
            self.viewer_url()
        }
    }

    /// Returns true when there's enough config to run the local engine (tailer,
    /// parser, triggers, DPS meter) regardless of remote server availability.
    pub fn local_ready(&self) -> bool {
        self.log_path.is_some()
    }

    /// Returns true when the config has everything needed to start pushing.
    pub fn is_ready(&self) -> bool {
        self.log_path.is_some() && self.ingest_ws_url().is_some() && self.stream_token.is_some()
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

    /// Best-effort player name: explicit field first, then derived from log filename.
    pub fn effective_player_name(&self) -> String {
        if let Some(ref name) = self.player_name {
            if !name.is_empty() {
                return name.clone();
            }
        }
        self.log_path
            .as_deref()
            .and_then(|p| {
                let stem = std::path::Path::new(p).file_stem()?.to_str()?;
                let parts: Vec<&str> = stem.split('_').collect();
                if parts.len() >= 3 {
                    Some(parts[1].to_string())
                } else {
                    None
                }
            })
            .unwrap_or_default()
    }

    /// Server name derived from log filename (e.g. "Test" from eqlog_Name_Test.txt).
    pub fn server_name_from_log(&self) -> Option<String> {
        let p = self.log_path.as_deref()?;
        let stem = std::path::Path::new(p).file_stem()?.to_str()?;
        // eqlog_{player}_{server}.txt  →  parts[2] is the server
        let parts: Vec<&str> = stem.split('_').collect();
        if parts.len() >= 3 {
            Some(parts[2..].join("_"))
        } else {
            None
        }
    }
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
