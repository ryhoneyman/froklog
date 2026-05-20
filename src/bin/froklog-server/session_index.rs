/// Per-stream session index: tracks play-session boundaries within a single journal.
///
/// Layout: `<data_dir>/<stream_id>/sessions.jsonl`
///
/// Each line is a JSON object:
///   `{"num":<u32>,"start_journal_idx":<usize>,"start_log_ts":<u64>,"start_wall_ts":<u64>,"label":<str>}`
///
/// Sessions are cut by two mechanisms (in priority order):
///   1. A `Login` event received in an ingest batch ("Welcome to EverQuest Legends!").
///   2. WS client reconnect after a gap of ≥ RECONNECT_GAP_SECS with no pushes.
///
/// For journals that predate session tracking, `retroactive_scan` detects boundaries
/// from gaps of ≥ GAP_SECS between consecutive combat-event log timestamps.
use std::io::{BufRead, Write};
use std::path::PathBuf;
use std::sync::Arc;

use chrono::{DateTime, Datelike, Utc};
use serde::{Deserialize, Serialize};
use tokio::sync::RwLock;
use tracing::warn;

use crate::journal::IndexEntry;

/// Minimum gap (seconds) between consecutive combat log timestamps that triggers
/// a retroactive session boundary during historical journal scanning.
pub const GAP_SECS: u64 = 1800; // 30 minutes

/// Minimum inactivity gap (seconds) since the last received journal batch before
/// a WS client reconnect is treated as a new session.
pub const RECONNECT_GAP_SECS: u64 = 600; // 10 minutes

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionEntry {
    pub num: u32,
    /// Index into the journal where this session's first batch lives.
    pub start_journal_idx: usize,
    /// EQ log-event timestamp of the first event in this session.
    pub start_log_ts: u64,
    /// Wall-clock unix timestamp when the session-start batch was received.
    pub start_wall_ts: u64,
    /// Human-readable label, e.g. "May 17, 2026".
    pub label: String,
}

pub struct SessionIndex {
    path: PathBuf,
    sessions: Vec<SessionEntry>,
}

impl SessionIndex {
    /// Open (or create) the sessions file and load existing entries.
    pub fn open(data_dir: &std::path::Path, stream_id: &str) -> std::io::Result<Self> {
        let dir = data_dir.join(stream_id);
        std::fs::create_dir_all(&dir)?;
        let path = dir.join("sessions.jsonl");
        let mut sessions = Vec::new();
        if path.exists() {
            let file = std::fs::File::open(&path)?;
            let reader = std::io::BufReader::new(file);
            for line in reader.lines() {
                let line = line?;
                let trimmed = line.trim();
                if trimmed.is_empty() {
                    continue;
                }
                match serde_json::from_str::<SessionEntry>(trimmed) {
                    Ok(s) => sessions.push(s),
                    Err(e) => warn!("SessionIndex [{stream_id}]: skipping malformed line: {e}"),
                }
            }
        }
        Ok(Self { path, sessions })
    }

    /// Append a new session entry to disk and the in-memory list.
    pub fn append(&mut self, entry: SessionEntry) -> std::io::Result<()> {
        let line = serde_json::to_string(&entry)
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
        let mut file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.path)?;
        writeln!(file, "{line}")?;
        self.sessions.push(entry);
        Ok(())
    }

    pub fn list(&self) -> &[SessionEntry] {
        &self.sessions
    }

    pub fn len(&self) -> usize {
        self.sessions.len()
    }

    pub fn is_empty(&self) -> bool {
        self.sessions.is_empty()
    }

    /// Journal index of the most recently recorded session start, if any.
    pub fn last_start_idx(&self) -> Option<usize> {
        self.sessions.last().map(|s| s.start_journal_idx)
    }

    /// Truncate the on-disk sessions file to zero bytes and clear the in-memory list.
    pub fn clear(&mut self) -> std::io::Result<()> {
        if self.path.exists() {
            std::fs::OpenOptions::new()
                .write(true)
                .truncate(true)
                .open(&self.path)?;
        }
        self.sessions.clear();
        Ok(())
    }

    /// Scan an existing journal index for combat-event time gaps and populate
    /// this session index retroactively.  No-op when sessions already exist.
    pub fn retroactive_scan(&mut self, journal_index: &[IndexEntry]) -> std::io::Result<()> {
        if journal_index.is_empty() || !self.sessions.is_empty() {
            return Ok(());
        }

        let first = &journal_index[0];
        let first_log_ts = first.log_ts.unwrap_or(first.wall_ts);
        self.append(SessionEntry {
            num: 1,
            start_journal_idx: 0,
            start_log_ts: first_log_ts,
            start_wall_ts: first.wall_ts,
            label: format_label(first_log_ts),
        })?;

        let mut num = 1u32;
        for i in 1..journal_index.len() {
            let prev = &journal_index[i - 1];
            let curr = &journal_index[i];
            let prev_ts = prev.log_ts.unwrap_or(prev.wall_ts);
            let curr_ts = curr.log_ts.unwrap_or(curr.wall_ts);
            if curr_ts.saturating_sub(prev_ts) > GAP_SECS {
                num += 1;
                self.append(SessionEntry {
                    num,
                    start_journal_idx: i,
                    start_log_ts: curr_ts,
                    start_wall_ts: curr.wall_ts,
                    label: format_label(curr_ts),
                })?;
            }
        }
        Ok(())
    }
}

/// Thread-safe handle used from async handlers.
pub type SharedSessionIndex = Arc<RwLock<SessionIndex>>;

pub fn format_label(ts: u64) -> String {
    let dt = DateTime::<Utc>::from_timestamp(ts as i64, 0).unwrap_or_default();
    let month = match dt.month() {
        1 => "Jan",
        2 => "Feb",
        3 => "Mar",
        4 => "Apr",
        5 => "May",
        6 => "Jun",
        7 => "Jul",
        8 => "Aug",
        9 => "Sep",
        10 => "Oct",
        11 => "Nov",
        _ => "Dec",
    };
    format!("{} {}, {}", month, dt.day(), dt.year())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::journal::IndexEntry;

    fn make_idx(entries: &[(u64, Option<u64>)]) -> Vec<IndexEntry> {
        entries
            .iter()
            .map(|&(wall_ts, log_ts)| IndexEntry {
                wall_ts,
                log_ts,
                byte_offset: 0,
            })
            .collect()
    }

    #[test]
    fn retroactive_scan_empty_journal() {
        let dir = tempfile::tempdir().unwrap();
        let mut si = SessionIndex::open(dir.path(), "s1").unwrap();
        si.retroactive_scan(&[]).unwrap();
        assert!(si.is_empty());
    }

    #[test]
    fn retroactive_scan_no_gap() {
        let dir = tempfile::tempdir().unwrap();
        let mut si = SessionIndex::open(dir.path(), "s2").unwrap();
        let idx = make_idx(&[(1000, Some(100)), (1060, Some(160)), (1120, Some(220))]);
        si.retroactive_scan(&idx).unwrap();
        assert_eq!(si.len(), 1);
        assert_eq!(si.sessions[0].start_journal_idx, 0);
    }

    #[test]
    fn retroactive_scan_single_gap() {
        let dir = tempfile::tempdir().unwrap();
        let mut si = SessionIndex::open(dir.path(), "s3").unwrap();
        let idx = make_idx(&[
            (1000, Some(100)),
            (1060, Some(160)),
            (5000, Some(4000)), // gap > 1800s
            (5060, Some(4060)),
        ]);
        si.retroactive_scan(&idx).unwrap();
        assert_eq!(si.len(), 2);
        assert_eq!(si.sessions[0].start_journal_idx, 0);
        assert_eq!(si.sessions[1].start_journal_idx, 2);
        assert_eq!(si.sessions[1].num, 2);
    }

    #[test]
    fn retroactive_scan_skips_when_sessions_exist() {
        let dir = tempfile::tempdir().unwrap();
        let mut si = SessionIndex::open(dir.path(), "s4").unwrap();
        si.append(SessionEntry {
            num: 1,
            start_journal_idx: 0,
            start_log_ts: 100,
            start_wall_ts: 200,
            label: "existing".into(),
        })
        .unwrap();
        let idx = make_idx(&[(1000, Some(100)), (5000, Some(4000))]);
        si.retroactive_scan(&idx).unwrap();
        // Should still be 1 — didn't add anything
        assert_eq!(si.len(), 1);
    }

    #[test]
    fn append_and_reload() {
        let dir = tempfile::tempdir().unwrap();
        {
            let mut si = SessionIndex::open(dir.path(), "s5").unwrap();
            si.append(SessionEntry {
                num: 1,
                start_journal_idx: 0,
                start_log_ts: 500,
                start_wall_ts: 600,
                label: "May 17, 2026".into(),
            })
            .unwrap();
            si.append(SessionEntry {
                num: 2,
                start_journal_idx: 42,
                start_log_ts: 9000,
                start_wall_ts: 9100,
                label: "May 18, 2026".into(),
            })
            .unwrap();
        }
        let si2 = SessionIndex::open(dir.path(), "s5").unwrap();
        assert_eq!(si2.len(), 2);
        assert_eq!(si2.sessions[1].start_journal_idx, 42);
        assert_eq!(si2.sessions[1].label, "May 18, 2026");
    }
}
