/// Per-stream session index: tracks play-session boundaries within a stream.
///
/// Stored in the stream's SQLite database (`froklog.db`, `sessions` table,
/// schema created in `journal.rs::open_db`).
///
/// Sessions are cut by two mechanisms (in priority order):
///   1. A `Login` event received in an ingest batch ("Welcome to EverQuest Legends!").
///   2. WS client reconnect after a gap of ≥ RECONNECT_GAP_SECS with no pushes.
///
/// A session anchors to the permanent row id of its first batch
/// (`start_batch_id`), NOT a positional index: pruning old batches never shifts
/// a session boundary. Positions are derived on demand via
/// `Journal::pos_of_id`.
///
/// For journals that predate session tracking, `retroactive_scan` detects
/// boundaries from gaps of ≥ GAP_SECS between consecutive batch log timestamps.
use std::sync::Arc;

use chrono::{DateTime, Datelike, Utc};
use rusqlite::Connection;
use serde::{Deserialize, Serialize};
use tokio::sync::RwLock;

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
    /// Permanent row id of this session's first batch in the `batches` table.
    pub start_batch_id: i64,
    /// EQ log-event timestamp of the first event in this session.
    pub start_log_ts: u64,
    /// Wall-clock unix timestamp when the session-start batch was received.
    pub start_wall_ts: u64,
    /// Human-readable label, e.g. "May 17, 2026".
    pub label: String,
}

pub struct SessionIndex {
    conn: std::sync::Mutex<Connection>,
    sessions: Vec<SessionEntry>,
}

fn sql_err(e: rusqlite::Error) -> std::io::Error {
    std::io::Error::other(e)
}

impl SessionIndex {
    /// Open the stream database and load existing session entries.
    pub fn open(data_dir: &std::path::Path, stream_id: &str) -> std::io::Result<Self> {
        let conn = crate::journal::open_db(data_dir, stream_id)?;
        let mut sessions = Vec::new();
        {
            let mut stmt = conn
                .prepare(
                    "SELECT num, start_batch_id, start_log_ts, start_wall_ts, label
                     FROM sessions ORDER BY num",
                )
                .map_err(sql_err)?;
            let rows = stmt
                .query_map([], |row| {
                    Ok(SessionEntry {
                        num: row.get::<_, i64>(0)? as u32,
                        start_batch_id: row.get(1)?,
                        start_log_ts: row.get::<_, i64>(2)? as u64,
                        start_wall_ts: row.get::<_, i64>(3)? as u64,
                        label: row.get(4)?,
                    })
                })
                .map_err(sql_err)?;
            for row in rows {
                sessions.push(row.map_err(sql_err)?);
            }
        }
        Ok(Self {
            conn: std::sync::Mutex::new(conn),
            sessions,
        })
    }

    /// Append a new session entry to the database and the in-memory list.
    pub fn append(&mut self, entry: SessionEntry) -> std::io::Result<()> {
        let conn = self.conn.lock().expect("session db mutex");
        conn.prepare_cached(
            "INSERT INTO sessions (num, start_batch_id, start_log_ts, start_wall_ts, label)
             VALUES (?1, ?2, ?3, ?4, ?5)",
        )
        .map_err(sql_err)?
        .execute(rusqlite::params![
            entry.num as i64,
            entry.start_batch_id,
            entry.start_log_ts as i64,
            entry.start_wall_ts as i64,
            entry.label,
        ])
        .map_err(sql_err)?;
        drop(conn);
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

    /// Batch id of the most recently recorded session start, if any.
    pub fn last_start_id(&self) -> Option<i64> {
        self.sessions.last().map(|s| s.start_batch_id)
    }

    /// Delete all sessions from the database and the in-memory list.
    pub fn clear(&mut self) -> std::io::Result<()> {
        let conn = self.conn.lock().expect("session db mutex");
        conn.execute("DELETE FROM sessions", []).map_err(sql_err)?;
        drop(conn);
        self.sessions.clear();
        Ok(())
    }

    /// After batches older than a cutoff were pruned, drop sessions that no
    /// longer own any batch. `min_remaining_id` is the smallest surviving batch
    /// row id (`None` = journal now empty → drop all sessions).
    ///
    /// The newest session that started before the surviving range is kept and
    /// re-anchored to the first surviving batch, so every remaining batch
    /// always belongs to a session.
    pub fn prune(&mut self, min_remaining_id: Option<i64>) -> std::io::Result<usize> {
        let Some(min_id) = min_remaining_id else {
            let n = self.sessions.len();
            self.clear()?;
            return Ok(n);
        };

        // Newest session at-or-before the surviving range keeps ownership of it.
        let keep_from = self
            .sessions
            .iter()
            .rposition(|s| s.start_batch_id <= min_id)
            .unwrap_or(0);

        let removed = keep_from;
        if removed > 0 {
            let first_kept_num = self.sessions[keep_from].num as i64;
            let conn = self.conn.lock().expect("session db mutex");
            conn.execute("DELETE FROM sessions WHERE num < ?1", [first_kept_num])
                .map_err(sql_err)?;
            drop(conn);
            self.sessions.drain(..keep_from);
        }

        // Re-anchor the (now) first session to the first surviving batch.
        if let Some(first) = self.sessions.first_mut() {
            if first.start_batch_id < min_id {
                first.start_batch_id = min_id;
                let conn = self.conn.lock().expect("session db mutex");
                conn.execute(
                    "UPDATE sessions SET start_batch_id = ?1 WHERE num = ?2",
                    rusqlite::params![min_id, first.num as i64],
                )
                .map_err(sql_err)?;
            }
        }
        Ok(removed)
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
            start_batch_id: first.rowid,
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
                    start_batch_id: curr.rowid,
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
            .enumerate()
            .map(|(i, &(wall_ts, log_ts))| IndexEntry {
                wall_ts,
                log_ts,
                rowid: (i + 1) as i64,
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
        assert_eq!(si.sessions[0].start_batch_id, 1);
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
        assert_eq!(si.sessions[0].start_batch_id, 1);
        assert_eq!(si.sessions[1].start_batch_id, 3);
        assert_eq!(si.sessions[1].num, 2);
    }

    #[test]
    fn retroactive_scan_skips_when_sessions_exist() {
        let dir = tempfile::tempdir().unwrap();
        let mut si = SessionIndex::open(dir.path(), "s4").unwrap();
        si.append(SessionEntry {
            num: 1,
            start_batch_id: 1,
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
                start_batch_id: 1,
                start_log_ts: 500,
                start_wall_ts: 600,
                label: "May 17, 2026".into(),
            })
            .unwrap();
            si.append(SessionEntry {
                num: 2,
                start_batch_id: 42,
                start_log_ts: 9000,
                start_wall_ts: 9100,
                label: "May 18, 2026".into(),
            })
            .unwrap();
        }
        let si2 = SessionIndex::open(dir.path(), "s5").unwrap();
        assert_eq!(si2.len(), 2);
        assert_eq!(si2.sessions[1].start_batch_id, 42);
        assert_eq!(si2.sessions[1].label, "May 18, 2026");
    }

    #[test]
    fn prune_drops_dead_sessions_and_reanchors() {
        let dir = tempfile::tempdir().unwrap();
        let mut si = SessionIndex::open(dir.path(), "s6").unwrap();
        for (num, id, ts) in [(1u32, 1i64, 100u64), (2, 10, 5000), (3, 20, 9000)] {
            si.append(SessionEntry {
                num,
                start_batch_id: id,
                start_log_ts: ts,
                start_wall_ts: ts,
                label: format!("s{num}"),
            })
            .unwrap();
        }

        // Batches 1..=14 pruned; oldest survivor is id 15 (mid-session-2).
        let removed = si.prune(Some(15)).unwrap();
        assert_eq!(removed, 1); // session 1 gone
        assert_eq!(si.len(), 2);
        // Session 2 survives, re-anchored to the first surviving batch.
        assert_eq!(si.sessions[0].num, 2);
        assert_eq!(si.sessions[0].start_batch_id, 15);
        assert_eq!(si.sessions[1].num, 3);
        assert_eq!(si.sessions[1].start_batch_id, 20);

        // Persisted: reload sees the same state.
        drop(si);
        let si2 = SessionIndex::open(dir.path(), "s6").unwrap();
        assert_eq!(si2.len(), 2);
        assert_eq!(si2.sessions[0].start_batch_id, 15);
    }

    #[test]
    fn prune_empty_journal_drops_all() {
        let dir = tempfile::tempdir().unwrap();
        let mut si = SessionIndex::open(dir.path(), "s7").unwrap();
        si.append(SessionEntry {
            num: 1,
            start_batch_id: 1,
            start_log_ts: 100,
            start_wall_ts: 100,
            label: "x".into(),
        })
        .unwrap();
        let removed = si.prune(None).unwrap();
        assert_eq!(removed, 1);
        assert!(si.is_empty());
    }
}
