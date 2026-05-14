use std::net::SocketAddr;
use std::sync::Arc;

use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::{ConnectInfo, Path, State};
use axum::http::{header, HeaderMap, StatusCode};
use axum::response::IntoResponse;
use tracing::{info, warn};

use crate::ServerState;

/// Maximum EQ log-time span (seconds) allowed per journal entry.
/// Batches spanning more than this are split so that dump-mode streams
/// (where all events arrive in one giant push) still get per-minute
/// granularity in the seek index, allowing Play/Pause/Seek to work correctly.
const BATCH_SPLIT_SECS: u64 = 60;

/// Split an `EventBatch` that spans more than `BATCH_SPLIT_SECS` of EQ log
/// time into sequential sub-batches, each covering at most that window.
/// Returns the original batch unchanged when it already fits.
fn split_by_log_time(batch: froklog::event::EventBatch) -> Vec<froklog::event::EventBatch> {
    if batch.events.is_empty() {
        return vec![batch];
    }
    let min_ts = batch.events.iter().map(|e| e.ts() as u64).min().unwrap();
    let max_ts = batch.events.iter().map(|e| e.ts() as u64).max().unwrap();
    if max_ts.saturating_sub(min_ts) <= BATCH_SPLIT_SECS {
        return vec![batch];
    }
    let base_seq = batch.seq;
    let num_windows = ((max_ts - min_ts) / BATCH_SPLIT_SECS + 1) as usize;
    let mut buckets: Vec<Vec<froklog::event::CombatEvent>> = vec![Vec::new(); num_windows];
    for event in batch.events {
        let slot = ((event.ts() as u64 - min_ts) / BATCH_SPLIT_SECS) as usize;
        buckets[slot.min(num_windows - 1)].push(event);
    }
    let mut result: Vec<froklog::event::EventBatch> = Vec::new();
    for events in buckets {
        if !events.is_empty() {
            let seq = base_seq.wrapping_add(result.len() as u32);
            result.push(froklog::event::EventBatch { seq, events });
        }
    }
    result
}

/// `GET /ingest/:stream_id` — WebSocket endpoint for Windows clients.
///
/// Authentication: `Authorization: Bearer <stream_token>` request header.
/// The token is validated before the WS upgrade so a bad credential gets a
/// plain 401 HTTP response, not a WS close frame.
pub async fn ingest_ws_handler(
    Path(stream_id): Path<String>,
    headers: HeaderMap,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    ws: WebSocketUpgrade,
    State(state): State<ServerState>,
) -> impl IntoResponse {
    let token = match extract_bearer(&headers) {
        Some(t) => t,
        None => {
            warn!("Ingest [{stream_id}]: missing Authorization header");
            return StatusCode::UNAUTHORIZED.into_response();
        }
    };

    let valid = {
        let reg = state.registry.read().await;
        match reg.get(&stream_id) {
            Some(entry) => froklog::auth::tokens_match(&entry.stream_token, &token),
            None => false,
        }
    };

    if !valid {
        warn!("Ingest [{stream_id}]: invalid token");
        return StatusCode::UNAUTHORIZED.into_response();
    }

    info!(
        "Ingest [{stream_id}]: client connected from {}",
        crate::client_ip(&headers, peer)
    );
    ws.on_upgrade(move |socket| handle_ingest(socket, stream_id, state))
        .into_response()
}

async fn handle_ingest(mut socket: WebSocket, stream_id: String, state: ServerState) {
    let connected_flag = {
        let reg = state.registry.read().await;
        reg.get(&stream_id).map(|e| Arc::clone(&e.client_connected))
    };
    if let Some(flag) = &connected_flag {
        flag.store(true, std::sync::atomic::Ordering::Relaxed);
    }

    while let Some(result) = socket.recv().await {
        match result {
            Ok(Message::Text(json)) => {
                // Validate that this is a well-formed EventBatch before storing.
                let batch = match serde_json::from_str::<froklog::event::EventBatch>(&json) {
                    Ok(b) => b,
                    Err(_) => {
                        warn!("Ingest [{stream_id}]: received invalid EventBatch — skipping");
                        continue;
                    }
                };

                let wall_ts = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap_or_default()
                    .as_secs();

                // Split oversized batches (e.g. from dump mode) into per-minute
                // sub-batches so the seek index has sufficient granularity.
                let sub_batches = split_by_log_time(batch);

                let reg = state.registry.read().await;
                if let Some(entry) = reg.get(&stream_id) {
                    for sub in sub_batches {
                        let log_ts = sub.max_log_ts();
                        match serde_json::to_string(&sub) {
                            Ok(sub_json) => {
                                let arc_json = Arc::new(sub_json);
                                entry
                                    .append_journal(wall_ts, log_ts, sub.seq, &arc_json)
                                    .await;
                                // broadcast_tx.send fails only when there are no
                                // subscribers, which is fine (no viewers connected).
                                let _ = entry.broadcast_tx.send(arc_json);
                            }
                            Err(e) => warn!("Ingest [{stream_id}]: serialise sub-batch: {e}"),
                        }
                    }
                }
            }
            Ok(Message::Close(_)) | Err(_) => break,
            _ => {}
        }
    }

    warn!("Ingest [{stream_id}]: client disconnected");
    if let Some(flag) = &connected_flag {
        flag.store(false, std::sync::atomic::Ordering::Relaxed);
    }
}

pub(crate) fn extract_bearer(headers: &HeaderMap) -> Option<String> {
    let auth = headers.get(header::AUTHORIZATION)?.to_str().ok()?;
    auth.strip_prefix("Bearer ").map(str::to_owned)
}
