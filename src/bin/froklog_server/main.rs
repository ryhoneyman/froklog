mod admin;
mod ingest;
mod journal;
mod ratelimit;
mod streams;
mod viewer;

use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;

use axum::extract::{ConnectInfo, State};
use axum::http::{HeaderMap, StatusCode};
use axum::middleware;
use axum::response::IntoResponse;
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::{Deserialize, Serialize};
use serde_json::json;
use tower_http::cors::{Any, CorsLayer};
use tracing::{info, warn};

use streams::{new_registry, SharedRegistry, StreamEntry};

// ── Utilities ─────────────────────────────────────────────────────────────────

/// Returns the best available client IP: the leftmost address in
/// `X-Forwarded-For` (set by Caddy / any trusted reverse proxy), then
/// `X-Real-IP`, then the raw TCP peer address as a fallback for direct
/// connections.
pub(crate) fn client_ip(headers: &axum::http::HeaderMap, peer: SocketAddr) -> String {
    let forwarded = headers
        .get("x-forwarded-for")
        .and_then(|v| v.to_str().ok())
        .and_then(|s| s.split(',').next())
        .map(str::trim)
        .filter(|s| !s.is_empty());

    if let Some(ip) = forwarded {
        return ip.to_string();
    }

    if let Some(ip) = headers
        .get("x-real-ip")
        .and_then(|v| v.to_str().ok())
        .map(str::trim)
        .filter(|s| !s.is_empty())
    {
        return ip.to_string();
    }

    peer.ip().to_string()
}

// ── Shared state ──────────────────────────────────────────────────────────────

/// State shared across all Axum handlers.
#[derive(Clone)]
pub struct ServerState {
    pub registry: SharedRegistry,
    /// Root directory for on-disk journals.
    pub data_dir: PathBuf,
    /// Secret that authorises `GET /admin`.
    admin_token: String,
    /// Optional password required to create streams via `POST /stream`.
    /// When `None`, stream creation is open to anyone.
    stream_password: Option<String>,
    /// Per-IP request rate limiter / DoS blacklist.
    pub rate_limiter: Arc<ratelimit::RateLimiter>,
}

impl ServerState {
    pub fn is_admin_token(&self, token: &str) -> bool {
        froklog::auth::tokens_match(&self.admin_token, token)
    }

    /// Returns `true` when the supplied password satisfies the stream-creation
    /// policy: open servers always return `true`; password-protected servers
    /// require a non-empty match.
    pub fn stream_auth_ok(&self, supplied: Option<&str>) -> bool {
        match &self.stream_password {
            None => true,
            Some(required) => supplied
                .map(|s| froklog::auth::tokens_match(required, s))
                .unwrap_or(false),
        }
    }

    pub fn requires_stream_password(&self) -> bool {
        self.stream_password.is_some()
    }
}

// ── Entry point ───────────────────────────────────────────────────────────────

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt()
        .with_env_filter(
            std::env::var("RUST_LOG")
                .unwrap_or_else(|_| "froklog_server=info".into())
                .as_str(),
        )
        .init();

    let admin_token = std::env::var("FROKLOG_ADMIN_TOKEN").unwrap_or_else(|_| {
        let t = froklog::auth::generate_token();
        eprintln!("⚠  FROKLOG_ADMIN_TOKEN not set — using temporary token: {t}");
        eprintln!("   Set the env var to make this permanent.");
        t
    });

    let stream_password = std::env::var("FROKLOG_STREAM_PASSWORD")
        .ok()
        .filter(|s| !s.is_empty());

    let data_dir: PathBuf = std::env::var("FROKLOG_DATA_DIR")
        .unwrap_or_else(|_| "streams".to_string())
        .into();

    std::fs::create_dir_all(&data_dir).expect("create FROKLOG_DATA_DIR");

    let registry = new_registry(data_dir.clone());

    // ── Reload persisted streams from disk ────────────────────────────────────
    // Each sub-directory of data_dir that contains a `meta.json` file is a
    // previously-created stream we can restore (journal already on disk).
    load_persisted_streams(&data_dir, &registry).await;

    if stream_password.is_some() {
        info!("Stream creation: password-protected (FROKLOG_STREAM_PASSWORD is set)");
    } else {
        info!("Stream creation: open (set FROKLOG_STREAM_PASSWORD to require a password)");
    }

    let rate_max: u32 = std::env::var("FROKLOG_RATE_MAX")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(100);
    let rate_window: u64 = std::env::var("FROKLOG_RATE_WINDOW_SECS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(10);
    let ban_secs: u64 = std::env::var("FROKLOG_BAN_SECS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(300);

    info!(
        "DoS protection: max {rate_max} req / {rate_window}s per IP, ban duration {ban_secs}s \
         (override with FROKLOG_RATE_MAX / FROKLOG_RATE_WINDOW_SECS / FROKLOG_BAN_SECS)"
    );

    let rate_limiter = ratelimit::RateLimiter::new(rate_max, rate_window, ban_secs);

    let state = ServerState {
        registry,
        data_dir,
        admin_token,
        stream_password,
        rate_limiter: Arc::clone(&rate_limiter),
    };

    // Periodically evict stale rate-limiter entries so memory doesn't grow unboundedly.
    tokio::spawn({
        let limiter = Arc::clone(&rate_limiter);
        async move {
            let mut interval = tokio::time::interval(std::time::Duration::from_secs(300));
            loop {
                interval.tick().await;
                limiter.cleanup().await;
            }
        }
    });

    let cors = CorsLayer::new().allow_origin(Any);

    let app = Router::new()
        // Admin panel
        .route("/admin", get(admin::admin_panel_handler))
        // Stream management
        .route("/stream", post(create_stream_handler))
        // Viewer routes (private, token-gated)
        .route("/stream/{id}", get(viewer::stream_page_handler))
        .route("/stream/{id}/ws", get(viewer::stream_ws_handler))
        .route("/stream/{id}/stats", get(stream_stats_handler))
        // Public player routes (live only, no token)
        .route("/player/{server}/{name}", get(viewer::player_page_handler))
        .route("/player/{server}/{name}/ws", get(viewer::player_ws_handler))
        // Ingest route (Windows clients push here)
        .route("/ingest/{id}", get(ingest::ingest_ws_handler))
        // Health check
        .route("/health", get(health_handler))
        // Stream list / index
        .route("/", get(index_handler))
        .fallback(unmatched_handler)
        .layer(middleware::from_fn_with_state(
            state.clone(),
            ratelimit::rate_limit_middleware,
        ))
        .layer(cors)
        .with_state(state);

    let addr: SocketAddr = std::env::var("FROKLOG_BIND")
        .unwrap_or_else(|_| "0.0.0.0:8766".to_string())
        .parse()
        .expect("invalid FROKLOG_BIND address");

    info!("froklog-server listening on http://{addr}");
    info!("Create a stream:  POST /stream  with  Authorization: Bearer <FROKLOG_ADMIN_TOKEN>");

    let listener = tokio::net::TcpListener::bind(addr)
        .await
        .expect("bind server");

    axum::serve(
        listener,
        app.into_make_service_with_connect_info::<SocketAddr>(),
    )
    .await
    .expect("server");
}

// ── Stream creation ───────────────────────────────────────────────────────────

#[derive(Deserialize)]
struct CreateStreamBody {
    server: String,
    player: String,
}

#[derive(Serialize)]
struct CreateStreamResponse {
    stream_id: String,
    stream_token: String,
    view_token: String,
    server: String,
    player: String,
    ingest_ws_path: String,
    view_path: String,
    player_path: Option<String>,
}

async fn create_stream_handler(
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    State(state): State<ServerState>,
    headers: HeaderMap,
    Json(body): Json<CreateStreamBody>,
) -> impl IntoResponse {
    let ip = client_ip(&headers, peer);
    let supplied = ingest::extract_bearer(&headers);
    if !state.stream_auth_ok(supplied.as_deref()) {
        warn!("Register [{ip}]: rejected — bad or missing stream password");
        return StatusCode::UNAUTHORIZED.into_response();
    }
    info!(
        "Register [{ip}]: player '{}' on '{}'",
        body.player, body.server
    );

    // Use the first 16 chars of a random token as the public stream ID.
    let stream_id = froklog::auth::generate_token()[..16].to_string();
    let stream_token = froklog::auth::generate_token();
    let view_token = froklog::auth::generate_token();

    let ingest_path = format!("/ingest/{stream_id}");
    let view_path = format!("/stream/{stream_id}?vtok={view_token}");
    let player_path = if !body.server.is_empty() {
        Some(format!("/player/{}/{}", body.server, body.player))
    } else {
        None
    };

    let entry = StreamEntry::new(
        stream_id.clone(),
        stream_token.clone(),
        view_token.clone(),
        body.server.clone(),
        body.player.clone(),
        &state.data_dir,
    );

    let entry = match entry {
        Ok(e) => e,
        Err(e) => {
            tracing::error!("Failed to create journal for {stream_id}: {e}");
            return StatusCode::INTERNAL_SERVER_ERROR.into_response();
        }
    };

    // Persist meta so the stream survives a server restart.
    let meta_path = state.data_dir.join(&stream_id).join("meta.json");
    let meta = serde_json::json!({
        "stream_id": stream_id,
        "stream_token": stream_token,
        "view_token": view_token,
        "server": body.server,
        "player": body.player,
    });
    if let Err(e) = std::fs::write(&meta_path, serde_json::to_string(&meta).unwrap()) {
        tracing::warn!("Failed to write meta for {stream_id}: {e}");
    }

    info!(
        "Created stream {stream_id} for player '{}' on '{}'",
        body.player, body.server
    );
    state.registry.write().await.insert(entry);

    Json(CreateStreamResponse {
        stream_id,
        stream_token,
        view_token,
        server: body.server,
        player: body.player,
        ingest_ws_path: ingest_path,
        view_path,
        player_path,
    })
    .into_response()
}

// ── HTTP stats snapshot (poll fallback for viewers) ───────────────────────────

async fn stream_stats_handler(
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
    axum::extract::Path(stream_id): axum::extract::Path<String>,
    axum::extract::Query(params): axum::extract::Query<viewer::ViewQuery>,
    State(state): State<ServerState>,
) -> impl IntoResponse {
    info!("Stats [{stream_id}] from {}", client_ip(&headers, peer));
    let reg = state.registry.read().await;
    match reg.get(&stream_id) {
        Some(entry) => {
            let valid = params
                .vtok
                .as_deref()
                .map(|t| froklog::auth::tokens_match(&entry.view_token, t))
                .unwrap_or(false);
            if !valid {
                return StatusCode::UNAUTHORIZED.into_response();
            }
            let journal = entry.journal.read().await;
            Json(json!({
                "batches": journal.len(),
                "first_ts": journal.first_ts(),
                "last_ts": journal.last_ts(),
                "client_connected": entry.client_connected.load(std::sync::atomic::Ordering::Relaxed),
            }))
            .into_response()
        }
        None => StatusCode::NOT_FOUND.into_response(),
    }
}

// ── Health check ──────────────────────────────────────────────────────────────

async fn health_handler(
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
    State(state): State<ServerState>,
) -> impl IntoResponse {
    info!("Health check (Test) from {}", client_ip(&headers, peer));
    Json(serde_json::json!({
        "ok": true,
        "requires_password": state.requires_stream_password(),
    }))
}

// ── Server index ──────────────────────────────────────────────────────────────

async fn index_handler(
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
) -> impl IntoResponse {
    info!("Index from {}", client_ip(&headers, peer));
    StatusCode::NOT_FOUND
}

async fn unmatched_handler(
    method: axum::http::Method,
    uri: axum::http::Uri,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
) -> impl IntoResponse {
    warn!(
        "Unmatched {} {} from {}",
        method,
        uri,
        client_ip(&headers, peer)
    );
    StatusCode::NOT_FOUND
}

// ── Startup: reload streams persisted from a prior run ───────────────────────

#[derive(serde::Deserialize)]
struct StreamMeta {
    stream_id: String,
    stream_token: String,
    view_token: String,
    #[serde(default)]
    server: String,
    player: String,
}

async fn load_persisted_streams(data_dir: &std::path::Path, registry: &SharedRegistry) {
    let Ok(entries) = std::fs::read_dir(data_dir) else {
        return;
    };
    let mut loaded = 0usize;
    for entry in entries.flatten() {
        let meta_path = entry.path().join("meta.json");
        if !meta_path.exists() {
            continue;
        }
        let Ok(raw) = std::fs::read_to_string(&meta_path) else {
            continue;
        };
        let Ok(meta) = serde_json::from_str::<StreamMeta>(&raw) else {
            tracing::warn!("Skipping malformed meta at {}", meta_path.display());
            continue;
        };
        match StreamEntry::new(
            meta.stream_id.clone(),
            meta.stream_token,
            meta.view_token,
            meta.server,
            meta.player.clone(),
            data_dir,
        ) {
            Ok(entry) => {
                registry.write().await.insert(entry);
                loaded += 1;
                info!(
                    "Restored stream {} (player: {})",
                    meta.stream_id, meta.player
                );
            }
            Err(e) => {
                tracing::warn!("Failed to restore stream {}: {e}", meta.stream_id);
            }
        }
    }
    info!(
        "Loaded {loaded} persisted stream(s) from {}",
        data_dir.display()
    );
}
