use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use tokio::sync::{broadcast, RwLock};
use tracing::warn;

use crate::journal::{open_shared, SharedJournal};

const BROADCAST_CAPACITY: usize = 64;

/// State for a single player's log stream hosted on the server.
pub struct StreamEntry {
    /// Public identifier used in URLs: `/stream/<stream_id>`.
    pub stream_id: String,
    /// Secret token the Windows client presents to push data (Bearer auth).
    pub stream_token: String,
    /// Secret token viewers must supply in the query string to watch the stream.
    pub view_token: String,
    /// EverQuest server name (e.g. "Bristlebane").
    pub server: String,
    /// EverQuest character name reported by the client on stream creation.
    pub player_name: String,
    /// Persistent on-disk journal (append-only, seekable).
    pub journal: SharedJournal,
    /// Fan-out channel: every viewer WebSocket subscribes to this.
    /// Carries raw EventBatch JSON strings (the same content written to disk).
    pub broadcast_tx: broadcast::Sender<Arc<String>>,
    /// True while a Windows client is actively connected on the ingest WS.
    /// Shared atomically so viewer WS handlers can observe disconnection.
    pub client_connected: Arc<AtomicBool>,
}

impl StreamEntry {
    pub fn new(
        stream_id: String,
        stream_token: String,
        view_token: String,
        server: String,
        player_name: String,
        data_dir: &std::path::Path,
    ) -> std::io::Result<Self> {
        let journal = open_shared(data_dir, &stream_id)?;
        let (broadcast_tx, _) = broadcast::channel(BROADCAST_CAPACITY);
        Ok(Self {
            stream_id,
            stream_token,
            view_token,
            server,
            player_name,
            journal,
            broadcast_tx,
            client_connected: Arc::new(AtomicBool::new(false)),
        })
    }

    /// Append a raw EventBatch JSON string to the on-disk journal.
    /// `wall_ts` is the server-side unix seconds when the batch was received.
    /// `log_ts` is the max EQ log-event unix timestamp from the batch.
    pub async fn append_journal(&self, wall_ts: u64, log_ts: Option<u64>, seq: u32, json: &str) {
        let mut j = self.journal.write().await;
        if let Err(e) = j.append(wall_ts, log_ts, seq, json) {
            warn!("Journal [{}]: write error: {e}", self.stream_id);
        }
    }
}

/// Inner registry — held behind an `Arc<RwLock<…>>`.
pub struct StreamRegistry {
    /// Primary index: stream_id → entry.
    by_id: HashMap<String, StreamEntry>,
    /// Reverse index: stream_token → stream_id (for O(1) ingest auth lookup).
    token_to_id: HashMap<String, String>,
    /// Public name index: (server_lower, player_lower) → stream_id.
    /// Points to the most recently inserted stream for that identity.
    name_to_id: HashMap<(String, String), String>,
    /// Root directory for on-disk journals.
    pub data_dir: PathBuf,
}

impl StreamRegistry {
    pub fn new(data_dir: PathBuf) -> Self {
        Self {
            by_id: HashMap::new(),
            token_to_id: HashMap::new(),
            name_to_id: HashMap::new(),
            data_dir,
        }
    }

    pub fn insert(&mut self, entry: StreamEntry) {
        if !entry.server.is_empty() {
            let key = (
                entry.server.to_lowercase(),
                entry.player_name.to_lowercase(),
            );
            self.name_to_id.insert(key, entry.stream_id.clone());
        }
        self.token_to_id
            .insert(entry.stream_token.clone(), entry.stream_id.clone());
        self.by_id.insert(entry.stream_id.clone(), entry);
    }

    /// Look up the stream_id for a public player identity (case-insensitive).
    pub fn find_id_by_player(&self, server: &str, name: &str) -> Option<String> {
        let key = (server.to_lowercase(), name.to_lowercase());
        self.name_to_id.get(&key).cloned()
    }

    pub fn get(&self, stream_id: &str) -> Option<&StreamEntry> {
        self.by_id.get(stream_id)
    }

    pub fn get_mut(&mut self, stream_id: &str) -> Option<&mut StreamEntry> {
        self.by_id.get_mut(stream_id)
    }

    pub fn find_id_by_token(&self, token: &str) -> Option<String> {
        self.token_to_id.get(token).cloned()
    }

    pub fn remove(&mut self, stream_id: &str) -> Option<StreamEntry> {
        if let Some(entry) = self.by_id.remove(stream_id) {
            self.token_to_id.remove(&entry.stream_token);
            let key = (
                entry.server.to_lowercase(),
                entry.player_name.to_lowercase(),
            );
            // Only remove from name_to_id if this stream is still the active one.
            if self.name_to_id.get(&key).map(|id| id == stream_id).unwrap_or(false) {
                self.name_to_id.remove(&key);
            }
            Some(entry)
        } else {
            None
        }
    }

    pub fn list(&self) -> Vec<(&str, &str, bool)> {
        self.by_id
            .values()
            .map(|e| {
                (
                    e.stream_id.as_str(),
                    e.player_name.as_str(),
                    e.client_connected.load(Ordering::Relaxed),
                )
            })
            .collect()
    }

    pub fn list_admin(&self) -> Vec<AdminStreamInfo> {
        self.by_id
            .values()
            .map(|e| AdminStreamInfo {
                stream_id: e.stream_id.clone(),
                server: e.server.clone(),
                player_name: e.player_name.clone(),
                view_token: e.view_token.clone(),
                connected: e.client_connected.load(Ordering::Relaxed),
                journal: e.journal.clone(),
                journal_path: self.data_dir.join(&e.stream_id).join("journal.jsonl"),
            })
            .collect()
    }
}

/// Snapshot of a stream's fields needed by the admin panel.
pub struct AdminStreamInfo {
    pub stream_id: String,
    pub server: String,
    pub player_name: String,
    pub view_token: String,
    pub connected: bool,
    /// Handle to the journal so the admin handler can read stats outside the registry lock.
    pub journal: SharedJournal,
    /// Absolute path to the journal file (for file-size stat).
    pub journal_path: PathBuf,
}

/// Thread-safe handle to the registry shared across all Axum handlers.
pub type SharedRegistry = Arc<RwLock<StreamRegistry>>;

pub fn new_registry(data_dir: PathBuf) -> SharedRegistry {
    Arc::new(RwLock::new(StreamRegistry::new(data_dir)))
}
