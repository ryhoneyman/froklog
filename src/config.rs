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

fn default_overlay_font_size() -> u32 {
    14
}

fn default_overlay_idle_secs() -> u32 {
    6
}

fn default_overlay_max_entries() -> usize {
    8
}

fn default_neg_one_i32() -> i32 {
    -1
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
    /// Font point size for normal overlay entries. Featured entry is 2×. Default 14.
    #[serde(default = "default_overlay_font_size")]
    pub overlay_font_size: u32,
    /// Seconds of inactivity before the overlay auto-hides. Default 6.
    #[serde(default = "default_overlay_idle_secs")]
    pub overlay_idle_secs: u32,
    /// Maximum number of entries kept in the scroll buffer. Default 8.
    #[serde(default = "default_overlay_max_entries")]
    pub overlay_max_entries: usize,
    /// Overlay window X position (pixels from left of screen). Default -1 = auto-centre.
    #[serde(default = "default_neg_one_i32")]
    pub overlay_x: i32,
    /// Overlay window Y position (pixels from top of screen). Default -1 = auto-centre.
    #[serde(default = "default_neg_one_i32")]
    pub overlay_y: i32,
}

impl Config {
    pub fn load() -> Self {
        let path = config_path();
        let Ok(text) = std::fs::read_to_string(&path) else {
            return Self {
                logging_enabled: true,
                ..Default::default()
            };
        };
        toml::from_str(&text).unwrap_or_else(|_| Self {
            logging_enabled: true,
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

    /// Returns true when the config has everything needed to start pushing.
    pub fn is_ready(&self) -> bool {
        self.log_path.is_some() && self.ingest_ws_url().is_some() && self.stream_token.is_some()
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
