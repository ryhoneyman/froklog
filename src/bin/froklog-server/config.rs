use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use tracing::{info, warn};

#[derive(Serialize, Deserialize)]
pub struct ServerConfig {
    pub bind: String,
    pub data_dir: String,
    pub admin_token: String,
    #[serde(default)]
    pub stream_password: String,
    pub rust_log: String,
    pub rate_max: u32,
    pub rate_window_secs: u64,
    pub ban_secs: u64,
    /// Automatically delete stream data older than this many days.
    /// 0 (the default) disables automatic pruning entirely.
    #[serde(default)]
    pub retention_days: u64,
}

impl Default for ServerConfig {
    fn default() -> Self {
        Self {
            bind: "0.0.0.0:8766".into(),
            data_dir: "streams".into(),
            admin_token: froklog::auth::generate_token(),
            stream_password: String::new(),
            rust_log: "froklog_server=info".into(),
            rate_max: 100,
            rate_window_secs: 10,
            ban_secs: 300,
            retention_days: 0,
        }
    }
}

/// Returns the config file path: `FROKLOG_CONFIG` env var, or `froklog-server.toml`
/// in the same directory as the running binary, falling back to the current directory.
pub fn config_path() -> PathBuf {
    if let Ok(p) = std::env::var("FROKLOG_CONFIG") {
        return PathBuf::from(p);
    }
    std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(Path::to_path_buf))
        .unwrap_or_else(|| PathBuf::from("."))
        .join("froklog-server.toml")
}

/// If the binary lives inside a Cargo `target/<profile>/` directory, returns the
/// project root (two levels up). Otherwise returns `None`.
fn project_root_from_exe() -> Option<PathBuf> {
    let exe = std::env::current_exe().ok()?;
    let profile_dir = exe.parent()?; // e.g. …/target/debug
    let target_dir = profile_dir.parent()?; // e.g. …/target
    if target_dir.file_name()?.to_str()? != "target" {
        return None;
    }
    target_dir.parent().map(Path::to_path_buf)
}

/// Resolve `data_dir` to an absolute `PathBuf`.
///
/// Absolute paths are returned unchanged. Relative paths are resolved against:
///   1. The project root when the binary lives inside `target/<profile>/`
///      (i.e. a `cargo run` / dev-build scenario), or
///   2. The directory containing the config file.
///
/// This ensures streams survive `cargo clean` even when no explicit absolute
/// path has been set in the config.
pub fn resolve_data_dir(data_dir: &str, config_path: &Path) -> PathBuf {
    let p = PathBuf::from(data_dir);
    if p.is_absolute() {
        return p;
    }
    let base = project_root_from_exe()
        .or_else(|| config_path.parent().map(Path::to_path_buf))
        .unwrap_or_else(|| PathBuf::from("."));
    base.join(p)
}

/// Load config from `path`, creating it with defaults if it does not exist.
pub fn load_or_create(path: &Path) -> ServerConfig {
    if path.exists() {
        match std::fs::read_to_string(path) {
            Ok(raw) => match toml::from_str::<ServerConfig>(&raw) {
                Ok(cfg) => {
                    info!("Loaded config from {}", path.display());
                    return cfg;
                }
                Err(e) => {
                    warn!(
                        "Config parse error in {} — using defaults. Error: {e}",
                        path.display()
                    );
                }
            },
            Err(e) => {
                warn!(
                    "Could not read {} — using defaults. Error: {e}",
                    path.display()
                );
            }
        }
    }

    // Generate an absolute data_dir so newly-created configs don't store a
    // relative path that depends on the working directory at runtime.
    let cfg = ServerConfig {
        data_dir: resolve_data_dir("streams", path)
            .to_string_lossy()
            .into_owned(),
        ..Default::default()
    };
    write_defaults(path, &cfg);
    cfg
}

fn write_defaults(path: &Path, cfg: &ServerConfig) {
    let contents = format!(
        r#"# froklog-server configuration
# Generated on first run — edit to suit your deployment.
# Environment variables (FROKLOG_*) override these values when set.

# Address and port to listen on.
bind = "{bind}"

# Directory where stream journals are stored.
data_dir = "{data_dir}"

# Token required for admin operations (POST /stream, GET /admin).
# A random token was generated — change it or keep this one.
admin_token = "{admin_token}"

# Password required to create new streams.
# Leave empty to allow anyone to create streams.
stream_password = "{stream_password}"

# Log filter (RUST_LOG syntax).
rust_log = "{rust_log}"

# Maximum requests per IP within the window before the IP is blocked.
rate_max = {rate_max}

# Rate-limit window in seconds.
rate_window_secs = {rate_window_secs}

# How long (in seconds) a banned IP stays blocked.
ban_secs = {ban_secs}

# Automatically delete stream data older than this many days (daily sweep).
# 0 disables automatic pruning. Owners can always prune their own stream
# manually via POST /stream/<id>/prune.
retention_days = {retention_days}
"#,
        bind = cfg.bind,
        data_dir = cfg.data_dir,
        admin_token = cfg.admin_token,
        stream_password = cfg.stream_password,
        rust_log = cfg.rust_log,
        rate_max = cfg.rate_max,
        rate_window_secs = cfg.rate_window_secs,
        ban_secs = cfg.ban_secs,
        retention_days = cfg.retention_days,
    );

    match std::fs::write(path, contents) {
        Ok(()) => info!("Created default config at {}", path.display()),
        Err(e) => warn!("Could not write default config to {}: {e}", path.display()),
    }
}

/// Apply any `FROKLOG_*` environment variables on top of the loaded config.
/// Env vars win over the config file, preserving backward compatibility.
pub fn apply_env_overrides(cfg: &mut ServerConfig) {
    if let Ok(v) = std::env::var("FROKLOG_BIND") {
        cfg.bind = v;
    }
    if let Ok(v) = std::env::var("FROKLOG_DATA_DIR") {
        cfg.data_dir = v;
    }
    if let Ok(v) = std::env::var("FROKLOG_ADMIN_TOKEN") {
        cfg.admin_token = v;
    }
    if let Ok(v) = std::env::var("FROKLOG_STREAM_PASSWORD") {
        cfg.stream_password = v;
    }
    if let Ok(v) = std::env::var("RUST_LOG") {
        cfg.rust_log = v;
    }
    if let Ok(v) = std::env::var("FROKLOG_RATE_MAX") {
        if let Ok(n) = v.parse() {
            cfg.rate_max = n;
        }
    }
    if let Ok(v) = std::env::var("FROKLOG_RATE_WINDOW_SECS") {
        if let Ok(n) = v.parse() {
            cfg.rate_window_secs = n;
        }
    }
    if let Ok(v) = std::env::var("FROKLOG_BAN_SECS") {
        if let Ok(n) = v.parse() {
            cfg.ban_secs = n;
        }
    }
    if let Ok(v) = std::env::var("FROKLOG_RETENTION_DAYS") {
        if let Ok(n) = v.parse() {
            cfg.retention_days = n;
        }
    }
}
