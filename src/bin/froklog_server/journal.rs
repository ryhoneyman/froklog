/// Persistent on-disk journal for a single stream.
///
/// Layout:  `<data_dir>/<stream_id>/journal.jsonl`
///
/// Each line is a JSON object:
///   `{"wall_ts":<unix_secs_u64>,"seq":<u32>,"batch":<EventBatch_json>}`
///
/// An in-memory index maps wall_ts → byte offset so seek is O(log n).
use std::io::{BufRead, Seek, SeekFrom, Write};
use std::path::PathBuf;
use std::sync::Arc;

use serde::{Deserialize, Serialize};
use tokio::sync::RwLock;
use tracing::info;

#[derive(Serialize, Deserialize)]
struct JournalLine {
    /// Wall-clock unix seconds when this batch was received by the server.
    wall_ts: u64,
    /// Max EQ log-event unix timestamp from the batch (for replay pacing).
    #[serde(default)]
    log_ts: Option<u64>,
    seq: u32,
    batch: serde_json::Value,
}

/// One entry in the seek index.
#[derive(Clone, Copy)]
pub struct IndexEntry {
    pub wall_ts: u64,
    /// Max EQ log-event unix timestamp in this batch, if recorded.
    pub log_ts: Option<u64>,
    pub byte_offset: u64,
}

/// The append-only, seekable disk journal for one stream.
pub struct Journal {
    path: PathBuf,
    /// Seek index: sorted ascending by wall_ts. Protected by the same outer
    /// RwLock as the rest of the StreamEntry so no extra lock is needed.
    pub index: Vec<IndexEntry>,
}

impl Journal {
    /// Open (or create) the journal file and build the seek index from existing content.
    pub fn open(data_dir: &std::path::Path, stream_id: &str) -> std::io::Result<Self> {
        let dir = data_dir.join(stream_id);
        std::fs::create_dir_all(&dir)?;
        let path = dir.join("journal.jsonl");

        let mut index = Vec::new();

        if path.exists() {
            let file = std::fs::File::open(&path)?;
            let mut reader = std::io::BufReader::new(file);
            let mut offset: u64 = 0;
            let mut line = String::new();
            loop {
                line.clear();
                let n = reader.read_line(&mut line)?;
                if n == 0 {
                    break;
                }
                let trimmed = line.trim_end();
                if !trimmed.is_empty() {
                    if let Ok(jl) = serde_json::from_str::<JournalLine>(trimmed) {
                        index.push(IndexEntry { wall_ts: jl.wall_ts, log_ts: jl.log_ts, byte_offset: offset });
                    }
                }
                offset += n as u64;
            }
            info!("Journal [{stream_id}]: loaded {} batches from disk", index.len());
        }

        Ok(Self { path, index })
    }

    /// Append a raw EventBatch JSON string received at `wall_ts` to disk and update the index.
    /// `log_ts` is the max EQ log-event unix timestamp from the batch (used for replay pacing).
    pub fn append(&mut self, wall_ts: u64, log_ts: Option<u64>, seq: u32, batch_json: &str) -> std::io::Result<()> {
        // Parse the batch value from the already-validated JSON string.
        let batch: serde_json::Value = serde_json::from_str(batch_json)
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;

        let jl = JournalLine { wall_ts, log_ts, seq, batch };
        let line = serde_json::to_string(&jl)
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;

        let mut file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.path)?;

        let byte_offset = file.seek(SeekFrom::End(0))?;
        writeln!(file, "{}", line)?;
        file.flush()?;

        self.index.push(IndexEntry { wall_ts, log_ts, byte_offset });
        Ok(())
    }

    /// Return the total number of batches stored on disk.
    pub fn len(&self) -> usize {
        self.index.len()
    }

    pub fn is_empty(&self) -> bool {
        self.index.len() == 0
    }

    /// Wall-clock unix timestamp of the first batch (if any).
    pub fn first_ts(&self) -> Option<u64> {
        self.index.first().map(|e| e.wall_ts)
    }

    /// Wall-clock unix timestamp of the last batch (if any).
    pub fn last_ts(&self) -> Option<u64> {
        self.index.last().map(|e| e.wall_ts)
    }

    /// EQ log-event timestamp of the first batch (falls back to wall_ts).
    pub fn log_first_ts(&self) -> Option<u64> {
        self.index.first().map(|e| e.log_ts.unwrap_or(e.wall_ts))
    }

    /// EQ log-event timestamp of the last batch (falls back to wall_ts).
    pub fn log_last_ts(&self) -> Option<u64> {
        self.index.last().map(|e| e.log_ts.unwrap_or(e.wall_ts))
    }

    /// Find the index position of the first batch with wall_ts >= target_ts.
    /// Returns `self.index.len()` when target_ts is past the end.
    pub fn seek_index(&self, target_ts: u64) -> usize {
        // Binary search for the leftmost entry with wall_ts >= target_ts.
        let mut lo = 0usize;
        let mut hi = self.index.len();
        while lo < hi {
            let mid = lo + (hi - lo) / 2;
            if self.index[mid].wall_ts < target_ts {
                lo = mid + 1;
            } else {
                hi = mid;
            }
        }
        lo
    }

    /// Find the index position of the first batch with log_ts >= target_ts.
    /// Entries that lack a log_ts fall back to their wall_ts for comparison.
    /// Returns `self.index.len()` when target_ts is past the end.
    pub fn seek_index_by_log_ts(&self, target_ts: u64) -> usize {
        let mut lo = 0usize;
        let mut hi = self.index.len();
        while lo < hi {
            let mid = lo + (hi - lo) / 2;
            let ts = self.index[mid].log_ts.unwrap_or(self.index[mid].wall_ts);
            if ts < target_ts {
                lo = mid + 1;
            } else {
                hi = mid;
            }
        }
        lo
    }

    /// Read a single line (EventBatch JSON) from disk by its index position.
    /// Returns `None` if the position is out of range or the file cannot be read.
    pub fn read_at(&self, pos: usize) -> Option<Arc<String>> {
        let entry = self.index.get(pos)?;
        let mut file = std::fs::File::open(&self.path).ok()?;
        file.seek(SeekFrom::Start(entry.byte_offset)).ok()?;
        let mut reader = std::io::BufReader::new(file);
        let mut line = String::new();
        reader.read_line(&mut line).ok()?;
        let trimmed = line.trim_end();
        if trimmed.is_empty() {
            return None;
        }
        // Parse and re-emit just the batch field so the client receives the
        // same EventBatch JSON it would get from the live broadcast.
        let jl: JournalLine = serde_json::from_str(trimmed).ok()?;
        Some(Arc::new(serde_json::to_string(&jl.batch).ok()?))
    }

    /// Read the wall_ts for a given index position without deserialising the full line.
    pub fn ts_at(&self, pos: usize) -> Option<u64> {
        self.index.get(pos).map(|e| e.wall_ts)
    }

    /// Read the EQ log-event timestamp for a given index position.
    /// Falls back to `wall_ts` for old journal entries that predate this field.
    pub fn log_ts_at(&self, pos: usize) -> Option<u64> {
        self.index.get(pos).map(|e| e.log_ts.unwrap_or(e.wall_ts))
    }
}

/// Thread-safe wrapper used from async code.
pub type SharedJournal = Arc<RwLock<Journal>>;

pub fn open_shared(data_dir: &std::path::Path, stream_id: &str) -> std::io::Result<SharedJournal> {
    Ok(Arc::new(RwLock::new(Journal::open(data_dir, stream_id)?)))
}
