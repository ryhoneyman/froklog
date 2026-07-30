use std::net::SocketAddr;
use std::sync::Arc;

use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::{ConnectInfo, Path, State};
use axum::http::{header, HeaderMap, StatusCode};
use axum::response::IntoResponse;
use tracing::{info, warn};

use crate::session_index::{SessionEntry, SharedSessionIndex, RECONNECT_GAP_SECS};
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

    // Clone the per-stream handles under a short registry read lock and drop
    // it immediately. The registry lock must never be held across journal
    // disk writes: it is FIFO-fair, so one slow history import holding it
    // would queue a writer and block every other request on the server.
    let handles = {
        let reg = state.registry.read().await;
        reg.get(&stream_id).map(|e| {
            (
                Arc::clone(&e.client_connected),
                Arc::clone(&e.journal),
                Arc::clone(&e.session_index),
                e.broadcast_tx.clone(),
            )
        })
    };
    let Some((connected_flag, journal, session_index, broadcast_tx)) = handles else {
        warn!("Ingest [{stream_id}]: stream no longer exists");
        return;
    };
    connected_flag.store(true, std::sync::atomic::Ordering::Relaxed);

    // WS reconnect trigger: cut a session if the journal has content and the
    // last batch arrived more than RECONNECT_GAP_SECS ago (player was gone).
    // Skip when the last batch was historical replay data (wall_ts >> log_ts):
    // in that case the gap is between replay uploads, not actual gameplay breaks.
    {
        let (journal_len, next_id, last_wall_ts, last_log_ts) = {
            let j = journal.read().await;
            (j.len(), j.next_batch_id(), j.last_ts(), j.log_last_ts())
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
                cut_session(&session_index, next_id, wall_ts, wall_ts, &stream_id).await;
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
                // from the WS reconnect at this same batch position, deduplicate.
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

                // Anchor the session to the id the next appended batch will get,
                // so the session starts at the batch containing the Login event.
                if let Some(log_ts) = login_ts {
                    let start_id = journal.read().await.next_batch_id();
                    let last_session_id = session_index.read().await.last_start_id();
                    // Deduplicate: skip if the reconnect trigger already cut at
                    // this exact batch position.
                    if last_session_id != Some(start_id) {
                        cut_session(&session_index, start_id, log_ts, wall_ts, &stream_id).await;
                    }
                }

                for sub in sub_batches {
                    let log_ts = sub.max_log_ts();
                    match serde_json::to_string(&sub) {
                        Ok(sub_json) => {
                            let arc_json = Arc::new(sub_json);
                            {
                                let mut j = journal.write().await;
                                if let Err(e) = j.append(wall_ts, log_ts, sub.seq, &arc_json) {
                                    warn!("Journal [{stream_id}]: write error: {e}");
                                }
                            }
                            // broadcast_tx.send fails only when there are no
                            // subscribers, which is fine (no viewers connected).
                            let _ = broadcast_tx.send(arc_json);
                        }
                        Err(e) => warn!("Ingest [{stream_id}]: serialise sub-batch: {e}"),
                    }
                }
            }
            Ok(Message::Close(_)) | Err(_) => break,
            _ => {}
        }
    }

    warn!("Ingest [{stream_id}]: client disconnected");
    connected_flag.store(false, std::sync::atomic::Ordering::Relaxed);
}

/// Append a new entry to the stream's session index.
async fn cut_session(
    session_index: &SharedSessionIndex,
    start_batch_id: i64,
    start_log_ts: u64,
    start_wall_ts: u64,
    stream_id: &str,
) {
    let mut si = session_index.write().await;
    let num = si.len() as u32 + 1;
    let session = SessionEntry {
        num,
        start_batch_id,
        start_log_ts,
        start_wall_ts,
        label: crate::session_index::format_label(start_log_ts),
    };
    if let Err(e) = si.append(session) {
        warn!("SessionIndex [{stream_id}]: failed to write session {num}: {e}");
    } else {
        info!("SessionIndex [{stream_id}]: session {num} starts at batch id {start_batch_id}");
    }
}

pub(crate) fn extract_bearer(headers: &HeaderMap) -> Option<String> {
    let auth = headers.get(header::AUTHORIZATION)?.to_str().ok()?;
    auth.strip_prefix("Bearer ").map(str::to_owned)
}
