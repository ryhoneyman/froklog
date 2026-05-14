mod ingest;
mod journal;
mod streams;
mod viewer;

use std::net::SocketAddr;
use std::path::PathBuf;

use axum::extract::State;
use axum::http::{HeaderMap, StatusCode};
use axum::response::IntoResponse;
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::{Deserialize, Serialize};
use serde_json::json;
use tower_http::cors::{Any, CorsLayer};
use tracing::info;

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
    /// Secret that authorises `POST /stream` (stream creation).
    admin_token: String,
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

    let data_dir: PathBuf = std::env::var("FROKLOG_DATA_DIR")
        .unwrap_or_else(|_| "streams".to_string())
        .into();

    std::fs::create_dir_all(&data_dir).expect("create FROKLOG_DATA_DIR");

    let registry = new_registry(data_dir.clone());

    // ── Reload persisted streams from disk ────────────────────────────────────
    // Each sub-directory of data_dir that contains a `meta.json` file is a
    // previously-created stream we can restore (journal already on disk).
    load_persisted_streams(&data_dir, &registry).await;

    let state = ServerState {
        registry,
        data_dir,
        admin_token,
    };

    let cors = CorsLayer::new().allow_origin(Any);

    let app = Router::new()
        // Stream management
        .route("/stream", post(create_stream_handler))
        // Viewer routes
        .route("/stream/{id}", get(viewer::stream_page_handler))
        .route("/stream/{id}/ws", get(viewer::stream_ws_handler))
        .route("/stream/{id}/stats", get(stream_stats_handler))
        // Ingest route (Windows clients push here)
        .route("/ingest/{id}", get(ingest::ingest_ws_handler))
        // Health / stream list
        .route("/", get(index_handler))
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
    player: String,
}

#[derive(Serialize)]
struct CreateStreamResponse {
    stream_id: String,
    stream_token: String,
    view_token: String,
    ingest_ws_path: String,
    view_path: String,
}

async fn create_stream_handler(
    State(state): State<ServerState>,
    headers: HeaderMap,
    Json(body): Json<CreateStreamBody>,
) -> impl IntoResponse {
    let token = match ingest::extract_bearer(&headers) {
        Some(t) => t,
        None => return StatusCode::UNAUTHORIZED.into_response(),
    };
    if !froklog::auth::tokens_match(&state.admin_token, &token) {
        return StatusCode::UNAUTHORIZED.into_response();
    }

    // Use the first 16 chars of a random token as the public stream ID.
    let stream_id = froklog::auth::generate_token()[..16].to_string();
    let stream_token = froklog::auth::generate_token();
    let view_token = froklog::auth::generate_token();

    let ingest_path = format!("/ingest/{stream_id}");
    let view_path = format!("/stream/{stream_id}?vtok={view_token}");

    let entry = StreamEntry::new(
        stream_id.clone(),
        stream_token.clone(),
        view_token.clone(),
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
        "player": body.player,
    });
    if let Err(e) = std::fs::write(&meta_path, serde_json::to_string(&meta).unwrap()) {
        tracing::warn!("Failed to write meta for {stream_id}: {e}");
    }

    info!("Created stream {stream_id} for player '{}'", body.player);
    state.registry.write().await.insert(entry);

    Json(CreateStreamResponse {
        stream_id,
        stream_token,
        view_token,
        ingest_ws_path: ingest_path,
        view_path,
    })
    .into_response()
}

// ── HTTP stats snapshot (poll fallback for viewers) ───────────────────────────

async fn stream_stats_handler(
    axum::extract::Path(stream_id): axum::extract::Path<String>,
    axum::extract::Query(params): axum::extract::Query<viewer::ViewQuery>,
    State(state): State<ServerState>,
) -> impl IntoResponse {
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

// ── Server index ──────────────────────────────────────────────────────────────

async fn index_handler() -> impl IntoResponse {
    StatusCode::NOT_FOUND
}

// ── Startup: reload streams persisted from a prior run ───────────────────────

#[derive(serde::Deserialize)]
struct StreamMeta {
    stream_id: String,
    stream_token: String,
    view_token: String,
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
