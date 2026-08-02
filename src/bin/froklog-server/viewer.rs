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
use axum::Json;

/// Number of journal batches to send per loop iteration during catch-up.
/// Amortises per-batch async overhead without affecting replay granularity.
const CATCHUP_BURST: usize = 64;
/// How often a viewer socket proves it is still alive. Comfortably under the
/// 60 s idle timeout typical of reverse proxies, and well under the client's
/// own staleness watchdog.
const HEARTBEAT_EVERY: Duration = Duration::from_secs(25);

#[derive(Deserialize)]
pub struct ViewQuery {
    pub vtok: Option<String>,
    /// Either a session number, or the literal `latest`.
    ///
    /// It has to be able to say "latest" symbolically. The page auto-selects
    /// the newest session on load and writes its choice into the URL; when
    /// that was a NUMBER the choice froze, so once a new session began the
    /// tab was pinned to a session that had ended — showing stale data,
    /// dropping every newer batch, and surviving refresh because the
    /// auto-select only runs when the parameter is absent. `latest`
    /// re-resolves on every load.
    pub session: Option<String>,
    /// Explicit time-slice scoping (log-time epoch seconds), e.g. from a
    /// raid_start/raid_end marker pair. Overrides `session` when present.
    pub from_ts: Option<u64>,
    pub to_ts: Option<u64>,
}

/// `GET /stream/:id?vtok=<view_token>` — serves the viewer HTML page.
pub async fn stream_page_handler(
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
    Path(stream_id): Path<String>,
    Query(params): Query<ViewQuery>,
    State(state): State<ServerState>,
) -> Response {
    let response = if !is_valid_view_token(&stream_id, &params.vtok, &state).await {
        StatusCode::UNAUTHORIZED.into_response()
    } else {
        let vtok = params.vtok.clone().unwrap_or_default();
        let (player_name, session_index) = {
            let reg = state.registry.read().await;
            let entry = reg.get(&stream_id);
            (
                entry.map(|e| e.player_name.clone()).unwrap_or_default(),
                entry.map(|e| Arc::clone(&e.session_index)),
            )
        };
        let (session_log_ts, session_end_log_ts): (u64, u64) = if params.from_ts.is_some() {
            // Marker/explicit slice: same template slots the session filter
            // uses, so the page scopes stats to the given window.
            (params.from_ts.unwrap_or(0), params.to_ts.unwrap_or(0))
        } else if let (Some(param), Some(si_arc)) = (params.session.as_deref(), session_index) {
            let si = si_arc.read().await;
            resolve_session_window(param, si.list())
        } else {
            (0, 0)
        };
        let ws_path = format!("/stream/{stream_id}/ws?vtok={vtok}");
        let sessions_path = format!("/stream/{stream_id}/sessions?vtok={vtok}");
        // player_name and vtok are client-supplied: HTML-escape for the badge
        // and JS-escape for the string literals to prevent stored/reflected XSS.
        let html = include_str!("../../../static/stream.html")
            .replace("__STREAM_ID__", &js_str_escape(&stream_id))
            .replace("__VIEW_TOKEN__", &js_str_escape(&vtok))
            .replace("__PLAYER_NAME__", &crate::admin::html_escape(&player_name))
            .replace("__WS_PATH__", &js_str_escape(&ws_path))
            .replace("__SESSIONS_PATH__", &js_str_escape(&sessions_path))
            .replace("__SESSION_LOG_TS__", &session_log_ts.to_string())
            .replace("__SESSION_END_LOG_TS__", &session_end_log_ts.to_string());
        Html(html).into_response()
    };
    info!(
        "Stream page [{stream_id}] from {} ({})",
        crate::client_ip(&headers, peer),
        response.status()
    );
    response
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
    // The stream can be purged between token validation and this lookup —
    // return 404 instead of panicking on that race.
    let handles = {
        let reg = state.registry.read().await;
        reg.get(&stream_id).map(|entry| {
            (
                entry.broadcast_tx.subscribe(),
                Arc::clone(&entry.journal),
                Arc::clone(&entry.client_connected),
            )
        })
    };
    let Some((rx, journal, client_connected)) = handles else {
        return StatusCode::NOT_FOUND.into_response();
    };

    info!(
        "Viewer [{stream_id}]: client connected from {}",
        crate::client_ip(&headers, peer)
    );
    ws.on_upgrade(move |socket| handle_viewer_ws(socket, rx, journal, client_connected, None))
        .into_response()
}

// ── WS control messages ───────────────────────────────────────────────────────

/// What the Paused arm should do on its periodic tick.
#[derive(Debug, PartialEq)]
struct PausedTick {
    /// Tell the page the ingest client is back.
    announce: bool,
    /// Also resume delivery ourselves.
    resume: bool,
}

/// Decide whether to announce the ingest client's return, and whether to
/// resume delivery.
///
/// These are deliberately separate. `ingest_was_down` records that the
/// stream went away at some point and is NOT cleared when the page sends a
/// control message; `paused_by_disconnect` records that WE forced the pause
/// and IS cleared, because a control message means the user took the wheel.
///
/// Conflating them cost a live viewer an hour: the page repositions itself
/// on `stream_done`, that reposition looked like a user pause, and the
/// viewer then never learned the client had returned — it sat frozen at the
/// moment of the disconnect while still reporting the stream connected.
fn on_paused_tick(
    ingest_was_down: bool,
    connected: bool,
    paused_by_disconnect: bool,
) -> PausedTick {
    let back = ingest_was_down && connected;
    PausedTick {
        announce: back,
        resume: back && paused_by_disconnect,
    }
}

/// Clamp a preload range so the target is never behind the start.
///
/// `seek_index_by_log_ts` returns the journal length when a timestamp is past
/// the end, so an empty or not-yet-written session can legitimately produce
/// `to < from`. CatchUpTo treats `pos >= target_pos` as "done", so the only
/// thing that must not happen is a target that makes it stream backwards.
fn preload_bounds(from_pos: usize, to_pos: usize) -> (usize, usize) {
    (from_pos, to_pos.max(from_pos))
}

/// Resolve a `session` query value against the session list, returning
/// `(start_log_ts, end_log_ts)`. `end` is 0 for the newest session, meaning
/// "unbounded — keep following live".
///
/// `latest` always resolves to the newest session, which is what keeps a
/// long-lived tab from being pinned to a session that has since ended.
fn resolve_session_window(
    param: &str,
    sessions: &[crate::session_index::SessionEntry],
) -> (u64, u64) {
    let num = if param.eq_ignore_ascii_case("latest") {
        match sessions.last() {
            Some(s) => s.num,
            None => return (0, 0),
        }
    } else {
        match param.parse::<u32>() {
            Ok(n) => n,
            Err(_) => return (0, 0),
        }
    };
    let start = sessions
        .iter()
        .find(|s| s.num == num)
        .map(|s| s.start_log_ts)
        .unwrap_or(0);
    let end = sessions
        .iter()
        .find(|s| s.num > num)
        .map(|s| s.start_log_ts)
        .unwrap_or(0);
    (start, end)
}

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
    /// Preload a bounded RANGE of the journal — batches from `from_log_ts` up
    /// to `to_log_ts` — then pause.
    ///
    /// The session-scoped viewer needs exactly one session and discards
    /// everything before it on arrival. Preloading from position 0 made every
    /// page load re-stream the whole journal: by the second day that was
    /// 65,000 batches and 39 MB to render one evening's play, and the page sat
    /// on "Loading data…" long enough to look broken.
    PreloadRange { from_log_ts: u64, to_log_ts: u64 },
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
    /// Periodic liveness beat.
    ///
    /// Without it a viewer socket that dies silently — laptop sleep, a NAT or
    /// proxy reaping an idle connection during a quiet stretch — is
    /// indistinguishable from one where simply nothing is happening. The page
    /// kept showing its last status while nothing updated. This also keeps
    /// intermediaries from reaping the connection in the first place.
    Heartbeat,
    /// Sent when a CatchUpTo sequence finishes and the server transitions to
    /// Paused, signalling that all requested batches have been delivered.
    CatchUpDone,
}

/// Wrap a raw EventBatch JSON string in our WS envelope without re-parsing.
fn wrap_batch(json: &str) -> String {
    format!(r#"{{"t":"batch","batch":{}}}"#, json)
}

/// Escape a value for interpolation inside a single-quoted JS string literal
/// in the viewer template. Prevents breaking out of the string (or the
/// surrounding <script> block) via quotes, backslashes, or `</script>`.
fn js_str_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '\\' => out.push_str("\\\\"),
            '\'' => out.push_str("\\'"),
            '"' => out.push_str("\\\""),
            '<' => out.push_str("\\u003c"),
            '>' => out.push_str("\\u003e"),
            '\n' | '\r' => out.push(' '),
            _ => out.push(c),
        }
    }
    out
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
    mut revoke_rx: Option<tokio::sync::watch::Receiver<()>>,
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
    // True when WE paused this viewer because the ingest client vanished —
    // as opposed to the user pausing on purpose. Only that kind of pause is
    // undone when the client comes back (the self-heal path).
    let mut paused_by_disconnect = false;
    // Whether the ingest client has gone away at any point during this
    // viewer's life. Unlike `paused_by_disconnect` this is NOT cleared when
    // the page sends a control message, because the page sends one as a
    // REACTION to the disconnect (snapSessionToStart repositions itself on
    // stream_done). Clearing on that message left the viewer permanently
    // unable to learn the client had come back: it sat frozen at the moment
    // of the disconnect while still reporting the stream as connected.
    let mut ingest_was_down = false;
    let mut last_beat = std::time::Instant::now();

    loop {
        // Fast non-blocking check: close immediately if public access was revoked.
        if let Some(ref mut rx) = revoke_rx {
            if rx.has_changed().unwrap_or(false) {
                return;
            }
        }

        match mode {
            // ── Catch-up-to: blast stored batches until target, then transition
            ViewerMode::CatchUpTo {
                ref mut pos,
                target_pos,
                then_speed,
            } => {
                if *pos >= target_pos {
                    if then_speed.is_none() {
                        if let Ok(txt) = serde_json::to_string(&ServerMsg::CatchUpDone) {
                            let _ = socket.send(Message::Text(txt.into())).await;
                        }
                    }
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
                let count = CATCHUP_BURST.min(target_pos - *pos);
                let batches = {
                    let j = journal.read().await;
                    j.read_burst(*pos, count)
                };
                let exhausted = batches.len() < count;
                for json in &batches {
                    if socket
                        .send(Message::Text(wrap_batch(json).into()))
                        .await
                        .is_err()
                    {
                        return;
                    }
                    *pos += 1;
                }
                if exhausted {
                    let cur = *pos;
                    if then_speed.is_none() {
                        if let Ok(txt) = serde_json::to_string(&ServerMsg::CatchUpDone) {
                            let _ = socket.send(Message::Text(txt.into())).await;
                        }
                    }
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
                let batches = {
                    let j = journal.read().await;
                    j.read_burst(*pos, CATCHUP_BURST)
                };
                let end_of_journal = batches.len() < CATCHUP_BURST;
                for json in &batches {
                    if socket
                        .send(Message::Text(wrap_batch(json).into()))
                        .await
                        .is_err()
                    {
                        return;
                    }
                    *pos += 1;
                }
                if end_of_journal {
                    // Caught up to the end of stored data.
                    if !client_connected.load(Ordering::Relaxed) {
                        // Stream is a completed recording; no live data will
                        // follow.  Tell the client and pause at the end.
                        paused_by_disconnect = true;
                        ingest_was_down = true;
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
                        if last_beat.elapsed() >= HEARTBEAT_EVERY {
                            last_beat = std::time::Instant::now();
                            if let Ok(txt) = serde_json::to_string(&ServerMsg::Heartbeat) {
                                if socket.send(Message::Text(txt.into())).await.is_err() {
                                    return;
                                }
                            }
                        }
                        if !client_connected.load(Ordering::Relaxed) {
                            let pos = { journal.read().await.len() };
                            paused_by_disconnect = true;
                            ingest_was_down = true;
                        if let Ok(txt) = serde_json::to_string(&ServerMsg::StreamDone) {
                                let _ = socket.send(Message::Text(txt.into())).await;
                            }
                            mode = ViewerMode::Paused { pos };
                        }
                    }
                    _ = async {
                        if let Some(rx) = revoke_rx.as_mut() {
                            rx.changed().await.ok();
                        } else {
                            std::future::pending::<()>().await;
                        }
                    } => { return; }
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
                                _ = async {
                                    if let Some(rx) = revoke_rx.as_mut() {
                                        rx.changed().await.ok();
                                    } else {
                                        std::future::pending::<()>().await;
                                    }
                                } => { return; }
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
                        paused_by_disconnect = true;
                        ingest_was_down = true;
                        if let Ok(txt) = serde_json::to_string(&ServerMsg::StreamDone) {
                            let _ = socket.send(Message::Text(txt.into())).await;
                        }
                        mode = ViewerMode::Paused { pos: *pos };
                    }
                }
            }

            // ── Paused: hold position, wait for Resume / Seek ────────────────────────
            ViewerMode::Paused { pos } => tokio::select! {
                msg = socket.recv() => match msg {
                    Some(Ok(Message::Text(txt))) => {
                        if let Some(new_mode) =
                            handle_client_msg(&txt, &journal, &mut socket, pos).await
                        {
                            // The user took the wheel — whatever pause the
                            // disconnect forced is no longer ours to undo.
                            paused_by_disconnect = false;
                            mode = new_mode;
                        }
                    }
                    None | Some(Ok(Message::Close(_))) | Some(Err(_)) => return,
                    _ => {}
                },
                // Self-heal: this pause was forced by the ingest client
                // vanishing — when it comes back, catch the viewer up on
                // whatever it missed and return to live, announcing it so
                // the page can un-end itself.
                _ = sleep(Duration::from_secs(2)) => {
                    if last_beat.elapsed() >= HEARTBEAT_EVERY {
                        last_beat = std::time::Instant::now();
                        if let Ok(txt) = serde_json::to_string(&ServerMsg::Heartbeat) {
                            if socket.send(Message::Text(txt.into())).await.is_err() {
                                return;
                            }
                        }
                    }
                    let act = on_paused_tick(
                        ingest_was_down,
                        client_connected.load(Ordering::Relaxed),
                        paused_by_disconnect,
                    );
                    if act.announce {
                        ingest_was_down = false;
                        if let Ok(txt) = serde_json::to_string(&ServerMsg::NowLive) {
                            let _ = socket.send(Message::Text(txt.into())).await;
                        }
                    }
                    if act.resume {
                        paused_by_disconnect = false;
                        mode = ViewerMode::CatchUp { pos };
                    }
                },
                _ = async {
                    if let Some(rx) = revoke_rx.as_mut() {
                        rx.changed().await.ok();
                    } else {
                        std::future::pending::<()>().await;
                    }
                } => { return; }
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
        ClientMsg::PreloadRange {
            from_log_ts,
            to_log_ts,
        } => {
            let (pos, target_pos, first_ts, last_ts) = {
                let j = journal.read().await;
                (
                    j.seek_index_by_log_ts(from_log_ts),
                    j.seek_index_by_log_ts(to_log_ts),
                    j.first_ts(),
                    j.last_ts(),
                )
            };
            let (pos, target_pos) = preload_bounds(pos, target_pos);
            let ack = ServerMsg::SeekOk { first_ts, last_ts };
            if let Ok(txt) = serde_json::to_string(&ack) {
                let _ = socket.send(Message::Text(txt.into())).await;
            }
            Some(ViewerMode::CatchUpTo {
                pos,
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

// ── Public player routes (/player/{server}/{name}) ────────────────────────────

#[derive(Deserialize)]
pub struct PlayerQuery {
    /// Session number, or the literal `latest` — see `ViewQuery::session`.
    pub session: Option<String>,
    /// Explicit time-slice scoping (log-time epoch seconds); overrides `session`.
    pub from_ts: Option<u64>,
    pub to_ts: Option<u64>,
}

/// `GET /player/:game/:server/:name` — serves the viewer page for a public stream.
/// Shows the full viewer whether the player is live or has past recorded sessions.
pub async fn player_page_handler(
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
    Path((game, server, name)): Path<(String, String, String)>,
    Query(params): Query<PlayerQuery>,
    State(state): State<ServerState>,
) -> Response {
    let entry_data = {
        let reg = state.registry.read().await;
        reg.find_id_by_player(&game, &server, &name).and_then(|id| {
            reg.get(&id).and_then(|e| {
                if !e.public_stream {
                    return None;
                }
                Some((
                    e.stream_id.clone(),
                    e.player_name.clone(),
                    Arc::clone(&e.session_index),
                ))
            })
        })
    };

    let response = match entry_data {
        None => StatusCode::NOT_FOUND.into_response(),
        Some((stream_id, player_name, session_index)) => {
            let (session_log_ts, session_end_log_ts): (u64, u64) = if params.from_ts.is_some() {
                (params.from_ts.unwrap_or(0), params.to_ts.unwrap_or(0))
            } else if let Some(param) = params.session.as_deref() {
                let si = session_index.read().await;
                resolve_session_window(param, si.list())
            } else {
                (0, 0)
            };
            let ws_path = format!("/player/{game}/{server}/{name}/ws");
            let sessions_path = format!("/player/{game}/{server}/{name}/sessions");
            // Path segments and player_name are client-supplied: HTML-escape
            // the badge, JS-escape the string literals (stored XSS guard —
            // stream creation may be unauthenticated).
            let html = include_str!("../../../static/stream.html")
                .replace("__STREAM_ID__", &js_str_escape(&stream_id))
                .replace("__VIEW_TOKEN__", "")
                .replace("__PLAYER_NAME__", &crate::admin::html_escape(&player_name))
                .replace("__WS_PATH__", &js_str_escape(&ws_path))
                .replace("__SESSIONS_PATH__", &js_str_escape(&sessions_path))
                .replace("__SESSION_LOG_TS__", &session_log_ts.to_string())
                .replace("__SESSION_END_LOG_TS__", &session_end_log_ts.to_string());
            Html(html).into_response()
        }
    };
    info!(
        "Player page [{game}/{server}/{name}] from {} ({})",
        crate::client_ip(&headers, peer),
        response.status()
    );
    response
}

/// `GET /player/:game/:server/:name/ws` — viewer WebSocket for a public stream.
/// Works for both live streams and completed recordings.
pub async fn player_ws_handler(
    Path((game, server, name)): Path<(String, String, String)>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
    ws: WebSocketUpgrade,
    State(state): State<ServerState>,
) -> Response {
    let handles = {
        let reg = state.registry.read().await;
        reg.find_id_by_player(&game, &server, &name).and_then(|id| {
            reg.get(&id).and_then(|e| {
                if !e.public_stream {
                    return None;
                }
                Some((
                    e.stream_id.clone(),
                    e.broadcast_tx.subscribe(),
                    Arc::clone(&e.journal),
                    Arc::clone(&e.client_connected),
                    e.public_revoke_tx.subscribe(),
                ))
            })
        })
    };

    let (stream_id, rx, journal, client_connected, revoke_rx) = match handles {
        None => return StatusCode::NOT_FOUND.into_response(),
        Some(h) => h,
    };

    info!(
        "Player viewer [{stream_id}] ({game}/{server}/{name}): client connected from {}",
        crate::client_ip(&headers, peer)
    );
    ws.on_upgrade(move |socket| {
        handle_viewer_ws(socket, rx, journal, client_connected, Some(revoke_rx))
    })
    .into_response()
}

/// `GET /player/:game/:server/:name/sessions` — public session list for a player's stream.
pub async fn player_sessions_handler(
    Path((game, server, name)): Path<(String, String, String)>,
    State(state): State<ServerState>,
) -> Response {
    let session_index = {
        let reg = state.registry.read().await;
        reg.find_id_by_player(&game, &server, &name).and_then(|id| {
            reg.get(&id).and_then(|e| {
                if !e.public_stream {
                    None
                } else {
                    Some(Arc::clone(&e.session_index))
                }
            })
        })
    };
    match session_index {
        None => StatusCode::NOT_FOUND.into_response(),
        Some(si_arc) => {
            let si = si_arc.read().await;
            let sessions: Vec<serde_json::Value> = si
                .list()
                .iter()
                .map(|s| {
                    serde_json::json!({
                        "num": s.num,
                        "start_log_ts": s.start_log_ts,
                        "start_wall_ts": s.start_wall_ts,
                        "label": s.label,
                    })
                })
                .collect();
            Json(sessions).into_response()
        }
    }
}

// ── Session list ──────────────────────────────────────────────────────────────

/// `GET /stream/:id/sessions?vtok=<view_token>` — returns the list of play sessions
/// archived within this stream's journal.
pub async fn stream_sessions_handler(
    Path(stream_id): Path<String>,
    Query(params): Query<ViewQuery>,
    State(state): State<ServerState>,
) -> Response {
    if !is_valid_view_token(&stream_id, &params.vtok, &state).await {
        return StatusCode::UNAUTHORIZED.into_response();
    }
    let reg = state.registry.read().await;
    match reg.get(&stream_id) {
        None => StatusCode::NOT_FOUND.into_response(),
        Some(entry) => {
            let si = entry.session_index.read().await;
            let sessions: Vec<serde_json::Value> = si
                .list()
                .iter()
                .map(|s| {
                    serde_json::json!({
                        "num": s.num,
                        "start_log_ts": s.start_log_ts,
                        "start_wall_ts": s.start_wall_ts,
                        "label": s.label,
                    })
                })
                .collect();
            Json(sessions).into_response()
        }
    }
}

// ─────────────────────────────────────────────────────────────────────────────

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

#[cfg(test)]
mod tests {
    use super::*;

    /// The page repositions ITSELF when the stream ends (snapSessionToStart
    /// sends seek_paused_log on stream_done). That clears
    /// `paused_by_disconnect`, and for an hour it also silently disabled the
    /// return announcement — the viewer never learned the client was back.
    /// The announcement must survive a control message.
    fn sessions() -> Vec<crate::session_index::SessionEntry> {
        [(10u32, 1785609829u64), (11, 1785636419), (12, 1785666788)]
            .into_iter()
            .map(|(num, start_log_ts)| crate::session_index::SessionEntry {
                num,
                start_batch_id: num as i64,
                start_log_ts,
                start_wall_ts: start_log_ts,
                label: format!("session {num}"),
            })
            .collect()
    }

    /// `latest` must re-resolve every load. This is the whole point: a tab
    /// left open across a new session used to stay pinned to the old one.
    #[test]
    fn latest_resolves_to_the_newest_session_and_stays_unbounded() {
        let (start, end) = resolve_session_window("latest", &sessions());
        assert_eq!(start, 1785666788, "newest session start");
        assert_eq!(end, 0, "0 = unbounded, keep following live");
    }

    /// The bug as it was reported: a page pinned to #10 gets an END bound at
    /// the start of #11, so every batch after Aug 1 22:06 is discarded and
    /// the viewer looks frozen no matter how often it is refreshed.
    #[test]
    fn a_numbered_session_is_bounded_by_the_next_one() {
        let (start, end) = resolve_session_window("10", &sessions());
        assert_eq!(start, 1785609829);
        assert_eq!(end, 1785636419, "bounded by session 11's start");
    }

    /// Junk and unknown numbers must not silently scope the page to nothing.
    #[test]
    fn an_unresolvable_session_scopes_to_the_whole_journal() {
        assert_eq!(resolve_session_window("banana", &sessions()), (0, 0));
        assert_eq!(resolve_session_window("999", &sessions()), (0, 0));
        assert_eq!(resolve_session_window("latest", &[]), (0, 0));
    }

    /// A session preload must START at the session, not at batch 0 — that
    /// was 65,000 batches and 39 MB per page load by day two.
    #[test]
    fn a_preload_starts_where_the_session_starts() {
        assert_eq!(preload_bounds(40_000, 65_859), (40_000, 65_859));
    }

    /// A session with nothing in it yet must not stream backwards.
    #[test]
    fn an_empty_session_preloads_nothing() {
        let (pos, target) = preload_bounds(500, 200);
        assert_eq!((pos, target), (500, 500));
        assert!(pos >= target, "CatchUpTo sees this as immediately done");
    }

    #[test]
    fn a_page_repositioning_itself_still_hears_the_client_return() {
        // user (or the page) sent a control message => paused_by_disconnect
        // cleared, but the stream did go down at some point.
        let t = on_paused_tick(true, true, false);
        assert!(t.announce, "the page must be told the client is back");
        assert!(!t.resume, "but we do not drag a user-paused viewer to live");
    }

    /// The pause we forced ourselves resumes on its own.
    #[test]
    fn a_disconnect_forced_pause_resumes_itself() {
        assert_eq!(
            on_paused_tick(true, true, true),
            PausedTick {
                announce: true,
                resume: true
            }
        );
    }

    /// Nothing to say while the client is still gone, or if it never went.
    #[test]
    fn silence_when_there_is_no_return_to_report() {
        assert_eq!(
            on_paused_tick(true, false, true),
            PausedTick {
                announce: false,
                resume: false
            },
            "still disconnected"
        );
        assert_eq!(
            on_paused_tick(false, true, false),
            PausedTick {
                announce: false,
                resume: false
            },
            "never disconnected — an ordinary user pause stays paused"
        );
    }
}
