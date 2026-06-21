pub mod auth;
pub mod chat;
pub mod config;
pub mod event;
pub mod parser;
pub mod patterns;
pub mod pusher;
pub mod state;
pub mod tailer;
pub mod tray;
pub mod triggers;

#[cfg(feature = "tray")]
pub mod settings_win;
#[cfg(feature = "tray")]
pub mod overlay;
#[cfg(feature = "tray")]
pub mod overlay_config_win;
