mod admin;
mod config;
mod ingest;
mod journal;
mod markers;
mod mob_overrides;
mod ratelimit;
mod segment_roster;
mod session_index;
mod streams;
mod viewer;

use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;

use axum::extract::{ConnectInfo, Path, State};
use axum::http::{HeaderMap, StatusCode};
use axum::middleware;
use axum::response::{IntoResponse, Response};
use axum::routing::{delete, get, patch, post};
use axum::{Json, Router};
use serde::{Deserialize, Serialize};
use serde_json::json;
use tower_http::cors::{Any, CorsLayer};
use tracing::{info, warn};

use streams::{new_registry, SharedRegistry, StreamEntry};

// ── Utilities ─────────────────────────────────────────────────────────────────

/// When set (config `trusted_proxy`), forwarded headers are honored ONLY for
/// connections arriving from this address. Without it, anyone who can reach
/// the port directly can spoof X-Forwarded-For and bypass the per-IP rate
/// limiter/ban with a fresh fake address per request.
static TRUSTED_PROXY: std::sync::OnceLock<Option<std::net::IpAddr>> = std::sync::OnceLock::new();

/// Returns the best available client IP: the leftmost address in
/// `X-Forwarded-For` (set by Caddy / any trusted reverse proxy), then
/// `X-Real-IP`, then the raw TCP peer address as a fallback for direct
/// connections.
pub(crate) fn client_ip(headers: &axum::http::HeaderMap, peer: SocketAddr) -> String {
    // If a trusted proxy is configured, only believe forwarded headers from it.
    if let Some(Some(trusted)) = TRUSTED_PROXY.get() {
        if peer.ip() != *trusted {
            return peer.ip().to_string();
        }
    }
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
    let cfg_path = config::config_path();
    let (mut cfg, cfg_warnings) = config::load_or_create(&cfg_path);
    config::apply_env_overrides(&mut cfg);

    tracing_subscriber::fmt()
        .with_env_filter(cfg.rust_log.as_str())
        .init();

    // Emit warnings collected before tracing existed (config parse errors
    // used to vanish here — including the one that regenerates the admin token).
    for w in &cfg_warnings {
        warn!("{w}");
    }
    info!("Config: {}", cfg_path.display());

    // Only honor X-Forwarded-For from the configured reverse proxy (if any).
    let trusted_proxy = if cfg.trusted_proxy.is_empty() {
        None
    } else {
        match cfg.trusted_proxy.parse::<std::net::IpAddr>() {
            Ok(ip) => {
                info!("Trusting forwarded headers only from proxy {ip}");
                Some(ip)
            }
            Err(_) => {
                warn!(
                    "Invalid trusted_proxy '{}' — forwarded headers will be trusted from ANY connection",
                    cfg.trusted_proxy
                );
                None
            }
        }
    };
    let _ = TRUSTED_PROXY.set(trusted_proxy);

    let stream_password = if cfg.stream_password.is_empty() {
        None
    } else {
        Some(cfg.stream_password.clone())
    };

    let data_dir = config::resolve_data_dir(&cfg.data_dir, &cfg_path);
    std::fs::create_dir_all(&data_dir).expect("create data_dir");
    info!("Stream data directory: {}", data_dir.display());

    let registry = new_registry(data_dir.clone());

    // ── Reload persisted streams from disk ────────────────────────────────────
    // Each sub-directory of data_dir that contains a `meta.json` file is a
    // previously-created stream we can restore (journal already on disk).
    load_persisted_streams(&data_dir, &registry).await;

    if stream_password.is_some() {
        info!("Stream creation: password-protected");
    } else {
        info!("Stream creation: open (set stream_password in config to require a password)");
    }

    info!(
        "DoS protection: max {} req / {}s per IP, ban duration {}s",
        cfg.rate_max, cfg.rate_window_secs, cfg.ban_secs
    );

    let rate_limiter =
        ratelimit::RateLimiter::new(cfg.rate_max, cfg.rate_window_secs, cfg.ban_secs);

    let state = ServerState {
        registry,
        data_dir,
        admin_token: cfg.admin_token.clone(),
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

    // Optional retention sweep: prune data older than retention_days from every
    // stream, once at startup and then daily. Disabled when retention_days = 0.
    if cfg.retention_days > 0 {
        info!(
            "Retention: pruning stream data older than {} day(s), swept daily",
            cfg.retention_days
        );
        tokio::spawn({
            let registry = Arc::clone(&state.registry);
            let days = cfg.retention_days;
            async move {
                let mut interval = tokio::time::interval(std::time::Duration::from_secs(86_400));
                loop {
                    interval.tick().await;
                    let now = std::time::SystemTime::now()
                        .duration_since(std::time::UNIX_EPOCH)
                        .unwrap_or_default()
                        .as_secs();
                    let cutoff = now.saturating_sub(days.saturating_mul(86_400));
                    // Clone handles under a short lock, prune outside it.
                    let streams: Vec<_> = {
                        let reg = registry.read().await;
                        reg.list_admin()
                            .into_iter()
                            .map(|i| (i.stream_id, i.journal, i.session_index, i.markers))
                            .collect()
                    };
                    for (stream_id, journal, session_index, markers) in streams {
                        match prune_stream_data(&journal, &session_index, &markers, cutoff).await {
                            Ok((batches, sessions)) if batches > 0 || sessions > 0 => {
                                info!(
                                    "Retention [{stream_id}]: removed {batches} batches, {sessions} sessions"
                                );
                            }
                            Ok(_) => {}
                            Err(e) => warn!("Retention [{stream_id}]: prune failed: {e}"),
                        }
                    }
                }
            }
        });
    }

    let cors = CorsLayer::new().allow_origin(Any);

    let app = Router::new()
        // Favicon
        .route("/favicon.ico", get(favicon_handler))
        // Admin panel
        .route("/admin", get(admin::admin_panel_handler))
        // Stream management
        .route("/stream", post(create_stream_handler))
        .route("/stream/{id}", patch(patch_stream_handler))
        .route("/stream/{id}", delete(reset_stream_handler))
        .route("/stream/{id}/purge", delete(delete_stream_handler))
        .route("/stream/{id}/prune", post(prune_stream_handler))
        // Time markers (raid/group slices)
        .route("/stream/{id}/marker", post(add_marker_handler))
        .route("/stream/{id}/marker/{mid}", delete(delete_marker_handler))
        .route("/stream/{id}/markers", get(list_markers_handler))
        // Sibling streams (same owner_key household link)
        .route("/stream/{id}/siblings", get(siblings_handler))
        // Named-NPC curation (owner judgment over the ★ heuristic)
        .route(
            "/stream/{id}/mob_overrides",
            get(list_mob_overrides_handler),
        )
        .route(
            "/stream/{id}/segment_members",
            get(list_segment_members_handler),
        )
        .route(
            "/stream/{id}/segment_member",
            post(set_segment_member_handler),
        )
        .route("/stream/{id}/mob_override", post(set_mob_override_handler))
        // Viewer routes (private, token-gated)
        .route("/stream/{id}", get(viewer::stream_page_handler))
        .route("/stream/{id}/ws", get(viewer::stream_ws_handler))
        .route("/stream/{id}/stats", get(stream_stats_handler))
        .route(
            "/stream/{id}/sessions",
            get(viewer::stream_sessions_handler),
        )
        // Public player routes (live and recorded, no token)
        .route(
            "/player/{game}/{server}/{name}",
            get(viewer::player_page_handler),
        )
        .route(
            "/player/{game}/{server}/{name}/ws",
            get(viewer::player_ws_handler),
        )
        .route(
            "/player/{game}/{server}/{name}/sessions",
            get(viewer::player_sessions_handler),
        )
        .route(
            "/player/{game}/{server}/{name}/mob_overrides",
            get(player_mob_overrides_handler),
        )
        .route(
            "/player/{game}/{server}/{name}/segment_members",
            get(player_segment_members_handler),
        )
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

    let addr: SocketAddr = cfg.bind.parse().expect("invalid bind address in config");

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
    #[serde(default)]
    game: String,
    server: String,
    player: String,
    #[serde(default)]
    public_stream: bool,
    #[serde(default)]
    is_replay: bool,
    /// Optional household link — see StreamEntry.owner_key.
    #[serde(default)]
    owner_key: String,
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
        Some(format!(
            "/player/{}/{}/{}",
            body.game, body.server, body.player
        ))
    } else {
        None
    };

    let owner_key = if body.owner_key.len() <= 64 {
        body.owner_key.clone()
    } else {
        String::new()
    };
    let entry = StreamEntry::new(
        stream_id.clone(),
        stream_token.clone(),
        view_token.clone(),
        body.game.clone(),
        body.server.clone(),
        body.player.clone(),
        body.public_stream,
        body.is_replay,
        owner_key.clone(),
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
        "game": body.game,
        "server": body.server,
        "player": body.player,
        "public_stream": body.public_stream,
        "is_replay": body.is_replay,
        "owner_key": owner_key,
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

// ── Stream metadata update ────────────────────────────────────────────────────

#[derive(Deserialize)]
struct PatchStreamBody {
    public_stream: Option<bool>,
    /// Backfill the household link on an existing stream (see owner_key).
    owner_key: Option<String>,
}

/// `PATCH /stream/:id` — update mutable stream metadata.
/// Authenticated with the per-stream `stream_token` (Bearer).
async fn patch_stream_handler(
    Path(stream_id): Path<String>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
    State(state): State<ServerState>,
    Json(body): Json<PatchStreamBody>,
) -> Response {
    let ip = client_ip(&headers, peer);
    let token = match ingest::extract_bearer(&headers) {
        Some(t) => t,
        None => {
            warn!("Patch [{stream_id}] [{ip}]: missing token");
            return StatusCode::UNAUTHORIZED.into_response();
        }
    };

    // Validate token, apply update, and snapshot fields for meta rewrite —
    // all inside the write lock so no reader sees a half-updated entry.
    let snapshot = {
        let mut reg = state.registry.write().await;
        let entry = match reg.get_mut(&stream_id) {
            Some(e) => e,
            None => return StatusCode::NOT_FOUND.into_response(),
        };
        if !froklog::auth::tokens_match(&entry.stream_token, &token) {
            warn!("Patch [{stream_id}] [{ip}]: bad token");
            return StatusCode::UNAUTHORIZED.into_response();
        }
        if let Some(public) = body.public_stream {
            entry.public_stream = public;
            if !public {
                let _ = entry.public_revoke_tx.send(());
            }
        }
        if let Some(key) = &body.owner_key {
            if key.len() <= 64 {
                entry.owner_key = key.clone();
            }
        }
        (
            entry.stream_id.clone(),
            entry.stream_token.clone(),
            entry.view_token.clone(),
            entry.game.clone(),
            entry.server.clone(),
            entry.player_name.clone(),
            entry.public_stream,
            entry.is_replay,
            entry.owner_key.clone(),
        )
    }; // write lock released here

    let meta_path = state.data_dir.join(&snapshot.0).join("meta.json");
    let meta = serde_json::json!({
        "stream_id":    snapshot.0,
        "stream_token": snapshot.1,
        "view_token":   snapshot.2,
        "game":         snapshot.3,
        "server":       snapshot.4,
        "player":       snapshot.5,
        "public_stream": snapshot.6,
        "is_replay":    snapshot.7,
        "owner_key":    snapshot.8,
    });
    if let Err(e) = std::fs::write(&meta_path, serde_json::to_string(&meta).unwrap()) {
        warn!("Patch [{stream_id}]: failed to rewrite meta: {e}");
    }
    info!("Patch [{stream_id}] [{ip}]: public_stream={}", snapshot.6);
    StatusCode::OK.into_response()
}

// ── Stream reset (admin only) ─────────────────────────────────────────────────

/// `DELETE /stream/:id` — wipe journal and sessions; stream identity is preserved.
/// Authenticated with the global admin token (Bearer).
async fn reset_stream_handler(
    Path(stream_id): Path<String>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
    State(state): State<ServerState>,
) -> Response {
    let ip = client_ip(&headers, peer);
    let token = match ingest::extract_bearer(&headers) {
        Some(t) => t,
        None => {
            warn!("Reset [{stream_id}] [{ip}]: missing token");
            return StatusCode::UNAUTHORIZED.into_response();
        }
    };
    if !state.is_admin_token(&token) {
        warn!("Reset [{stream_id}] [{ip}]: bad token");
        return StatusCode::UNAUTHORIZED.into_response();
    }

    // Clone Arc handles before dropping the registry lock.
    let handles = {
        let reg = state.registry.read().await;
        reg.get(&stream_id)
            .map(|e| (Arc::clone(&e.journal), Arc::clone(&e.session_index)))
    };
    let (journal, session_index) = match handles {
        Some(h) => h,
        None => return StatusCode::NOT_FOUND.into_response(),
    };

    {
        let mut j = journal.write().await;
        if let Err(e) = j.clear() {
            warn!("Reset [{stream_id}] [{ip}]: journal clear failed: {e}");
            return StatusCode::INTERNAL_SERVER_ERROR.into_response();
        }
    }
    {
        let mut si = session_index.write().await;
        if let Err(e) = si.clear() {
            warn!("Reset [{stream_id}] [{ip}]: session index clear failed: {e}");
            return StatusCode::INTERNAL_SERVER_ERROR.into_response();
        }
    }

    info!("Reset [{stream_id}] [{ip}]: journal and sessions cleared");
    StatusCode::OK.into_response()
}

// ── Stream delete (admin only) ────────────────────────────────────────────────

/// `DELETE /stream/:id/purge` — remove the stream entirely: deregister and delete all on-disk data.
async fn delete_stream_handler(
    Path(stream_id): Path<String>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
    State(state): State<ServerState>,
) -> Response {
    let ip = client_ip(&headers, peer);
    let token = match ingest::extract_bearer(&headers) {
        Some(t) => t,
        None => {
            warn!("Delete [{stream_id}] [{ip}]: missing token");
            return StatusCode::UNAUTHORIZED.into_response();
        }
    };
    // The stream's own token may destroy the stream and its data — the
    // owning client retires old characters this way. The view token
    // deliberately cannot: watchers must never be able to destroy history.
    let authorized = {
        let reg = state.registry.read().await;
        match reg.get(&stream_id) {
            None => return StatusCode::NOT_FOUND.into_response(),
            Some(e) => {
                froklog::auth::tokens_match(&e.stream_token, &token) || state.is_admin_token(&token)
            }
        }
    };
    if !authorized {
        warn!("Delete [{stream_id}] [{ip}]: bad token");
        return StatusCode::UNAUTHORIZED.into_response();
    }

    let stream_dir = {
        let mut reg = state.registry.write().await;
        match reg.remove(&stream_id) {
            Some(_) => reg.data_dir.join(&stream_id),
            None => return StatusCode::NOT_FOUND.into_response(),
        }
    };

    if stream_dir.exists() {
        if let Err(e) = std::fs::remove_dir_all(&stream_dir) {
            warn!("Delete [{stream_id}] [{ip}]: failed to remove directory: {e}");
            return StatusCode::INTERNAL_SERVER_ERROR.into_response();
        }
    }

    info!("Delete [{stream_id}] [{ip}]: stream removed");
    StatusCode::OK.into_response()
}

// ── Stream prune (owner or admin) ─────────────────────────────────────────────

#[derive(Deserialize)]
struct PruneStreamBody {
    /// Delete everything older than this many days (measured against the
    /// event log timestamps). Mutually exclusive with `before`.
    days: Option<u64>,
    /// Delete everything with a log timestamp older than this unix time.
    before: Option<u64>,
}

/// `POST /stream/:id/prune` — delete batches older than a cutoff and reclaim
/// disk space. Sessions that no longer own any batch are dropped.
///
/// Authenticated with the per-stream `stream_token` (the owner credential the
/// pushing client holds) or the global admin token. The shareable view token
/// deliberately cannot prune: anyone holding a view link can watch, and must
/// not be able to destroy history.
async fn prune_stream_handler(
    Path(stream_id): Path<String>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
    State(state): State<ServerState>,
    Json(body): Json<PruneStreamBody>,
) -> Response {
    let ip = client_ip(&headers, peer);
    let token = match ingest::extract_bearer(&headers) {
        Some(t) => t,
        None => {
            warn!("Prune [{stream_id}] [{ip}]: missing token");
            return StatusCode::UNAUTHORIZED.into_response();
        }
    };

    let cutoff = match (body.days, body.before) {
        (Some(days), None) => {
            let now = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_secs();
            now.saturating_sub(days.saturating_mul(86_400))
        }
        (None, Some(before)) => before,
        _ => {
            return (
                StatusCode::BAD_REQUEST,
                "body must contain exactly one of: days, before",
            )
                .into_response();
        }
    };

    // Owner (stream token) or admin may prune; clone handles under a short lock.
    let handles = {
        let reg = state.registry.read().await;
        match reg.get(&stream_id) {
            None => return StatusCode::NOT_FOUND.into_response(),
            Some(entry) => {
                let authorized = froklog::auth::tokens_match(&entry.stream_token, &token)
                    || state.is_admin_token(&token);
                if !authorized {
                    warn!("Prune [{stream_id}] [{ip}]: bad token");
                    return StatusCode::UNAUTHORIZED.into_response();
                }
                (
                    Arc::clone(&entry.journal),
                    Arc::clone(&entry.session_index),
                    Arc::clone(&entry.markers),
                )
            }
        }
    };

    match prune_stream_data(&handles.0, &handles.1, &handles.2, cutoff).await {
        Ok((deleted_batches, deleted_sessions)) => {
            info!(
                "Prune [{stream_id}] [{ip}]: {deleted_batches} batches, {deleted_sessions} sessions removed (cutoff {cutoff})"
            );
            Json(json!({
                "deleted_batches": deleted_batches,
                "deleted_sessions": deleted_sessions,
                "cutoff": cutoff,
            }))
            .into_response()
        }
        Err(e) => {
            warn!("Prune [{stream_id}] [{ip}]: failed: {e}");
            StatusCode::INTERNAL_SERVER_ERROR.into_response()
        }
    }
}

/// Prune one stream's batches older than `cutoff` and clean up its sessions
/// and markers. Shared by the prune endpoint and the retention sweep.
async fn prune_stream_data(
    journal: &journal::SharedJournal,
    session_index: &session_index::SharedSessionIndex,
    markers: &markers::SharedMarkers,
    cutoff: u64,
) -> std::io::Result<(usize, usize)> {
    let (deleted_batches, min_remaining) = {
        let mut j = journal.write().await;
        j.prune(cutoff)?
    };
    let deleted_sessions = {
        let mut si = session_index.write().await;
        si.prune(min_remaining)?
    };
    markers.prune(cutoff)?;
    Ok((deleted_batches, deleted_sessions))
}

// ── Time markers (raid/group slices) ─────────────────────────────────────────

#[derive(Deserialize)]
struct AddMarkerBody {
    /// e.g. "raid_start", "raid_end", "group_start", "group_end".
    kind: String,
    #[serde(default)]
    label: String,
    /// True-epoch log-time seconds. Omitted = the server's current time,
    /// which matches live event timestamps now that both are true epoch.
    ts: Option<u64>,
}

/// `POST /stream/:id/marker` — set a time marker. Owner (stream token) or admin.
async fn add_marker_handler(
    Path(stream_id): Path<String>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
    axum::extract::Query(params): axum::extract::Query<viewer::ViewQuery>,
    State(state): State<ServerState>,
    Json(body): Json<AddMarkerBody>,
) -> Response {
    let ip = client_ip(&headers, peer);
    let Some(markers) = owner_markers(&state, &stream_id, &headers, params.vtok.as_deref()).await
    else {
        warn!("Marker [{stream_id}] [{ip}]: bad or missing token");
        return StatusCode::UNAUTHORIZED.into_response();
    };
    if body.kind.is_empty() || body.kind.len() > 64 || body.label.len() > 200 {
        return StatusCode::BAD_REQUEST.into_response();
    }
    let ts = body.ts.unwrap_or_else(|| {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs()
    });
    match markers.add(ts, &body.kind, &body.label) {
        Ok(m) => {
            info!("Marker [{stream_id}] [{ip}]: {} @ {ts}", m.kind);
            Json(m).into_response()
        }
        Err(e) => {
            warn!("Marker [{stream_id}] [{ip}]: write failed: {e}");
            StatusCode::INTERNAL_SERVER_ERROR.into_response()
        }
    }
}

/// `DELETE /stream/:id/marker/:mid` — remove a marker. Viewer page, owner or admin.
async fn delete_marker_handler(
    Path((stream_id, marker_id)): Path<(String, i64)>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
    axum::extract::Query(params): axum::extract::Query<viewer::ViewQuery>,
    State(state): State<ServerState>,
) -> Response {
    let ip = client_ip(&headers, peer);
    let Some(markers) = owner_markers(&state, &stream_id, &headers, params.vtok.as_deref()).await
    else {
        warn!("Marker delete [{stream_id}] [{ip}]: bad or missing token");
        return StatusCode::UNAUTHORIZED.into_response();
    };
    match markers.delete(marker_id) {
        Ok(true) => StatusCode::OK.into_response(),
        Ok(false) => StatusCode::NOT_FOUND.into_response(),
        Err(e) => {
            warn!("Marker delete [{stream_id}] [{ip}]: {e}");
            StatusCode::INTERNAL_SERVER_ERROR.into_response()
        }
    }
}

/// `GET /stream/:id/markers?vtok=` — list markers. View-token auth (read-only).
async fn list_markers_handler(
    Path(stream_id): Path<String>,
    axum::extract::Query(params): axum::extract::Query<viewer::ViewQuery>,
    State(state): State<ServerState>,
) -> Response {
    let markers = {
        let reg = state.registry.read().await;
        match reg.get(&stream_id) {
            None => return StatusCode::NOT_FOUND.into_response(),
            Some(entry) => {
                let valid = params
                    .vtok
                    .as_deref()
                    .map(|t| froklog::auth::tokens_match(&entry.view_token, t))
                    .unwrap_or(false);
                if !valid {
                    return StatusCode::UNAUTHORIZED.into_response();
                }
                Arc::clone(&entry.markers)
            }
        }
    };
    match markers.list() {
        Ok(list) => Json(list).into_response(),
        Err(e) => {
            warn!("Markers [{stream_id}]: list failed: {e}");
            StatusCode::INTERNAL_SERVER_ERROR.into_response()
        }
    }
}

// ── Sibling streams ───────────────────────────────────────────────────────────

/// `GET /stream/:id/siblings?vtok=…` — the other streams sharing this
/// stream's owner_key (one household's characters), each with its recent-
/// activity state so the viewer can hint "another character is live".
///
/// Requires this stream's view or stream token. Sibling view links are
/// included: whoever holds one of a household's links is trusted with the
/// household's others (they were registered by the same client). Streams
/// with no owner_key have no siblings.
async fn siblings_handler(
    Path(stream_id): Path<String>,
    axum::extract::Query(params): axum::extract::Query<viewer::ViewQuery>,
    headers: HeaderMap,
    State(state): State<ServerState>,
) -> Response {
    let reg = state.registry.read().await;
    let me = match reg.get(&stream_id) {
        None => return StatusCode::NOT_FOUND.into_response(),
        Some(e) => e,
    };
    let by_vtok = params
        .vtok
        .as_deref()
        .map(|t| froklog::auth::tokens_match(&me.view_token, t))
        .unwrap_or(false);
    let by_bearer = ingest::extract_bearer(&headers)
        .map(|t| froklog::auth::tokens_match(&me.stream_token, &t) || state.is_admin_token(&t))
        .unwrap_or(false);
    if !by_vtok && !by_bearer {
        return StatusCode::UNAUTHORIZED.into_response();
    }
    if me.owner_key.is_empty() {
        return Json(serde_json::json!([])).into_response();
    }
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let mut out = Vec::new();
    for info in reg.list_admin() {
        if info.stream_id == stream_id {
            continue;
        }
        let Some(entry) = reg.get(&info.stream_id) else {
            continue;
        };
        if entry.owner_key != me.owner_key {
            continue;
        }
        // Last received event (wall clock) from the in-memory journal index.
        let last_wall = entry
            .journal
            .read()
            .await
            .index
            .last()
            .map(|e| e.wall_ts)
            .unwrap_or(0);
        let idle_secs = now.saturating_sub(last_wall);
        out.push(serde_json::json!({
            "player": entry.player_name,
            "server": entry.server,
            "game": entry.game,
            "public": entry.public_stream,
            "connected": entry.client_connected.load(std::sync::atomic::Ordering::Relaxed),
            "idle_secs": idle_secs,
            "active": idle_secs <= 30,
            "path": format!("/stream/{}?vtok={}", entry.stream_id, entry.view_token),
        }));
    }
    Json(out).into_response()
}

// ── Named-NPC curation ────────────────────────────────────────────────────────

#[derive(serde::Deserialize)]
struct SetMobOverrideBody {
    name: String,
    /// "named" | "trash" | "auto" (auto clears the override).
    kind: String,
}

/// `GET /stream/:id/mob_overrides?vtok=…` — list the curated set. A valid
/// view token OR a public stream reads it; the public player page needs the
/// list to label pulls the same way the private page does.
async fn list_mob_overrides_handler(
    Path(stream_id): Path<String>,
    axum::extract::Query(params): axum::extract::Query<viewer::ViewQuery>,
    State(state): State<ServerState>,
) -> Response {
    let overrides = {
        let reg = state.registry.read().await;
        match reg.get(&stream_id) {
            None => return StatusCode::NOT_FOUND.into_response(),
            Some(entry) => {
                let valid = entry.public_stream
                    || params
                        .vtok
                        .as_deref()
                        .map(|t| froklog::auth::tokens_match(&entry.view_token, t))
                        .unwrap_or(false);
                if !valid {
                    return StatusCode::UNAUTHORIZED.into_response();
                }
                Arc::clone(&entry.mob_overrides)
            }
        }
    };
    match overrides.list() {
        Ok(list) => Json(list).into_response(),
        Err(e) => {
            warn!("MobOverrides [{stream_id}]: list failed: {e}");
            StatusCode::INTERNAL_SERVER_ERROR.into_response()
        }
    }
}

/// `GET /player/:game/:server/:name/mob_overrides` — the public page's route
/// to the same list; only resolves for streams marked public.
async fn player_mob_overrides_handler(
    Path((game, server, name)): Path<(String, String, String)>,
    State(state): State<ServerState>,
) -> Response {
    let overrides = {
        let reg = state.registry.read().await;
        let Some(id) = reg.find_id_by_player(&game, &server, &name) else {
            return StatusCode::NOT_FOUND.into_response();
        };
        match reg.get(&id) {
            Some(entry) if entry.public_stream => Arc::clone(&entry.mob_overrides),
            _ => return StatusCode::NOT_FOUND.into_response(),
        }
    };
    match overrides.list() {
        Ok(list) => Json(list).into_response(),
        Err(e) => {
            warn!("MobOverrides [{game}/{server}/{name}]: list failed: {e}");
            StatusCode::INTERNAL_SERVER_ERROR.into_response()
        }
    }
}

/// `POST /stream/:id/mob_override?vtok=…` with `{name, kind}` — curate one
/// NPC. Unlike markers, the write is done from the viewer page, which holds
/// the VIEW token — so a valid view token authorizes it (as does the stream
/// or admin token). The public tokenless page gets no write path.
async fn set_mob_override_handler(
    Path(stream_id): Path<String>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    axum::extract::Query(params): axum::extract::Query<viewer::ViewQuery>,
    headers: HeaderMap,
    State(state): State<ServerState>,
    Json(body): Json<SetMobOverrideBody>,
) -> Response {
    let ip = client_ip(&headers, peer);
    let name = body.name.trim();
    if name.is_empty() || name.len() > 100 {
        return StatusCode::BAD_REQUEST.into_response();
    }
    if !matches!(body.kind.as_str(), "named" | "trash" | "auto") {
        return StatusCode::BAD_REQUEST.into_response();
    }
    let overrides = {
        let reg = state.registry.read().await;
        match reg.get(&stream_id) {
            None => return StatusCode::NOT_FOUND.into_response(),
            Some(entry) => {
                let by_vtok = params
                    .vtok
                    .as_deref()
                    .map(|t| froklog::auth::tokens_match(&entry.view_token, t))
                    .unwrap_or(false);
                let by_bearer = ingest::extract_bearer(&headers)
                    .map(|t| {
                        froklog::auth::tokens_match(&entry.stream_token, &t)
                            || state.is_admin_token(&t)
                    })
                    .unwrap_or(false);
                if !by_vtok && !by_bearer {
                    warn!("MobOverride [{stream_id}] [{ip}]: bad or missing token");
                    return StatusCode::UNAUTHORIZED.into_response();
                }
                Arc::clone(&entry.mob_overrides)
            }
        }
    };
    let result = if body.kind == "auto" {
        overrides.clear(name).map(|_| ())
    } else {
        overrides.set(name, &body.kind)
    };
    match result {
        Ok(()) => {
            info!("MobOverride [{stream_id}]: \"{name}\" -> {}", body.kind);
            StatusCode::NO_CONTENT.into_response()
        }
        Err(e) => {
            warn!("MobOverride [{stream_id}]: set failed: {e}");
            StatusCode::INTERNAL_SERVER_ERROR.into_response()
        }
    }
}

/// `GET /stream/:id/segment_members?vtok=…` — every per-segment exclusion for
/// this stream. Fetched once; the page applies them per segment as you scroll.
async fn list_segment_members_handler(
    Path(stream_id): Path<String>,
    axum::extract::Query(params): axum::extract::Query<viewer::ViewQuery>,
    State(state): State<ServerState>,
) -> Response {
    let roster = {
        let reg = state.registry.read().await;
        match reg.get(&stream_id) {
            None => return StatusCode::NOT_FOUND.into_response(),
            Some(entry) => {
                let valid = entry.public_stream
                    || params
                        .vtok
                        .as_deref()
                        .map(|t| froklog::auth::tokens_match(&entry.view_token, t))
                        .unwrap_or(false);
                if !valid {
                    return StatusCode::UNAUTHORIZED.into_response();
                }
                Arc::clone(&entry.segment_roster)
            }
        }
    };
    match roster.list() {
        Ok(list) => Json(list).into_response(),
        Err(e) => {
            warn!("SegmentRoster [{stream_id}]: list failed: {e}");
            StatusCode::INTERNAL_SERVER_ERROR.into_response()
        }
    }
}

/// `GET /player/:game/:server/:name/segment_members` — same list for the
/// public page, which needs it to aggregate a segment the same way.
async fn player_segment_members_handler(
    Path((game, server, name)): Path<(String, String, String)>,
    State(state): State<ServerState>,
) -> Response {
    let roster = {
        let reg = state.registry.read().await;
        let Some(id) = reg.find_id_by_player(&game, &server, &name) else {
            return StatusCode::NOT_FOUND.into_response();
        };
        match reg.get(&id) {
            Some(entry) if entry.public_stream => Arc::clone(&entry.segment_roster),
            _ => return StatusCode::NOT_FOUND.into_response(),
        }
    };
    match roster.list() {
        Ok(list) => Json(list).into_response(),
        Err(e) => {
            warn!("SegmentRoster [{game}/{server}/{name}]: list failed: {e}");
            StatusCode::INTERNAL_SERVER_ERROR.into_response()
        }
    }
}

#[derive(serde::Deserialize)]
struct SegmentMemberBody {
    /// Log timestamp the segment starts at.
    seg_ts: u64,
    name: String,
    /// true = counts toward the aggregate (removes any exclusion row).
    included: bool,
}

/// `POST /stream/:id/segment_member?vtok=…` — include or exclude one player
/// from one segment's aggregate. Written from the viewer page, so a valid
/// view token authorizes it, as does the stream or admin token.
async fn set_segment_member_handler(
    Path(stream_id): Path<String>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    axum::extract::Query(params): axum::extract::Query<viewer::ViewQuery>,
    headers: HeaderMap,
    State(state): State<ServerState>,
    Json(body): Json<SegmentMemberBody>,
) -> Response {
    let ip = client_ip(&headers, peer);
    let name = body.name.trim();
    if name.is_empty() || name.len() > 64 {
        return StatusCode::BAD_REQUEST.into_response();
    }
    let roster = {
        let reg = state.registry.read().await;
        match reg.get(&stream_id) {
            None => return StatusCode::NOT_FOUND.into_response(),
            Some(entry) => {
                let by_vtok = params
                    .vtok
                    .as_deref()
                    .map(|t| froklog::auth::tokens_match(&entry.view_token, t))
                    .unwrap_or(false);
                let by_bearer = ingest::extract_bearer(&headers)
                    .map(|t| {
                        froklog::auth::tokens_match(&entry.stream_token, &t)
                            || state.is_admin_token(&t)
                    })
                    .unwrap_or(false);
                if !by_vtok && !by_bearer {
                    warn!("SegmentMember [{stream_id}] [{ip}]: bad or missing token");
                    return StatusCode::UNAUTHORIZED.into_response();
                }
                Arc::clone(&entry.segment_roster)
            }
        }
    };
    let result = if body.included {
        roster.include(body.seg_ts, name).map(|_| ())
    } else {
        roster.exclude(body.seg_ts, name)
    };
    match result {
        Ok(()) => StatusCode::NO_CONTENT.into_response(),
        Err(e) => {
            warn!("SegmentMember [{stream_id}]: write failed: {e}");
            StatusCode::INTERNAL_SERVER_ERROR.into_response()
        }
    }
}

/// Resolve the stream's markers handle for a write.
///
/// A valid VIEW token authorizes it, as does the stream or admin token.
/// Marking "raid start" and "raid end" is done from the viewer page, which
/// holds only the view token — the same reasoning that already applies to
/// mob curation. The public tokenless page still gets no write path.
/// `None` = unauthorized or unknown stream.
async fn owner_markers(
    state: &ServerState,
    stream_id: &str,
    headers: &HeaderMap,
    vtok: Option<&str>,
) -> Option<markers::SharedMarkers> {
    let reg = state.registry.read().await;
    let entry = reg.get(stream_id)?;
    let by_vtok = vtok
        .map(|t| froklog::auth::tokens_match(&entry.view_token, t))
        .unwrap_or(false);
    let by_bearer = ingest::extract_bearer(headers)
        .map(|t| froklog::auth::tokens_match(&entry.stream_token, &t) || state.is_admin_token(&t))
        .unwrap_or(false);
    if by_vtok || by_bearer {
        Some(Arc::clone(&entry.markers))
    } else {
        None
    }
}

// ── HTTP stats snapshot (poll fallback for viewers) ───────────────────────────

async fn stream_stats_handler(
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
    Path(stream_id): Path<String>,
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

// ── Favicon ───────────────────────────────────────────────────────────────────

async fn favicon_handler() -> impl IntoResponse {
    static FAVICON: &[u8] = include_bytes!("../../../assets/froklog-green.png");
    ([(axum::http::header::CONTENT_TYPE, "image/png")], FAVICON)
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
    #[serde(default = "default_eql")]
    game: String,
    #[serde(default)]
    server: String,
    player: String,
    #[serde(default)]
    public_stream: bool,
    #[serde(default)]
    is_replay: bool,
    #[serde(default)]
    owner_key: String,
}

fn default_eql() -> String {
    "eql".to_string()
}

async fn load_persisted_streams(data_dir: &std::path::Path, registry: &SharedRegistry) {
    let Ok(entries) = std::fs::read_dir(data_dir) else {
        return;
    };

    // Collect all valid metas with their meta.json modification time.
    let mut metas: Vec<(std::time::SystemTime, StreamMeta)> = Vec::new();
    for entry in entries.flatten() {
        let meta_path = entry.path().join("meta.json");
        if !meta_path.exists() {
            continue;
        }
        let mtime = std::fs::metadata(&meta_path)
            .and_then(|m| m.modified())
            .unwrap_or(std::time::SystemTime::UNIX_EPOCH);
        let Ok(raw) = std::fs::read_to_string(&meta_path) else {
            continue;
        };
        let Ok(meta) = serde_json::from_str::<StreamMeta>(&raw) else {
            tracing::warn!("Skipping malformed meta at {}", meta_path.display());
            continue;
        };
        metas.push((mtime, meta));
    }

    // Sort private-before-public, older-before-newer so the most recently
    // active public stream is inserted last and wins the name_to_id slot
    // when multiple streams share the same (game, server, player) identity.
    metas.sort_by(|(at, a), (bt, b)| a.public_stream.cmp(&b.public_stream).then(at.cmp(bt)));

    let mut loaded = 0usize;
    for (_, meta) in metas {
        match StreamEntry::new(
            meta.stream_id.clone(),
            meta.stream_token,
            meta.view_token,
            meta.game,
            meta.server,
            meta.player.clone(),
            meta.public_stream,
            meta.is_replay,
            meta.owner_key,
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
