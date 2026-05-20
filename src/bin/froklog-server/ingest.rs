use std::net::SocketAddr;
use std::sync::Arc;

use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::{ConnectInfo, Path, State};
use axum::http::{header, HeaderMap, StatusCode};
use axum::response::IntoResponse;
use tracing::{info, warn};

use crate::session_index::{SessionEntry, RECONNECT_GAP_SECS};
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
    let now_secs = || {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs()
    };

    // Mark client connected and check for a reconnect-gap session boundary.
    let connected_flag = {
        let reg = state.registry.read().await;
        reg.get(&stream_id).map(|e| Arc::clone(&e.client_connected))
    };
    if let Some(flag) = &connected_flag {
        flag.store(true, std::sync::atomic::Ordering::Relaxed);
    }

    // WS reconnect trigger: cut a session if the journal has content and the
    // last batch arrived more than RECONNECT_GAP_SECS ago (player was gone).
    // Skip when the last batch was historical replay data (wall_ts >> log_ts):
    // in that case the gap is between replay uploads, not actual gameplay breaks.
    {
        let reg = state.registry.read().await;
        if let Some(entry) = reg.get(&stream_id) {
            let (journal_len, last_wall_ts, last_log_ts) = {
                let j = entry.journal.read().await;
                (j.len(), j.last_ts(), j.log_last_ts())
            };
            if journal_len > 0 {
                let gap = last_wall_ts
                    .map(|t| now_secs().saturating_sub(t))
                    .unwrap_or(0);
                // Skew between server-receipt time and EQ log time: near-zero for live
                // content, hours/days for historical replays.
                let skew = last_wall_ts
                    .zip(last_log_ts)
                    .map(|(w, l)| w.saturating_sub(l))
                    .unwrap_or(0);
                if gap >= RECONNECT_GAP_SECS && skew < RECONNECT_GAP_SECS {
                    let wall_ts = now_secs();
                    cut_session(entry, journal_len, wall_ts, wall_ts, &stream_id).await;
                }
            }
        }
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

                let wall_ts = now_secs();

                // Login event trigger: a "Welcome to EverQuest Legends!" line emitted
                // by the client parser.  Takes priority — if we already cut a session
                // from the WS reconnect at this same journal position, deduplicate.
                let login_ts = batch.events.iter().find_map(|e| {
                    if let froklog::event::CombatEvent::Login { ts } = e {
                        Some(*ts as u64)
                    } else {
                        None
                    }
                });

                // Split oversized batches (e.g. from dump mode) into per-minute
                // sub-batches so the seek index has sufficient granularity.
                let sub_batches = split_by_log_time(batch);

                let reg = state.registry.read().await;
                if let Some(entry) = reg.get(&stream_id) {
                    // Record journal position before appending so the session starts
                    // at the batch that contains the Login event.
                    if let Some(log_ts) = login_ts {
                        let start_idx = entry.journal.read().await.len();
                        let last_session_idx = entry.session_index.read().await.last_start_idx();
                        // Deduplicate: skip if the reconnect trigger already cut at
                        // this exact journal position.
                        if last_session_idx != Some(start_idx) {
                            cut_session(entry, start_idx, log_ts, wall_ts, &stream_id).await;
                        }
                    }

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

/// Append a new entry to the stream's session index.
async fn cut_session(
    entry: &crate::streams::StreamEntry,
    start_journal_idx: usize,
    start_log_ts: u64,
    start_wall_ts: u64,
    stream_id: &str,
) {
    let mut si = entry.session_index.write().await;
    let num = si.len() as u32 + 1;
    let session = SessionEntry {
        num,
        start_journal_idx,
        start_log_ts,
        start_wall_ts,
        label: crate::session_index::format_label(start_log_ts),
    };
    if let Err(e) = si.append(session) {
        warn!("SessionIndex [{stream_id}]: failed to write session {num}: {e}");
    } else {
        info!(
            "SessionIndex [{stream_id}]: session {num} started at journal idx {start_journal_idx}"
        );
    }
}

pub(crate) fn extract_bearer(headers: &HeaderMap) -> Option<String> {
    let auth = headers.get(header::AUTHORIZATION)?.to_str().ok()?;
    auth.strip_prefix("Bearer ").map(str::to_owned)
}
