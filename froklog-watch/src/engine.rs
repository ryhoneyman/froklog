//! One character, one pipeline: tail its log, parse it, push the events.
//!
//! This is the dev's own engine — `froklog::tailer` feeds `froklog::parser`
//! feeds `froklog::pusher`. All we add is running several of them at once, one
//! per character, which the single-log client cannot do.

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, RwLock};

use anyhow::{anyhow, Context, Result};
use arc_swap::ArcSwap;
use froklog::state::CombatState;
use froklog::tailer::{TailConfig, TailFrom};
use serde::Deserialize;
use tokio::sync::{broadcast, mpsc};

use crate::registry::{Character, Settings};

#[derive(Debug, Deserialize)]
struct StreamCreated {
    stream_id: String,
    stream_token: String,
    view_token: String,
}

/// Ask froklog for a stream for this character.
///
/// Stream creation is open unless the server sets `stream_password` in its
/// config, in which case it wants that password as a Bearer token. The admin
/// token is for managing existing streams and plays no part here.
pub async fn register(settings: &Settings, ch: &Character) -> Result<(String, String, String)> {
    if settings.server_url.trim().is_empty() {
        return Err(anyhow!("set the froklog server URL first"));
    }
    let url = format!("{}/stream", settings.server_url.trim_end_matches('/'));
    let body = serde_json::json!({
        "game": settings.game,
        "server": ch.server,
        "player": ch.player,
        "public_stream": ch.public,
        "owner_key": settings.owner_key,
        "home_token": settings.home_token,
    });
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(20))
        .build()
        .unwrap_or_default();
    let mut req = client.post(&url).json(&body);
    let pw = settings.stream_password.trim();
    if !pw.is_empty() {
        req = req.bearer_auth(pw);
    }
    let resp = req.send().await.with_context(|| format!("POST {url}"))?;
    if resp.status() == reqwest::StatusCode::UNAUTHORIZED {
        return Err(anyhow!(
            "this server requires a stream password — set one under Server"
        ));
    }
    if !resp.status().is_success() {
        let code = resp.status();
        let text = resp.text().await.unwrap_or_default();
        return Err(anyhow!("froklog said {code}: {}", text.trim()));
    }
    let created: StreamCreated = resp.json().await.context("reading the stream response")?;
    Ok((created.stream_id, created.stream_token, created.view_token))
}

/// Publish (or unpublish) a stream's tokenless /player page.
///
/// This is authenticated with the stream's own token, not the admin token, so
/// whoever owns the character can flip it. Turning it off revokes access
/// immediately — the server drops viewers already on the public page.
pub async fn set_public(settings: &Settings, ch: &Character, public: bool) -> Result<()> {
    let id = ch
        .stream_id
        .as_ref()
        .ok_or_else(|| anyhow!("character is not registered"))?;
    let token = ch
        .stream_token
        .as_ref()
        .ok_or_else(|| anyhow!("character has no stream token"))?;
    let url = format!(
        "{}/stream/{}",
        settings.server_url.trim_end_matches('/'),
        id
    );
    let resp = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(20))
        .build()
        .unwrap_or_default()
        .patch(&url)
        .bearer_auth(token)
        .json(&serde_json::json!({ "public_stream": public }))
        .send()
        .await
        .with_context(|| format!("PATCH {url}"))?;
    if !resp.status().is_success() {
        let code = resp.status();
        let text = resp.text().await.unwrap_or_default();
        return Err(anyhow!("froklog said {code}: {}", text.trim()));
    }
    Ok(())
}

/// Stamp the household link and front-door secret onto an already-registered
/// stream (streams from before they existed). Idempotent; authenticated with
/// the stream's own token.
pub async fn backfill_owner_key(settings: &Settings, ch: &Character) -> Result<()> {
    let (Some(id), Some(token)) = (&ch.stream_id, &ch.stream_token) else {
        return Ok(());
    };
    let url = format!(
        "{}/stream/{}",
        settings.server_url.trim_end_matches('/'),
        id
    );
    reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(20))
        .build()
        .unwrap_or_default()
        .patch(&url)
        .bearer_auth(token)
        .json(&serde_json::json!({
            "owner_key": settings.owner_key,
            "home_token": settings.home_token,
        }))
        .send()
        .await
        .with_context(|| format!("PATCH {url}"))?;
    Ok(())
}

/// Delete the stream from the server INCLUDING its journal database and all
/// history — the retirement path for old characters. Authenticated with the
/// stream's own token. 404 counts as success: the stream is already gone.
pub async fn delete_stream(settings: &Settings, ch: &Character) -> Result<()> {
    let id = ch
        .stream_id
        .as_ref()
        .ok_or_else(|| anyhow!("character is not registered"))?;
    let token = ch
        .stream_token
        .as_ref()
        .ok_or_else(|| anyhow!("character has no stream token"))?;
    let url = format!(
        "{}/stream/{}/purge",
        settings.server_url.trim_end_matches('/'),
        id
    );
    let resp = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(20))
        .build()
        .unwrap_or_default()
        .delete(&url)
        .bearer_auth(token)
        .send()
        .await
        .with_context(|| format!("DELETE {url}"))?;
    if !resp.status().is_success() && resp.status() != reqwest::StatusCode::NOT_FOUND {
        let code = resp.status();
        let text = resp.text().await.unwrap_or_default();
        return Err(anyhow!("froklog said {code}: {}", text.trim()));
    }
    Ok(())
}

/// Live counters for one running character, so the UI can show it working.
pub struct Handle {
    /// No stream: parsing, meter, triggers and fight history only. Healthy
    /// by definition — there is no server to be disconnected from.
    pub local_only: bool,
    pub events_sent: Arc<AtomicU64>,
    pub connected: Arc<AtomicBool>,
    pub last_error: Arc<RwLock<Option<String>>>,
    /// The parser's live combat snapshot — the same state the Windows client
    /// feeds its DPS meter from. The overlay reads it; nothing else writes it.
    pub combat: Arc<ArcSwap<CombatState>>,
    /// Set to ask the parser to clear combat totals (the meter's trash icon).
    pub reset: Arc<AtomicBool>,
    quit: Arc<AtomicBool>,
}

impl Handle {
    pub fn stop(&self) {
        self.quit.store(true, Ordering::Relaxed);
    }
}

impl Drop for Handle {
    fn drop(&mut self) {
        self.stop();
    }
}

/// Push a character's whole existing log, once, and stop at EOF.
///
/// Tailing only ever sees what happens next, so a character played for months
/// before installing this has all of that history sitting in a file and none of
/// it on the server. This is the same thing froklog-replay does in dump mode.
/// It is NOT idempotent — running it twice duplicates the events in the
/// journal — so the caller records that it has been done.
pub fn import_history(
    rt: &tokio::runtime::Handle,
    settings: &Settings,
    ch: &Character,
    done: Arc<AtomicBool>,
    progress: Arc<AtomicU64>,
) -> Result<()> {
    let push_url = ch
        .ingest_url(&settings.server_url)
        .ok_or_else(|| anyhow!("character is not registered"))?;
    let token = ch
        .stream_token
        .clone()
        .ok_or_else(|| anyhow!("character has no stream token"))?;
    let path = ch.log_path.clone();
    let player = ch.player.clone();

    let connected = Arc::new(AtomicBool::new(false));
    let last_error: Arc<RwLock<Option<String>>> = Arc::new(RwLock::new(None));
    let restart = Arc::new(AtomicBool::new(false));
    let quit = Arc::new(AtomicBool::new(false));

    let (line_tx, line_rx) = crossbeam_channel::unbounded::<String>();
    let (event_tx, event_rx) = mpsc::unbounded_channel();

    rt.spawn(async move {
        let cfg = TailConfig {
            from: TailFrom::Start,
            to: None,
            speed: None,
            dump: true, // as fast as the socket allows, then stop
        };
        froklog::tailer::tail(path, cfg, line_tx).await;
    });

    {
        let shared = Arc::new(ArcSwap::from_pointee(CombatState::default()));
        let reset = Arc::new(AtomicBool::new(false));
        let (btx, _brx) = broadcast::channel(16);
        std::thread::Builder::new()
            .name(format!("import-parse-{player}"))
            .spawn(move || {
                froklog::parser::run(line_rx, shared, reset, btx, event_tx, player);
            })
            .context("spawning import parser")?;
    }

    {
        let quit_for_push = Arc::clone(&quit);
        rt.spawn(async move {
            froklog::pusher::push_to_server(
                push_url,
                token,
                event_rx,
                progress,
                connected,
                last_error,
                restart,
                quit_for_push,
            )
            .await;
            // the pusher returns when the event channel closes, i.e. EOF
            done.store(true, Ordering::Relaxed);
        });
    }
    Ok(())
}

/// The log's most recent `/who` lines, one per player (last sighting wins),
/// in original order. Tailing starts at the log's end, so without this every
/// client restart forgets who is what class until the next in-game /who —
/// the meter's class-gradient bars all fall back to grey. Who lines are pure
/// metadata (classes/levels, no combat), so re-parsing them is safe; only
/// the tail of the file is scanned.
fn who_seed(log_path: &str) -> Vec<String> {
    use std::io::{Read, Seek, SeekFrom};
    const SCAN_BYTES: u64 = 4 * 1024 * 1024;
    let Ok(mut f) = std::fs::File::open(log_path) else {
        return Vec::new();
    };
    let len = f.metadata().map(|m| m.len()).unwrap_or(0);
    let start = len.saturating_sub(SCAN_BYTES);
    if f.seek(SeekFrom::Start(start)).is_err() {
        return Vec::new();
    }
    let mut buf = Vec::with_capacity((len - start) as usize);
    if f.read_to_end(&mut buf).is_err() {
        return Vec::new();
    }
    let text = String::from_utf8_lossy(&buf);
    let mut latest: std::collections::HashMap<String, (usize, String)> =
        std::collections::HashMap::new();
    for (i, line) in text.lines().enumerate() {
        if line.len() <= froklog::patterns::TS_LEN {
            continue;
        }
        let body = &line[froklog::patterns::TS_LEN..];
        if let Some(caps) = froklog::patterns::RE_WHO.captures(body) {
            latest.insert(caps["name"].to_string(), (i, line.to_string()));
        }
    }
    let mut rows: Vec<(usize, String)> = latest.into_values().collect();
    rows.sort_unstable_by_key(|(i, _)| *i);
    rows.into_iter().map(|(_, l)| l).collect()
}

/// Start tailing one character's log and pushing its events.
///
/// Tailing starts at the END of the file: replaying a whole log on every
/// start would re-send events the server already journalled.
pub fn start(
    rt: &tokio::runtime::Handle,
    settings: &Settings,
    ch: &Character,
    lines_out: Option<crossbeam_channel::Sender<String>>,
) -> Result<Handle> {
    // Registration buys the STREAM — the meter, triggers and fight history
    // are all fed by the local parser and owe the server nothing. An
    // unregistered character runs the same pipeline minus the pusher, so
    // the client stays fully usable when the server is down or absent.
    let push = ch
        .ingest_url(&settings.server_url)
        .zip(ch.stream_token.clone());
    let local_only = push.is_none();
    let path = ch.log_path.clone();
    let player = ch.player.clone();

    let events_sent = Arc::new(AtomicU64::new(0));
    let connected = Arc::new(AtomicBool::new(false));
    let last_error: Arc<RwLock<Option<String>>> = Arc::new(RwLock::new(None));
    let restart = Arc::new(AtomicBool::new(false));
    let quit = Arc::new(AtomicBool::new(false));

    let (line_tx, line_rx) = crossbeam_channel::unbounded::<String>();
    let (event_tx, event_rx) = mpsc::unbounded_channel();

    // Class knowledge first, before any live line: the parser reads the
    // channel in order, so seeded /who lines are processed ahead of combat.
    for line in who_seed(&ch.log_path) {
        let _ = line_tx.send(line);
    }

    // tailer: file -> lines
    {
        let quit = Arc::clone(&quit);
        rt.spawn(async move {
            let cfg = TailConfig {
                from: TailFrom::End,
                to: None,
                speed: None,
                dump: false,
            };
            tokio::select! {
                _ = froklog::tailer::tail(path, cfg, line_tx) => {}
                _ = async {
                    while !quit.load(Ordering::Relaxed) {
                        tokio::time::sleep(std::time::Duration::from_millis(250)).await;
                    }
                } => {}
            }
        });
    }

    // Triggers match on raw log text, not on parsed events, so the line stream
    // is forwarded to them as well as to the parser. A slow or absent listener
    // must never stall the pipeline that feeds the server, hence a separate
    // hop and a send that ignores failure.
    let line_rx = match lines_out {
        None => line_rx,
        Some(sink) => {
            let (tee_tx, tee_rx) = crossbeam_channel::unbounded::<String>();
            std::thread::Builder::new()
                .name(format!("tee-{player}"))
                .spawn(move || {
                    for line in line_rx {
                        let _ = sink.try_send(line.clone());
                        if tee_tx.send(line).is_err() {
                            break;
                        }
                    }
                })
                .context("spawning line tee")?;
            tee_rx
        }
    };

    // parser: lines -> events (blocking loop, so it gets its own thread)
    let combat = Arc::new(ArcSwap::from_pointee(CombatState::default()));
    let reset = Arc::new(AtomicBool::new(false));
    {
        let shared = Arc::clone(&combat);
        let reset = Arc::clone(&reset);
        let (btx, _brx) = broadcast::channel(16);
        std::thread::Builder::new()
            .name(format!("parse-{player}"))
            .spawn(move || {
                froklog::parser::run(line_rx, shared, reset, btx, event_tx, player);
            })
            .context("spawning parser thread")?;
    }

    // pusher: events -> froklog. Local-only characters skip it; the event
    // channel must still drain or the parser's sends back up forever.
    match push {
        Some((push_url, token)) => {
            let events_sent = Arc::clone(&events_sent);
            let connected = Arc::clone(&connected);
            let last_error = Arc::clone(&last_error);
            let restart = Arc::clone(&restart);
            let quit = Arc::clone(&quit);
            rt.spawn(async move {
                froklog::pusher::push_to_server(
                    push_url,
                    token,
                    event_rx,
                    events_sent,
                    connected,
                    last_error,
                    restart,
                    quit,
                )
                .await;
            });
        }
        None => {
            // "Connected" would light the tray green over nothing, and false
            // reads as an outage. Local-only is its own healthy state — the
            // Handle carries the flag and the tray/UI treat it as OK.
            connected.store(true, Ordering::Relaxed);
            let mut event_rx = event_rx;
            rt.spawn(async move { while event_rx.recv().await.is_some() {} });
        }
    }

    Ok(Handle {
        local_only,
        events_sent,
        connected,
        last_error,
        combat,
        reset,
        quit,
    })
}
