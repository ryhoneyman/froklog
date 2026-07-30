use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicI64, Ordering};
use std::sync::Arc;

use tokio::sync::{broadcast, watch, RwLock};

use crate::journal::{Journal, SharedJournal};
use crate::markers::{Markers, SharedMarkers};
use crate::mob_overrides::{MobOverrides, SharedMobOverrides};
use crate::session_index::{SessionIndex, SharedSessionIndex};

// Sized so a viewer parked in replay/pause for a few minutes can still drain
// the live backlog on returning to Live instead of hitting Lagged and
// silently missing batches (~1 batch/sec live).
const BROADCAST_CAPACITY: usize = 256;

/// State for a single player's log stream hosted on the server.
pub struct StreamEntry {
    /// Public identifier used in URLs: `/stream/<stream_id>`.
    pub stream_id: String,
    /// Secret token the Windows client presents to push data (Bearer auth).
    pub stream_token: String,
    /// Secret token viewers must supply in the query string to watch the stream.
    pub view_token: String,
    /// Game identifier, e.g. "eql" for Everquest Legends.
    pub game: String,
    /// EverQuest server name (e.g. "Bristlebane").
    pub server: String,
    /// EverQuest character name reported by the client on stream creation.
    pub player_name: String,
    /// Whether the user opted in to public streaming via the client checkbox.
    pub public_stream: bool,
    /// True when the stream was created by froklog-replay (never goes live).
    pub is_replay: bool,
    /// Fires `()` when `public_stream` flips to false so open public viewer
    /// WebSockets know to close themselves.
    pub public_revoke_tx: watch::Sender<()>,
    /// Persistent on-disk journal (append-only, seekable).
    pub journal: SharedJournal,
    /// Session boundary index — one entry per play session within this journal.
    pub session_index: SharedSessionIndex,
    /// User-defined time markers (raid/group start-end slices).
    pub markers: SharedMarkers,
    /// Owner-curated named/trash NPC overrides for the viewer's ★ grouping.
    pub mob_overrides: SharedMobOverrides,
    /// Fan-out channel: every viewer WebSocket subscribes to this.
    /// Carries raw EventBatch JSON strings (the same content written to disk).
    pub broadcast_tx: broadcast::Sender<Arc<String>>,
    /// True while a Windows client is actively connected on the ingest WS.
    /// Shared atomically so viewer WS handlers can observe disconnection.
    pub client_connected: Arc<AtomicBool>,
    /// Streamer's UTC offset in seconds (local = UTC + offset), reported by
    /// the client's hello message. Used to label sessions with the streamer's
    /// calendar dates. 0 until a client reports it.
    pub utc_offset_secs: Arc<AtomicI64>,
}

impl StreamEntry {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        stream_id: String,
        stream_token: String,
        view_token: String,
        game: String,
        server: String,
        player_name: String,
        public_stream: bool,
        is_replay: bool,
        data_dir: &std::path::Path,
    ) -> std::io::Result<Self> {
        // Open journal first so the retroactive session scan can read its index.
        let journal_inner = Journal::open(data_dir, &stream_id)?;
        let mut si_inner = SessionIndex::open(data_dir, &stream_id)?;
        if si_inner.is_empty() && !journal_inner.is_empty() {
            if let Err(e) = si_inner.retroactive_scan(&journal_inner.index) {
                tracing::warn!("SessionIndex [{stream_id}]: retroactive scan failed: {e}");
            }
        }
        let journal = Arc::new(tokio::sync::RwLock::new(journal_inner));
        let session_index = Arc::new(tokio::sync::RwLock::new(si_inner));
        let markers = Arc::new(Markers::open(data_dir, &stream_id)?);
        let mob_overrides = Arc::new(MobOverrides::open(data_dir, &stream_id)?);

        let (broadcast_tx, _) = broadcast::channel(BROADCAST_CAPACITY);
        let (public_revoke_tx, _) = watch::channel(());
        Ok(Self {
            stream_id,
            stream_token,
            view_token,
            game,
            server,
            player_name,
            public_stream,
            is_replay,
            public_revoke_tx,
            journal,
            session_index,
            markers,
            mob_overrides,
            broadcast_tx,
            client_connected: Arc::new(AtomicBool::new(false)),
            utc_offset_secs: Arc::new(AtomicI64::new(0)),
        })
    }
}

/// Inner registry — held behind an `Arc<RwLock<…>>`.
pub struct StreamRegistry {
    /// Primary index: stream_id → entry.
    by_id: HashMap<String, StreamEntry>,
    /// Reverse index: stream_token → stream_id (for O(1) ingest auth lookup).
    token_to_id: HashMap<String, String>,
    /// Public name index: (game_lower, server_lower, player_lower) → stream_id.
    /// Points to the most recently inserted stream for that identity.
    name_to_id: HashMap<(String, String, String), String>,
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
                entry.game.to_lowercase(),
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
    pub fn find_id_by_player(&self, game: &str, server: &str, name: &str) -> Option<String> {
        let key = (
            game.to_lowercase(),
            server.to_lowercase(),
            name.to_lowercase(),
        );
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
                entry.game.to_lowercase(),
                entry.server.to_lowercase(),
                entry.player_name.to_lowercase(),
            );
            // Only remove from name_to_id if this stream is still the active one.
            if self
                .name_to_id
                .get(&key)
                .map(|id| id == stream_id)
                .unwrap_or(false)
            {
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
                game: e.game.clone(),
                server: e.server.clone(),
                player_name: e.player_name.clone(),
                view_token: e.view_token.clone(),
                connected: e.client_connected.load(Ordering::Relaxed),
                public_stream: e.public_stream,
                is_replay: e.is_replay,
                journal: e.journal.clone(),
                journal_path: self.data_dir.join(&e.stream_id).join("froklog.db"),
                session_index: e.session_index.clone(),
                markers: e.markers.clone(),
            })
            .collect()
    }
}

/// Snapshot of a stream's fields needed by the admin panel.
pub struct AdminStreamInfo {
    pub stream_id: String,
    pub game: String,
    pub server: String,
    pub player_name: String,
    pub view_token: String,
    pub connected: bool,
    pub public_stream: bool,
    pub is_replay: bool,
    /// Handle to the journal so the admin handler can read stats outside the registry lock.
    pub journal: SharedJournal,
    /// Absolute path to the journal file (for file-size stat).
    pub journal_path: PathBuf,
    /// Handle to the session index for per-session breakdown.
    pub session_index: SharedSessionIndex,
    /// Handle to the stream's time markers (retention sweep prunes these too).
    pub markers: SharedMarkers,
}

/// Thread-safe handle to the registry shared across all Axum handlers.
pub type SharedRegistry = Arc<RwLock<StreamRegistry>>;

pub fn new_registry(data_dir: PathBuf) -> SharedRegistry {
    Arc::new(RwLock::new(StreamRegistry::new(data_dir)))
}
