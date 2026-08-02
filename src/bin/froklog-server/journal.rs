/// Persistent per-stream storage backed by embedded SQLite.
///
/// Layout: `<data_dir>/<stream_id>/froklog.db` (WAL mode).
///
/// Tables (created here, shared with `session_index.rs` and markers):
///   batches  (id INTEGER PRIMARY KEY AUTOINCREMENT, wall_ts, log_ts, seq, batch BLOB)
///   sessions (num INTEGER PRIMARY KEY, start_batch_id, start_log_ts, start_wall_ts, label)
///   markers  (id INTEGER PRIMARY KEY AUTOINCREMENT, ts, kind, label)
///   mob_overrides   (name PRIMARY KEY, kind)
///   segment_members (seg_ts, name PRIMARY KEY pair, display) — per-segment
///                   aggregate exclusions, see `segment_roster.rs`
///
/// Batch JSON is zlib-compressed in the BLOB, same compression as the old
/// binary journal format. The in-memory seek index (`Vec<IndexEntry>`, ordered
/// by insertion) is preserved from the old design: viewer positions stay
/// ephemeral per-connection `usize` offsets into this vec, and SQLite row ids
/// are the permanent handles (sessions anchor to ids so pruning old rows never
/// shifts a session boundary).
///
/// The old `journal.jsonl` format is NOT migrated; when one is present next to
/// an empty database a notice is logged and the file is ignored.
use std::io::{Read, Write};
use std::sync::{Arc, Mutex};

use flate2::read::ZlibDecoder;
use flate2::write::ZlibEncoder;
use flate2::Compression;
use rusqlite::Connection;
use tokio::sync::RwLock;
use tracing::{info, warn};

/// One entry in the in-memory seek index, ordered by insertion (= by `rowid`).
#[derive(Clone, Copy)]
pub struct IndexEntry {
    pub wall_ts: u64,
    /// Max EQ log-event unix timestamp in this batch, if recorded.
    pub log_ts: Option<u64>,
    /// Permanent SQLite row id of this batch. Monotonic, never reused.
    pub rowid: i64,
}

/// The durable, seekable batch store for one stream.
pub struct Journal {
    /// SQLite handle. `Connection` is Send but not Sync, so it sits behind a
    /// std Mutex; every lock is held only for one short, non-awaiting call.
    conn: Mutex<Connection>,
    /// Seek index rebuilt from the DB on open and maintained on append/prune.
    pub index: Vec<IndexEntry>,
}

fn compress_batch(json: &str) -> std::io::Result<Vec<u8>> {
    let mut enc = ZlibEncoder::new(Vec::new(), Compression::fast());
    enc.write_all(json.as_bytes())?;
    enc.finish()
}

fn decompress_batch(data: &[u8]) -> Option<String> {
    let mut dec = ZlibDecoder::new(data);
    let mut out = String::new();
    dec.read_to_string(&mut out).ok()?;
    Some(out)
}

fn sql_err(e: rusqlite::Error) -> std::io::Error {
    std::io::Error::other(e)
}

/// Open the per-stream database, applying pragmas and creating the schema.
/// Shared by `Journal` and `SessionIndex` (separate connections, same file).
pub(crate) fn open_db(data_dir: &std::path::Path, stream_id: &str) -> std::io::Result<Connection> {
    let dir = data_dir.join(stream_id);
    std::fs::create_dir_all(&dir)?;
    let conn = Connection::open(dir.join("froklog.db")).map_err(sql_err)?;
    conn.pragma_update(None, "journal_mode", "WAL")
        .map_err(sql_err)?;
    conn.pragma_update(None, "synchronous", "NORMAL")
        .map_err(sql_err)?;
    conn.busy_timeout(std::time::Duration::from_secs(5))
        .map_err(sql_err)?;
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS batches (
            id      INTEGER PRIMARY KEY AUTOINCREMENT,
            wall_ts INTEGER NOT NULL,
            log_ts  INTEGER,
            seq     INTEGER NOT NULL,
            batch   BLOB NOT NULL
        );
        CREATE TABLE IF NOT EXISTS sessions (
            num           INTEGER PRIMARY KEY,
            start_batch_id INTEGER NOT NULL,
            start_log_ts  INTEGER NOT NULL,
            start_wall_ts INTEGER NOT NULL,
            label         TEXT NOT NULL
        );
        CREATE TABLE IF NOT EXISTS markers (
            id    INTEGER PRIMARY KEY AUTOINCREMENT,
            ts    INTEGER NOT NULL,
            kind  TEXT NOT NULL,
            label TEXT NOT NULL DEFAULT ''
        );
        CREATE TABLE IF NOT EXISTS mob_overrides (
            name  TEXT PRIMARY KEY,
            kind  TEXT NOT NULL CHECK (kind IN ('named', 'trash'))
        );
        CREATE TABLE IF NOT EXISTS segment_members (
            seg_ts  INTEGER NOT NULL,
            name    TEXT NOT NULL,
            display TEXT NOT NULL,
            PRIMARY KEY (seg_ts, name)
        );",
    )
    .map_err(sql_err)?;
    Ok(conn)
}

impl Journal {
    /// Open (or create) the stream database and load the seek index.
    pub fn open(data_dir: &std::path::Path, stream_id: &str) -> std::io::Result<Self> {
        let conn = open_db(data_dir, stream_id)?;

        let mut index = Vec::new();
        {
            let mut stmt = conn
                .prepare("SELECT id, wall_ts, log_ts FROM batches ORDER BY id")
                .map_err(sql_err)?;
            let rows = stmt
                .query_map([], |row| {
                    Ok(IndexEntry {
                        rowid: row.get(0)?,
                        wall_ts: row.get::<_, i64>(1)? as u64,
                        log_ts: row.get::<_, Option<i64>>(2)?.map(|v| v as u64),
                    })
                })
                .map_err(sql_err)?;
            for row in rows {
                index.push(row.map_err(sql_err)?);
            }
        }

        // Old-format journal present but not migrated (by design).
        let legacy = data_dir.join(stream_id).join("journal.jsonl");
        if legacy.exists() && index.is_empty() {
            warn!(
                "Journal [{stream_id}]: legacy journal.jsonl found — old data is NOT migrated and will be ignored"
            );
        }

        if !index.is_empty() {
            info!("Journal [{stream_id}]: loaded {} batches", index.len());
        }

        Ok(Self {
            conn: Mutex::new(conn),
            index,
        })
    }

    /// Append a raw EventBatch JSON string received at `wall_ts` and update the index.
    /// `log_ts` is the max EQ log-event unix timestamp from the batch (used for replay pacing).
    pub fn append(
        &mut self,
        wall_ts: u64,
        log_ts: Option<u64>,
        seq: u32,
        batch_json: &str,
    ) -> std::io::Result<()> {
        let compressed = compress_batch(batch_json)?;
        let conn = self.conn.lock().expect("journal db mutex");
        conn.prepare_cached(
            "INSERT INTO batches (wall_ts, log_ts, seq, batch) VALUES (?1, ?2, ?3, ?4)",
        )
        .map_err(sql_err)?
        .execute(rusqlite::params![
            wall_ts as i64,
            log_ts.map(|v| v as i64),
            seq as i64,
            compressed,
        ])
        .map_err(sql_err)?;
        let rowid = conn.last_insert_rowid();
        drop(conn);

        self.index.push(IndexEntry {
            wall_ts,
            log_ts,
            rowid,
        });
        Ok(())
    }

    /// Return the total number of batches stored.
    pub fn len(&self) -> usize {
        self.index.len()
    }

    pub fn is_empty(&self) -> bool {
        self.index.is_empty()
    }

    /// The row id the NEXT appended batch will receive.
    /// Used to anchor a session boundary cut just before its first batch arrives.
    pub fn next_batch_id(&self) -> i64 {
        let conn = self.conn.lock().expect("journal db mutex");
        // AUTOINCREMENT: next id is strictly greater than any id ever used,
        // tracked in sqlite_sequence even after deletes.
        conn.query_row(
            "SELECT COALESCE((SELECT seq FROM sqlite_sequence WHERE name='batches'), 0) + 1",
            [],
            |row| row.get(0),
        )
        .unwrap_or_else(|_| self.index.last().map(|e| e.rowid + 1).unwrap_or(1))
    }

    /// Index position of the first batch with `rowid >= id`.
    /// Row ids are insertion-ordered, so this is a true binary search.
    pub fn pos_of_id(&self, id: i64) -> usize {
        self.index.partition_point(|e| e.rowid < id)
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
    /// wall_ts is server-assigned under the journal write lock, so it is
    /// monotonic and binary search is valid.
    pub fn seek_index(&self, target_ts: u64) -> usize {
        self.index.partition_point(|e| e.wall_ts < target_ts)
    }

    /// Find the index position of the first batch (in journal order) with
    /// log_ts >= target_ts. Entries without a log_ts fall back to wall_ts.
    ///
    /// This is a linear scan on purpose: log_ts is only sorted for a
    /// well-behaved live clock. Historical imports pushed into a stream with
    /// newer data (or a client clock stepping backwards) produce unsorted
    /// sequences, and a binary search over unsorted data lands at an arbitrary
    /// position. The index lives in RAM, so first-match is microseconds.
    pub fn seek_index_by_log_ts(&self, target_ts: u64) -> usize {
        self.index
            .iter()
            .position(|e| e.log_ts.unwrap_or(e.wall_ts) >= target_ts)
            .unwrap_or(self.index.len())
    }

    /// Read a single batch JSON by its index position.
    /// Returns `None` if the position is out of range or the row cannot be read.
    pub fn read_at(&self, pos: usize) -> Option<Arc<String>> {
        let entry = self.index.get(pos)?;
        let conn = self.conn.lock().expect("journal db mutex");
        let blob: Vec<u8> = conn
            .prepare_cached("SELECT batch FROM batches WHERE id = ?1")
            .ok()?
            .query_row([entry.rowid], |row| row.get(0))
            .ok()?;
        drop(conn);
        decompress_batch(&blob).map(Arc::new)
    }

    /// Read up to `count` batches sequentially starting at index position `pos`.
    /// Returns fewer than `count` entries only when the journal is exhausted.
    pub fn read_burst(&self, pos: usize, count: usize) -> Vec<Arc<String>> {
        if pos >= self.index.len() || count == 0 {
            return Vec::new();
        }
        let end = (pos + count).min(self.index.len());
        let first_id = self.index[pos].rowid;
        let last_id = self.index[end - 1].rowid;

        let conn = self.conn.lock().expect("journal db mutex");
        let mut stmt = match conn
            .prepare_cached("SELECT batch FROM batches WHERE id BETWEEN ?1 AND ?2 ORDER BY id")
        {
            Ok(s) => s,
            Err(_) => return Vec::new(),
        };
        let rows = match stmt.query_map([first_id, last_id], |row| row.get::<_, Vec<u8>>(0)) {
            Ok(r) => r,
            Err(_) => return Vec::new(),
        };
        let mut results = Vec::with_capacity(end - pos);
        for blob in rows.flatten() {
            match decompress_batch(&blob) {
                Some(json) => results.push(Arc::new(json)),
                None => break,
            }
        }
        results
    }

    /// Read the wall_ts for a given index position.
    pub fn ts_at(&self, pos: usize) -> Option<u64> {
        self.index.get(pos).map(|e| e.wall_ts)
    }

    /// Read the EQ log-event timestamp for a given index position.
    /// Falls back to `wall_ts` for entries that lack a log_ts.
    pub fn log_ts_at(&self, pos: usize) -> Option<u64> {
        self.index.get(pos).map(|e| e.log_ts.unwrap_or(e.wall_ts))
    }

    /// Delete all stored batches and clear the in-memory index.
    pub fn clear(&mut self) -> std::io::Result<()> {
        let conn = self.conn.lock().expect("journal db mutex");
        conn.execute("DELETE FROM batches", []).map_err(sql_err)?;
        drop(conn);
        self.index.clear();
        Ok(())
    }

    /// Delete every batch whose log timestamp (fallback: wall timestamp) is
    /// older than `cutoff_ts`, reclaim the space, and rebuild the index.
    /// Returns (batches_deleted, smallest_remaining_rowid).
    pub fn prune(&mut self, cutoff_ts: u64) -> std::io::Result<(usize, Option<i64>)> {
        let deleted = {
            let conn = self.conn.lock().expect("journal db mutex");
            let n = conn
                .execute(
                    "DELETE FROM batches WHERE COALESCE(log_ts, wall_ts) < ?1",
                    [cutoff_ts as i64],
                )
                .map_err(sql_err)?;
            if n > 0 {
                // Reclaim freed pages; per-stream DBs are small so this is quick.
                conn.execute_batch("VACUUM").map_err(sql_err)?;
            }
            n
        };
        self.index
            .retain(|e| e.log_ts.unwrap_or(e.wall_ts) >= cutoff_ts);
        Ok((deleted, self.index.first().map(|e| e.rowid)))
    }
}

/// Thread-safe wrapper used from async code.
pub type SharedJournal = Arc<RwLock<Journal>>;

#[cfg(test)]
mod tests {
    use super::*;

    fn make_journal(entries: &[(u64, Option<u64>)]) -> Journal {
        let dir = tempfile::tempdir().unwrap();
        let mut j = Journal::open(dir.path(), "mem").unwrap();
        for (i, &(wall_ts, log_ts)) in entries.iter().enumerate() {
            j.append(wall_ts, log_ts, i as u32, r#"{"seq":0,"events":[]}"#)
                .unwrap();
        }
        // tempdir is deleted here; the open connection keeps working on the
        // unlinked file for the duration of the test (POSIX) — index-only
        // assertions below never touch disk again anyway.
        std::mem::forget(dir);
        j
    }

    // ── len / is_empty ────────────────────────────────────────────────────────────
    #[test]
    fn len_empty() {
        assert_eq!(make_journal(&[]).len(), 0);
    }
    #[test]
    fn is_empty_true() {
        assert!(make_journal(&[]).is_empty());
    }
    #[test]
    fn len_nonempty() {
        assert_eq!(make_journal(&[(1, None), (2, None)]).len(), 2);
    }
    #[test]
    fn is_empty_false() {
        assert!(!make_journal(&[(1, None)]).is_empty());
    }

    // ── first_ts / last_ts ────────────────────────────────────────────────────────
    #[test]
    fn first_ts_empty() {
        assert_eq!(make_journal(&[]).first_ts(), None);
    }
    #[test]
    fn last_ts_empty() {
        assert_eq!(make_journal(&[]).last_ts(), None);
    }
    #[test]
    fn first_last_ts() {
        let j = make_journal(&[(100, None), (200, None), (300, None)]);
        assert_eq!(j.first_ts(), Some(100));
        assert_eq!(j.last_ts(), Some(300));
    }

    // ── log_first_ts / log_last_ts ────────────────────────────────────────────────
    #[test]
    fn log_ts_falls_back_to_wall_ts() {
        let j = make_journal(&[(100, None)]);
        assert_eq!(j.log_first_ts(), Some(100));
        assert_eq!(j.log_last_ts(), Some(100));
    }
    #[test]
    fn log_ts_uses_log_ts_when_present() {
        let j = make_journal(&[(100, Some(50)), (200, Some(150))]);
        assert_eq!(j.log_first_ts(), Some(50));
        assert_eq!(j.log_last_ts(), Some(150));
    }

    // ── ts_at / log_ts_at ─────────────────────────────────────────────────────────
    #[test]
    fn ts_at_valid() {
        let j = make_journal(&[(10, None), (20, None)]);
        assert_eq!(j.ts_at(0), Some(10));
        assert_eq!(j.ts_at(1), Some(20));
    }
    #[test]
    fn ts_at_out_of_range() {
        assert_eq!(make_journal(&[]).ts_at(0), None);
    }
    #[test]
    fn log_ts_at_fallback() {
        let j = make_journal(&[(10, None)]);
        assert_eq!(j.log_ts_at(0), Some(10)); // falls back to wall_ts
    }
    #[test]
    fn log_ts_at_explicit() {
        let j = make_journal(&[(10, Some(5))]);
        assert_eq!(j.log_ts_at(0), Some(5));
    }

    // ── seek_index ────────────────────────────────────────────────────────────────
    #[test]
    fn seek_index_empty() {
        assert_eq!(make_journal(&[]).seek_index(100), 0);
    }
    #[test]
    fn seek_index_before_all() {
        let j = make_journal(&[(100, None), (200, None), (300, None)]);
        assert_eq!(j.seek_index(50), 0);
    }
    #[test]
    fn seek_index_exact_match() {
        let j = make_journal(&[(100, None), (200, None), (300, None)]);
        assert_eq!(j.seek_index(200), 1);
    }
    #[test]
    fn seek_index_between_entries() {
        let j = make_journal(&[(100, None), (200, None), (300, None)]);
        assert_eq!(j.seek_index(150), 1); // first entry >= 150 is at index 1 (wall_ts=200)
    }
    #[test]
    fn seek_index_after_all() {
        let j = make_journal(&[(100, None), (200, None)]);
        assert_eq!(j.seek_index(999), 2); // past the end
    }

    // ── seek_index_by_log_ts ──────────────────────────────────────────────────────
    #[test]
    fn seek_by_log_ts_empty() {
        assert_eq!(make_journal(&[]).seek_index_by_log_ts(100), 0);
    }
    #[test]
    fn seek_by_log_ts_exact() {
        let j = make_journal(&[(100, Some(10)), (200, Some(20)), (300, Some(30))]);
        assert_eq!(j.seek_index_by_log_ts(20), 1);
    }
    #[test]
    fn seek_by_log_ts_fallback_to_wall() {
        // entries without log_ts fall back to wall_ts
        let j = make_journal(&[(100, None), (200, None)]);
        assert_eq!(j.seek_index_by_log_ts(150), 1);
    }
    #[test]
    fn seek_by_log_ts_past_end() {
        let j = make_journal(&[(100, Some(10))]);
        assert_eq!(j.seek_index_by_log_ts(999), 1);
    }
    #[test]
    fn seek_by_log_ts_unsorted_history_import() {
        // A historical import (log_ts 10, 20) lands AFTER newer live data
        // (log_ts 100): the array is not sorted by log_ts. First-match in
        // journal order is conservative — it may land early but never skips
        // batches at-or-after the target, unlike binary search on unsorted
        // data which lands at an arbitrary position.
        let j = make_journal(&[(1000, Some(100)), (2000, Some(10)), (3000, Some(20))]);
        assert_eq!(j.seek_index_by_log_ts(50), 0);
        assert_eq!(j.seek_index_by_log_ts(15), 0);
        assert_eq!(j.seek_index_by_log_ts(150), 3); // nothing >= 150 anywhere
    }

    // ── open / append / read_at (disk) ────────────────────────────────────────────
    #[test]
    fn append_and_read_at() {
        let dir = tempfile::tempdir().unwrap();
        let mut j = Journal::open(dir.path(), "test_stream").unwrap();
        assert!(j.is_empty());

        let batch_json = r#"{"seq":0,"events":[]}"#;
        j.append(1000, Some(900), 0, batch_json).unwrap();
        assert_eq!(j.len(), 1);
        assert_eq!(j.first_ts(), Some(1000));

        let content = j.read_at(0).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&content).unwrap();
        assert_eq!(parsed["seq"].as_u64().unwrap(), 0);
    }

    #[test]
    fn append_multiple_and_seek() {
        let dir = tempfile::tempdir().unwrap();
        let mut j = Journal::open(dir.path(), "stream2").unwrap();

        j.append(100, Some(10), 0, r#"{"seq":0,"events":[]}"#)
            .unwrap();
        j.append(200, Some(20), 1, r#"{"seq":1,"events":[]}"#)
            .unwrap();
        j.append(300, Some(30), 2, r#"{"seq":2,"events":[]}"#)
            .unwrap();

        assert_eq!(j.len(), 3);
        assert_eq!(j.seek_index(200), 1);
        assert_eq!(j.seek_index_by_log_ts(20), 1);
        assert_eq!(j.last_ts(), Some(300));
    }

    #[test]
    fn open_reloads_from_disk() {
        let dir = tempfile::tempdir().unwrap();
        {
            let mut j = Journal::open(dir.path(), "persist").unwrap();
            j.append(500, None, 0, r#"{"seq":0,"events":[]}"#).unwrap();
            j.append(600, None, 1, r#"{"seq":1,"events":[]}"#).unwrap();
        }
        // Re-open and verify the index is rebuilt
        let j2 = Journal::open(dir.path(), "persist").unwrap();
        assert_eq!(j2.len(), 2);
        assert_eq!(j2.first_ts(), Some(500));
        assert_eq!(j2.last_ts(), Some(600));
    }

    #[test]
    fn read_at_out_of_range() {
        let dir = tempfile::tempdir().unwrap();
        let j = Journal::open(dir.path(), "empty_stream").unwrap();
        assert!(j.read_at(0).is_none());
    }

    #[test]
    fn read_burst_reads_range() {
        let dir = tempfile::tempdir().unwrap();
        let mut j = Journal::open(dir.path(), "burst").unwrap();
        for i in 0..5u32 {
            j.append(
                100 + i as u64,
                None,
                i,
                &format!(r#"{{"seq":{i},"events":[]}}"#),
            )
            .unwrap();
        }
        let batches = j.read_burst(1, 3);
        assert_eq!(batches.len(), 3);
        let first: serde_json::Value = serde_json::from_str(&batches[0]).unwrap();
        assert_eq!(first["seq"].as_u64().unwrap(), 1);
    }

    // ── ids / prune ───────────────────────────────────────────────────────────────
    #[test]
    fn next_batch_id_and_pos_of_id() {
        let dir = tempfile::tempdir().unwrap();
        let mut j = Journal::open(dir.path(), "ids").unwrap();
        assert_eq!(j.next_batch_id(), 1);
        j.append(100, Some(10), 0, r#"{"seq":0,"events":[]}"#)
            .unwrap();
        j.append(200, Some(20), 1, r#"{"seq":1,"events":[]}"#)
            .unwrap();
        assert_eq!(j.next_batch_id(), 3);
        assert_eq!(j.pos_of_id(1), 0);
        assert_eq!(j.pos_of_id(2), 1);
        assert_eq!(j.pos_of_id(3), 2); // past the end
    }

    #[test]
    fn prune_deletes_old_and_keeps_ids_stable() {
        let dir = tempfile::tempdir().unwrap();
        let mut j = Journal::open(dir.path(), "prune").unwrap();
        j.append(1000, Some(100), 0, r#"{"seq":0,"events":[]}"#)
            .unwrap();
        j.append(2000, Some(200), 1, r#"{"seq":1,"events":[]}"#)
            .unwrap();
        j.append(3000, Some(300), 2, r#"{"seq":2,"events":[]}"#)
            .unwrap();

        let (deleted, min_id) = j.prune(250).unwrap();
        assert_eq!(deleted, 2);
        assert_eq!(j.len(), 1);
        assert_eq!(min_id, Some(3)); // surviving batch keeps its original id
        assert_eq!(j.log_first_ts(), Some(300));

        // New appends continue after the highest id ever used, never reusing 1/2.
        assert_eq!(j.next_batch_id(), 4);
        j.append(4000, Some(400), 3, r#"{"seq":3,"events":[]}"#)
            .unwrap();
        assert_eq!(j.pos_of_id(4), 1);

        // Survives a reopen.
        drop(j);
        let j2 = Journal::open(dir.path(), "prune").unwrap();
        assert_eq!(j2.len(), 2);
        assert_eq!(j2.log_first_ts(), Some(300));
        assert_eq!(j2.next_batch_id(), 5);
    }

    #[test]
    fn prune_nothing_to_delete() {
        let dir = tempfile::tempdir().unwrap();
        let mut j = Journal::open(dir.path(), "prune_noop").unwrap();
        j.append(1000, Some(100), 0, r#"{"seq":0,"events":[]}"#)
            .unwrap();
        let (deleted, min_id) = j.prune(50).unwrap();
        assert_eq!(deleted, 0);
        assert_eq!(min_id, Some(1));
        assert_eq!(j.len(), 1);
    }
}
