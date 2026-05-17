use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use futures_util::SinkExt;
use tokio::sync::mpsc::UnboundedReceiver;
use tokio_tungstenite::connect_async;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::Message;
use tracing::{error, info, warn};

use crate::event::{CombatEvent, EventBatch};

/// Connects to a remote froklog-server ingest WebSocket and forwards batched
/// `CombatEvent`s accumulated over 1-second windows.
/// Automatically reconnects on disconnect.
///
/// `push_url`     — WebSocket URL, e.g. `ws://server:8766/ingest/<stream_id>`
/// `stream_token` — Bearer token issued by `POST /stream` on the server.
/// `events_sent`  — Incremented by the number of events in each batch sent.
/// `connected`    — Set true when WS is up, false while disconnected/retrying.
pub async fn push_to_server(
    push_url: String,
    stream_token: String,
    mut event_rx: UnboundedReceiver<CombatEvent>,
    events_sent: Arc<AtomicU64>,
    connected: Arc<AtomicBool>,
) {
    let mut seq: u32 = 0;

    loop {
        let req = match build_request(&push_url, &stream_token) {
            Ok(r) => r,
            Err(e) => {
                error!("Pusher: cannot build request — {e}");
                return; // misconfiguration; don't retry
            }
        };

        info!("Pusher: connecting to {push_url}");
        let ws = match connect_async(req).await {
            Ok((ws, _)) => {
                info!("Pusher: connected");
                connected.store(true, Ordering::Relaxed);
                ws
            }
            Err(e) => {
                error!("Pusher: connect failed: {e}");
                connected.store(false, Ordering::Relaxed);
                // Drain accumulated events to avoid unbounded memory growth during
                // prolonged reconnect loops.
                while event_rx.try_recv().is_ok() {}
                tokio::time::sleep(Duration::from_secs(5)).await;
                continue;
            }
        };

        let (mut sink, _stream) = futures_util::StreamExt::split(ws);
        let mut tick = tokio::time::interval(Duration::from_secs(1));
        let mut pending: Vec<CombatEvent> = Vec::new();

        let disconnected = 'connected: loop {
            tokio::select! {
                _ = tick.tick() => {
                    if pending.is_empty() { continue; }
                    let mut failed = false;
                    for batch_events in split_by_game_second(std::mem::take(&mut pending)) {
                        let n = batch_events.len() as u64;
                        let batch = EventBatch { seq, events: batch_events };
                        seq = seq.wrapping_add(1);
                        match serde_json::to_string(&batch) {
                            Ok(json) => {
                                if sink.send(Message::Text(json)).await.is_err() {
                                    failed = true;
                                    break;
                                }
                                events_sent.fetch_add(n, Ordering::Relaxed);
                            }
                            Err(e) => warn!("Pusher: serialise error: {e}"),
                        }
                    }
                    if failed { break 'connected true; }
                }
                ev = event_rx.recv() => {
                    match ev {
                        Some(event) => pending.push(event),
                        None => {
                            // Event channel closed (parser thread done).
                            // Flush any pending events before exiting.
                            for batch_events in split_by_game_second(std::mem::take(&mut pending)) {
                                let n = batch_events.len() as u64;
                                let batch = EventBatch { seq, events: batch_events };
                                seq = seq.wrapping_add(1);
                                if let Ok(json) = serde_json::to_string(&batch) {
                                    if sink.send(Message::Text(json)).await.is_ok() {
                                        events_sent.fetch_add(n, Ordering::Relaxed);
                                    }
                                }
                            }
                            info!("Pusher: event channel closed, exiting");
                            connected.store(false, Ordering::Relaxed);
                            return;
                        }
                    }
                }
            }
        };

        connected.store(false, Ordering::Relaxed);
        if disconnected {
            warn!("Pusher: send failed — reconnecting in 5s");
        }
        tokio::time::sleep(Duration::from_secs(5)).await;
    }
}

/// Split a batch of events into 1-second game-time sub-batches.
/// Events are sorted by `ts` and grouped into buckets where all events share
/// the same integer second. This ensures that --dump sessions (which accumulate
/// many seconds of game events per wall-clock tick) produce fine-grained
/// journal entries that replay smoothly at 1-second granularity.
fn split_by_game_second(mut events: Vec<CombatEvent>) -> Vec<Vec<CombatEvent>> {
    if events.is_empty() {
        return Vec::new();
    }
    events.sort_unstable_by_key(|e| e.ts());
    let mut result: Vec<Vec<CombatEvent>> = Vec::new();
    let mut current: Vec<CombatEvent> = Vec::new();
    let mut bucket_ts = events[0].ts();
    for event in events {
        if event.ts() != bucket_ts {
            result.push(std::mem::take(&mut current));
            bucket_ts = event.ts();
        }
        current.push(event);
    }
    if !current.is_empty() {
        result.push(current);
    }
    result
}

fn build_request(
    url: &str,
    token: &str,
) -> Result<tokio_tungstenite::tungstenite::handshake::client::Request, Box<dyn std::error::Error>>
{
    let mut req = url.into_client_request()?;
    req.headers_mut().insert(
        tokio_tungstenite::tungstenite::http::header::AUTHORIZATION,
        tokio_tungstenite::tungstenite::http::HeaderValue::from_str(&format!("Bearer {token}"))?,
    );
    Ok(req)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::event::CombatEvent;

    fn melee(ts: u32) -> CombatEvent {
        CombatEvent::Melee {
            ts,
            mob: 0,
            src: "A".into(),
            tgt: "B".into(),
            dmg: 100,
            typ: "slash".into(),
            tank: false,
            mods: 0,
        }
    }

    #[test]
    fn split_empty() {
        assert!(split_by_game_second(vec![]).is_empty());
    }
    #[test]
    fn split_single_event() {
        let batches = split_by_game_second(vec![melee(1000)]);
        assert_eq!(batches.len(), 1);
        assert_eq!(batches[0].len(), 1);
    }
    #[test]
    fn split_same_second_groups_together() {
        let batches = split_by_game_second(vec![melee(1000), melee(1000), melee(1000)]);
        assert_eq!(batches.len(), 1);
        assert_eq!(batches[0].len(), 3);
    }
    #[test]
    fn split_three_seconds_three_batches() {
        let batches = split_by_game_second(vec![melee(1000), melee(1001), melee(1002)]);
        assert_eq!(batches.len(), 3);
        assert_eq!(batches[0].len(), 1);
    }
    #[test]
    fn split_out_of_order_sorted() {
        let batches = split_by_game_second(vec![melee(1002), melee(1000), melee(1001)]);
        assert_eq!(batches.len(), 3);
        assert_eq!(batches[0][0].ts(), 1000);
        assert_eq!(batches[1][0].ts(), 1001);
        assert_eq!(batches[2][0].ts(), 1002);
    }
    #[test]
    fn split_mixed_counts_per_second() {
        // 3 events at t=5, 1 event at t=6
        let batches = split_by_game_second(vec![melee(5), melee(5), melee(5), melee(6)]);
        assert_eq!(batches.len(), 2);
        assert_eq!(batches[0].len(), 3);
        assert_eq!(batches[1].len(), 1);
    }
}
