/// Persistent configuration stored in %APPDATA%\froklog\config.toml (Windows)
/// or ~/.config/froklog/config.toml (other platforms).
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

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
}

impl Config {
    pub fn load() -> Self {
        let path = config_path();
        let Ok(text) = std::fs::read_to_string(&path) else {
            return Self::default();
        };
        toml::from_str(&text).unwrap_or_default()
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
        // Convert http(s):// to ws(s)://
        let ws_base = base
            .replacen("https://", "wss://", 1)
            .replacen("http://", "ws://", 1);
        Some(format!("{ws_base}/ingest/{id}"))
    }

    /// Returns the full shareable viewer URL.
    pub fn viewer_url(&self) -> Option<String> {
        let base = self.server_url.as_deref()?;
        let id = self.stream_id.as_deref()?;
        let vtok = self.view_token.as_deref()?;
        Some(format!("{base}/stream/{id}?vtok={vtok}"))
    }

    /// Returns true when the config has everything needed to start pushing.
    pub fn is_ready(&self) -> bool {
        self.log_path.is_some() && self.ingest_ws_url().is_some() && self.stream_token.is_some()
    }

    /// Returns true when stream credentials have been obtained from a server
    /// (stream_id, stream_token, and view_token are all present).
    pub fn is_registered(&self) -> bool {
        self.stream_id.is_some() && self.stream_token.is_some() && self.view_token.is_some()
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
