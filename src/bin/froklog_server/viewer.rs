use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use std::net::SocketAddr;

use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::{ConnectInfo, Path, Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{Html, IntoResponse, Response};
use serde::{Deserialize, Serialize};
use tokio::time::sleep;
use tracing::{debug, info};

use crate::journal::SharedJournal;
use crate::ServerState;

/// Number of journal batches to send per loop iteration during catch-up.
/// Amortises per-batch async overhead without affecting replay granularity.
const CATCHUP_BURST: usize = 32;

#[derive(Deserialize)]
pub struct ViewQuery {
    pub vtok: Option<String>,
}

/// `GET /stream/:id?vtok=<view_token>` — serves the viewer HTML page.
pub async fn stream_page_handler(
    Path(stream_id): Path<String>,
    Query(params): Query<ViewQuery>,
    State(state): State<ServerState>,
) -> Response {
    if !is_valid_view_token(&stream_id, &params.vtok, &state).await {
        return StatusCode::UNAUTHORIZED.into_response();
    }
    let vtok = params.vtok.unwrap_or_default();
    let player_name = {
        let reg = state.registry.read().await;
        reg.get(&stream_id)
            .map(|e| e.player_name.clone())
            .unwrap_or_default()
    };
    let html = include_str!("../../../static/stream.html")
        .replace("__STREAM_ID__", &stream_id)
        .replace("__VIEW_TOKEN__", &vtok)
        .replace("__PLAYER_NAME__", &player_name);
    Html(html).into_response()
}

/// `GET /stream/:id/ws?vtok=<view_token>` — viewer WebSocket.
pub async fn stream_ws_handler(
    Path(stream_id): Path<String>,
    Query(params): Query<ViewQuery>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
    ws: WebSocketUpgrade,
    State(state): State<ServerState>,
) -> Response {
    if !is_valid_view_token(&stream_id, &params.vtok, &state).await {
        return StatusCode::UNAUTHORIZED.into_response();
    }

    // Capture handles before the upgrade so we never miss a live batch.
    let (rx, journal, client_connected) = {
        let reg = state.registry.read().await;
        let entry = reg.get(&stream_id).expect("validated above");
        (
            entry.broadcast_tx.subscribe(),
            Arc::clone(&entry.journal),
            Arc::clone(&entry.client_connected),
        )
    };

    info!(
        "Viewer [{stream_id}]: client connected from {}",
        crate::client_ip(&headers, peer)
    );
    ws.on_upgrade(move |socket| handle_viewer_ws(socket, rx, journal, client_connected))
        .into_response()
}

// ── WS control messages ───────────────────────────────────────────────────────

/// Messages the client sends to the server.
#[derive(Deserialize, Debug)]
#[serde(tag = "t", rename_all = "snake_case")]
enum ClientMsg {
    /// Jump to a specific wall-clock unix timestamp and play at `speed` ×
    /// realtime (1.0 = normal, 2.0 = 2×, 0.5 = half-speed).
    Seek { to_ts: u64, speed: f64 },
    /// Seek to a position but remain paused (no batches sent).
    SeekPaused { to_ts: u64 },
    /// Replay all history from the beginning up to `to_ts` at full speed,
    /// then continue replaying at `speed` × realtime from that point.
    /// This preserves fully-accumulated stats at the seek position.
    SeekFull { to_ts: u64, speed: f64 },
    /// Like SeekFull but remain paused once caught up to `to_ts`.
    SeekFullPaused { to_ts: u64 },
    /// Like SeekFull but seeks by EQ log-event timestamp instead of wall-clock.
    SeekFullLog { to_log_ts: u64, speed: f64 },
    /// Like SeekFullPaused but seeks by EQ log-event timestamp.
    SeekFullPausedLog { to_log_ts: u64 },
    /// Direct seek by EQ log-event timestamp — jumps to position and replays
    /// without re-streaming history.  The client is responsible for rebuilding
    /// state from its own in-memory batch cache before sending this.
    SeekLog { to_log_ts: u64, speed: f64 },
    /// Like SeekLog but remains paused at the target position.
    SeekPausedLog { to_log_ts: u64 },
    /// Pause replay at the current position.
    Pause,
    /// Resume replay from the paused position at `speed` × realtime.
    Resume { speed: f64 },
    /// Abandon replay and switch to live tail.
    Live,
}

/// Messages the server sends to the client.
#[derive(Serialize)]
#[serde(tag = "t", rename_all = "snake_case")]
enum ServerMsg {
    /// Timeline metadata — sent once on connect.
    Info {
        first_ts: Option<u64>,
        last_ts: Option<u64>,
        /// EQ log-event timestamp of the first journal entry.
        log_first_ts: Option<u64>,
        /// EQ log-event timestamp of the last journal entry.
        log_last_ts: Option<u64>,
        batch_count: usize,
        /// True while a client (e.g. froklog-replay) is actively pushing to
        /// this stream.  False means the stream is a completed recording and
        /// no further live data will arrive.
        is_live: bool,
    },
    /// Acknowledges a seek operation and confirms the timeline bounds.
    SeekOk {
        first_ts: Option<u64>,
        last_ts: Option<u64>,
    },
    /// Sent when the server transitions from replay to live mode.
    NowLive,
    /// Sent when catch-up completes on a stream whose client is no longer
    /// connected — signals that no further live data will arrive.
    StreamDone,
}

/// Wrap a raw EventBatch JSON string in our WS envelope without re-parsing.
fn wrap_batch(json: &str) -> String {
    format!(r#"{{"t":"batch","batch":{}}}"#, json)
}

// ── State machine ─────────────────────────────────────────────────────────────

enum ViewerMode {
    /// Sending all stored journal batches as fast as the socket allows,
    /// then transitioning to Live.
    CatchUp { pos: usize },
    /// Sending all stored batches from `pos` up to (but not including)
    /// `target_pos` as fast as possible, then transitioning to:
    ///   - `Replay { target_pos, speed }` if `then_speed` is `Some(speed)`
    ///   - `Paused { target_pos }` if `then_speed` is `None`
    CatchUpTo {
        pos: usize,
        target_pos: usize,
        then_speed: Option<f64>,
    },
    /// Subscribed to the live broadcast channel.
    Live,
    /// Replaying stored batches paced at `speed` × wall-clock rate.
    Replay { pos: usize, speed: f64 },
    /// Paused at a specific journal position; waiting for Resume or Seek.
    Paused { pos: usize },
}

// ── Handler ───────────────────────────────────────────────────────────────────

async fn handle_viewer_ws(
    mut socket: WebSocket,
    mut live_rx: tokio::sync::broadcast::Receiver<Arc<String>>,
    journal: SharedJournal,
    client_connected: Arc<AtomicBool>,
) {
    // Send timeline info so the client can render the seek bar.
    {
        let j = journal.read().await;
        let info = ServerMsg::Info {
            first_ts: j.first_ts(),
            last_ts: j.last_ts(),
            log_first_ts: j.log_first_ts(),
            log_last_ts: j.log_last_ts(),
            batch_count: j.len(),
            is_live: client_connected.load(Ordering::Relaxed),
        };
        if let Ok(txt) = serde_json::to_string(&info) {
            if socket.send(Message::Text(txt.into())).await.is_err() {
                return;
            }
        }
    }

    // Start by replaying the full history, then follow live.
    let mut mode = ViewerMode::CatchUp { pos: 0 };

    loop {
        match mode {
            // ── Catch-up-to: blast stored batches until target, then transition
            ViewerMode::CatchUpTo {
                ref mut pos,
                target_pos,
                then_speed,
            } => {
                if *pos >= target_pos {
                    mode = match then_speed {
                        Some(speed) => ViewerMode::Replay {
                            pos: target_pos,
                            speed,
                        },
                        None => ViewerMode::Paused { pos: target_pos },
                    };
                    continue;
                }
                // Burst-send up to CATCHUP_BURST batches before yielding for
                // client messages, amortising per-batch async overhead.
                let mut exhausted = false;
                for _ in 0..CATCHUP_BURST {
                    if *pos >= target_pos {
                        break;
                    }
                    let batch = {
                        let j = journal.read().await;
                        j.read_at(*pos)
                    };
                    if let Some(json) = batch {
                        if socket
                            .send(Message::Text(wrap_batch(&json).into()))
                            .await
                            .is_err()
                        {
                            return;
                        }
                        *pos += 1;
                    } else {
                        exhausted = true;
                        break;
                    }
                }
                if exhausted {
                    let cur = *pos;
                    mode = match then_speed {
                        Some(speed) => ViewerMode::Replay { pos: cur, speed },
                        None => ViewerMode::Paused { pos: cur },
                    };
                    continue;
                }
                // Non-blocking check for client control messages.
                let cur_pos = *pos;
                if let Ok(Some(Ok(Message::Text(txt)))) =
                    tokio::time::timeout(Duration::ZERO, socket.recv()).await
                {
                    if let Some(new_mode) =
                        handle_client_msg(&txt, &journal, &mut socket, cur_pos).await
                    {
                        mode = new_mode;
                    }
                }
            }

            // ── Catch-up: blast stored batches, yield to client msgs ──────────
            ViewerMode::CatchUp { ref mut pos } => {
                // Burst-send up to CATCHUP_BURST batches before yielding.
                let mut end_of_journal = false;
                for _ in 0..CATCHUP_BURST {
                    let batch = {
                        let j = journal.read().await;
                        j.read_at(*pos)
                    };
                    if let Some(json) = batch {
                        if socket
                            .send(Message::Text(wrap_batch(&json).into()))
                            .await
                            .is_err()
                        {
                            return;
                        }
                        *pos += 1;
                    } else {
                        end_of_journal = true;
                        break;
                    }
                }
                if end_of_journal {
                    // Caught up to the end of stored data.
                    if !client_connected.load(Ordering::Relaxed) {
                        // Stream is a completed recording; no live data will
                        // follow.  Tell the client and pause at the end.
                        if let Ok(txt) = serde_json::to_string(&ServerMsg::StreamDone) {
                            let _ = socket.send(Message::Text(txt.into())).await;
                        }
                        mode = ViewerMode::Paused { pos: *pos };
                    } else {
                        mode = ViewerMode::Live;
                    }
                    continue;
                }
                // Non-blocking check for client control messages.
                if let Ok(Some(Ok(Message::Text(txt)))) =
                    tokio::time::timeout(Duration::ZERO, socket.recv()).await
                {
                    if let Some(new_mode) =
                        handle_client_msg(&txt, &journal, &mut socket, *pos).await
                    {
                        mode = new_mode;
                    }
                }
            }

            // ── Live: fan-out from the broadcast channel ──────────────────────
            ViewerMode::Live => {
                tokio::select! {
                    result = live_rx.recv() => {
                        match result {
                            Ok(json) => {
                                if socket
                                    .send(Message::Text(wrap_batch(&json).into()))
                                    .await
                                    .is_err()
                                {
                                    return;
                                }
                            }
                            Err(tokio::sync::broadcast::error::RecvError::Lagged(n)) => {
                                debug!("Viewer WS lagged {n} frames");
                            }
                            Err(tokio::sync::broadcast::error::RecvError::Closed) => return,
                        }
                    }
                    msg = socket.recv() => {
                        match msg {
                            Some(Ok(Message::Text(txt))) => {
                                if let Some(new_mode) =
                                    handle_client_msg(&txt, &journal, &mut socket, {
                                        let j = journal.read().await;
                                        j.len()
                                    }).await
                                {
                                    mode = new_mode;
                                }
                            }
                            None | Some(Ok(Message::Close(_))) | Some(Err(_)) => return,
                            _ => {}
                        }
                    }
                    // Periodically check whether the ingest client has disconnected.
                    // If so, the stream is now a completed recording — notify the
                    // viewer so it can stop waiting for data that will never arrive.
                    _ = sleep(Duration::from_secs(2)) => {
                        if !client_connected.load(Ordering::Relaxed) {
                            let pos = { journal.read().await.len() };
                            if let Ok(txt) = serde_json::to_string(&ServerMsg::StreamDone) {
                                let _ = socket.send(Message::Text(txt.into())).await;
                            }
                            mode = ViewerMode::Paused { pos };
                        }
                    }
                }
            }

            // ── Replay: pace stored batches by speed × log-time delta ─────────
            ViewerMode::Replay { ref mut pos, speed } => {
                let (batch, cur_log_ts, next_log_ts) = {
                    let j = journal.read().await;
                    (j.read_at(*pos), j.log_ts_at(*pos), j.log_ts_at(*pos + 1))
                };

                if let Some(json) = batch {
                    if socket
                        .send(Message::Text(wrap_batch(&json).into()))
                        .await
                        .is_err()
                    {
                        return;
                    }
                    *pos += 1;

                    // Pace to the next batch: delay = Δlog_ts / speed.
                    // log_ts reflects actual EQ log time so 1× plays back at
                    // the same rate the events originally occurred.
                    if let (Some(ct), Some(nt)) = (cur_log_ts, next_log_ts) {
                        let delta = (nt.saturating_sub(ct)) as f64 / speed.max(0.01);
                        let wait = Duration::from_secs_f64(delta.min(30.0));
                        if !wait.is_zero() {
                            tokio::select! {
                                _ = sleep(wait) => {}
                                msg = socket.recv() => {
                                    match msg {
                                        Some(Ok(Message::Text(txt))) => {
                                            if let Some(new_mode) =
                                                handle_client_msg(&txt, &journal, &mut socket, *pos).await
                                            {
                                                mode = new_mode;
                                            }
                                        }
                                        None | Some(Ok(Message::Close(_))) | Some(Err(_)) => return,
                                        _ => {}
                                    }
                                }
                            }
                        }
                    }
                } else {
                    // Replay reached the end of stored data.
                    if client_connected.load(Ordering::Relaxed) {
                        // Still a live stream — switch to live fan-out.
                        if let Ok(txt) = serde_json::to_string(&ServerMsg::NowLive) {
                            let _ = socket.send(Message::Text(txt.into())).await;
                        }
                        mode = ViewerMode::Live;
                    } else {
                        // Completed recording — tell the client and pause at end.
                        if let Ok(txt) = serde_json::to_string(&ServerMsg::StreamDone) {
                            let _ = socket.send(Message::Text(txt.into())).await;
                        }
                        mode = ViewerMode::Paused { pos: *pos };
                    }
                }
            }

            // ── Paused: hold position, wait for Resume / Seek ────────────────────────
            ViewerMode::Paused { pos } => match socket.recv().await {
                Some(Ok(Message::Text(txt))) => {
                    if let Some(new_mode) =
                        handle_client_msg(&txt, &journal, &mut socket, pos).await
                    {
                        mode = new_mode;
                    }
                }
                None | Some(Ok(Message::Close(_))) | Some(Err(_)) => return,
                _ => {}
            },
        }
    }
}

/// Parse a client control message and return the new ViewerMode if it changed.
async fn handle_client_msg(
    txt: &str,
    journal: &SharedJournal,
    socket: &mut WebSocket,
    current_pos: usize,
) -> Option<ViewerMode> {
    let msg: ClientMsg = serde_json::from_str(txt).ok()?;
    match msg {
        ClientMsg::Seek { to_ts, speed } => {
            let (pos, first_ts, last_ts) = {
                let j = journal.read().await;
                (j.seek_index(to_ts), j.first_ts(), j.last_ts())
            };
            let ack = ServerMsg::SeekOk { first_ts, last_ts };
            if let Ok(txt) = serde_json::to_string(&ack) {
                let _ = socket.send(Message::Text(txt.into())).await;
            }
            let speed = speed.clamp(0.1, 32.0);
            Some(ViewerMode::Replay { pos, speed })
        }
        ClientMsg::SeekPaused { to_ts } => {
            let (pos, first_ts, last_ts) = {
                let j = journal.read().await;
                (j.seek_index(to_ts), j.first_ts(), j.last_ts())
            };
            let ack = ServerMsg::SeekOk { first_ts, last_ts };
            if let Ok(txt) = serde_json::to_string(&ack) {
                let _ = socket.send(Message::Text(txt.into())).await;
            }
            Some(ViewerMode::Paused { pos })
        }
        ClientMsg::SeekFull { to_ts, speed } => {
            let (target_pos, first_ts, last_ts) = {
                let j = journal.read().await;
                (j.seek_index(to_ts), j.first_ts(), j.last_ts())
            };
            let ack = ServerMsg::SeekOk { first_ts, last_ts };
            if let Ok(txt) = serde_json::to_string(&ack) {
                let _ = socket.send(Message::Text(txt.into())).await;
            }
            let speed = speed.clamp(0.1, 32.0);
            Some(ViewerMode::CatchUpTo {
                pos: 0,
                target_pos,
                then_speed: Some(speed),
            })
        }
        ClientMsg::SeekFullPaused { to_ts } => {
            let (target_pos, first_ts, last_ts) = {
                let j = journal.read().await;
                (j.seek_index(to_ts), j.first_ts(), j.last_ts())
            };
            let ack = ServerMsg::SeekOk { first_ts, last_ts };
            if let Ok(txt) = serde_json::to_string(&ack) {
                let _ = socket.send(Message::Text(txt.into())).await;
            }
            Some(ViewerMode::CatchUpTo {
                pos: 0,
                target_pos,
                then_speed: None,
            })
        }
        ClientMsg::SeekFullLog { to_log_ts, speed } => {
            let (target_pos, first_ts, last_ts) = {
                let j = journal.read().await;
                (j.seek_index_by_log_ts(to_log_ts), j.first_ts(), j.last_ts())
            };
            let ack = ServerMsg::SeekOk { first_ts, last_ts };
            if let Ok(txt) = serde_json::to_string(&ack) {
                let _ = socket.send(Message::Text(txt.into())).await;
            }
            let speed = speed.clamp(0.1, 32.0);
            Some(ViewerMode::CatchUpTo {
                pos: 0,
                target_pos,
                then_speed: Some(speed),
            })
        }
        ClientMsg::SeekFullPausedLog { to_log_ts } => {
            let (target_pos, first_ts, last_ts) = {
                let j = journal.read().await;
                (j.seek_index_by_log_ts(to_log_ts), j.first_ts(), j.last_ts())
            };
            let ack = ServerMsg::SeekOk { first_ts, last_ts };
            if let Ok(txt) = serde_json::to_string(&ack) {
                let _ = socket.send(Message::Text(txt.into())).await;
            }
            Some(ViewerMode::CatchUpTo {
                pos: 0,
                target_pos,
                then_speed: None,
            })
        }
        ClientMsg::SeekLog { to_log_ts, speed } => {
            // Direct seek — client has already rebuilt state from its cache.
            // Jump to the position and start replaying without re-streaming.
            let (pos, first_ts, last_ts) = {
                let j = journal.read().await;
                (j.seek_index_by_log_ts(to_log_ts), j.first_ts(), j.last_ts())
            };
            let ack = ServerMsg::SeekOk { first_ts, last_ts };
            if let Ok(txt) = serde_json::to_string(&ack) {
                let _ = socket.send(Message::Text(txt.into())).await;
            }
            let speed = speed.clamp(0.1, 32.0);
            Some(ViewerMode::Replay { pos, speed })
        }
        ClientMsg::SeekPausedLog { to_log_ts } => {
            // Direct seek — client has already rebuilt state from its cache.
            let (pos, first_ts, last_ts) = {
                let j = journal.read().await;
                (j.seek_index_by_log_ts(to_log_ts), j.first_ts(), j.last_ts())
            };
            let ack = ServerMsg::SeekOk { first_ts, last_ts };
            if let Ok(txt) = serde_json::to_string(&ack) {
                let _ = socket.send(Message::Text(txt.into())).await;
            }
            Some(ViewerMode::Paused { pos })
        }
        ClientMsg::Pause => Some(ViewerMode::Paused { pos: current_pos }),
        ClientMsg::Resume { speed } => {
            let speed = speed.clamp(0.1, 32.0);
            Some(ViewerMode::Replay {
                pos: current_pos,
                speed,
            })
        }
        ClientMsg::Live => Some(ViewerMode::Live),
    }
}

async fn is_valid_view_token(stream_id: &str, vtok: &Option<String>, state: &ServerState) -> bool {
    let reg = state.registry.read().await;
    match reg.get(stream_id) {
        Some(entry) => vtok
            .as_deref()
            .map(|t| froklog::auth::tokens_match(&entry.view_token, t))
            .unwrap_or(false),
        None => false,
    }
}
