/// Per-segment roster: who counts toward a session's or raid's aggregate.
///
/// A segment is a stretch of the timeline identified by the log timestamp it
/// starts at — a session boundary or a `raid_start` marker. Everyone seen
/// inside it counts by default; this table records the exceptions.
///
/// Exclusions rather than a whitelist, because membership cannot be derived:
/// `/who` returns everyone in the ZONE, not the raid (observed blocks of 1 to
/// 14 people in a single log), so the roster over-collects; and a healer may
/// never swing, so damage cannot be used to filter it down either. Defaulting
/// to "included" also means someone who joins mid-raid appears without anyone
/// having to remember to add them — the failure mode is showing one row too
/// many, not silently dropping a player from the numbers.
///
/// Stored in the stream's SQLite database (`segment_members` table, schema in
/// `journal.rs::open_db`). Written a handful of times per raid and read when a
/// viewer opens a segment, so direct queries — no cache, like markers.
use std::sync::{Arc, Mutex};

use rusqlite::Connection;
use serde::Serialize;

#[derive(Debug, Clone, Serialize)]
pub struct Exclusion {
    /// Log timestamp the segment starts at — its stable identity.
    pub seg_ts: u64,
    /// Player name as displayed (case preserved for the UI, matched lowercased).
    pub name: String,
}

pub struct SegmentRoster {
    conn: Mutex<Connection>,
}

fn sql_err(e: rusqlite::Error) -> std::io::Error {
    std::io::Error::other(e)
}

impl SegmentRoster {
    pub fn open(data_dir: &std::path::Path, stream_id: &str) -> std::io::Result<Self> {
        let conn = crate::journal::open_db(data_dir, stream_id)?;
        Ok(Self {
            conn: Mutex::new(conn),
        })
    }

    /// Exclude a player from one segment's aggregate.
    pub fn exclude(&self, seg_ts: u64, name: &str) -> std::io::Result<()> {
        let conn = self.conn.lock().expect("segment_members db mutex");
        conn.prepare_cached(
            "INSERT INTO segment_members (seg_ts, name, display) VALUES (?1, ?2, ?3)
             ON CONFLICT(seg_ts, name) DO UPDATE SET display = excluded.display",
        )
        .map_err(sql_err)?
        .execute(rusqlite::params![seg_ts as i64, name.to_lowercase(), name])
        .map_err(sql_err)?;
        Ok(())
    }

    /// Put a player back in. Returns true when a row was removed.
    pub fn include(&self, seg_ts: u64, name: &str) -> std::io::Result<bool> {
        let conn = self.conn.lock().expect("segment_members db mutex");
        let n = conn
            .execute(
                "DELETE FROM segment_members WHERE seg_ts = ?1 AND name = ?2",
                rusqlite::params![seg_ts as i64, name.to_lowercase()],
            )
            .map_err(sql_err)?;
        Ok(n > 0)
    }

    /// Everyone excluded anywhere in this stream, so the viewer can fetch once
    /// and apply per segment as the user scrolls.
    pub fn list(&self) -> std::io::Result<Vec<Exclusion>> {
        let conn = self.conn.lock().expect("segment_members db mutex");
        let mut stmt = conn
            .prepare_cached("SELECT seg_ts, display FROM segment_members ORDER BY seg_ts, name")
            .map_err(sql_err)?;
        let rows = stmt
            .query_map([], |row| {
                Ok(Exclusion {
                    seg_ts: row.get::<_, i64>(0)? as u64,
                    name: row.get(1)?,
                })
            })
            .map_err(sql_err)?;
        let mut out = Vec::new();
        for r in rows {
            out.push(r.map_err(sql_err)?);
        }
        Ok(out)
    }
}

/// Thread-safe handle (interior mutex, so no outer RwLock needed).
pub type SharedSegmentRoster = Arc<SegmentRoster>;

#[cfg(test)]
mod tests {
    use super::*;

    fn store() -> (tempfile::TempDir, SegmentRoster) {
        let dir = tempfile::tempdir().unwrap();
        let s = SegmentRoster::open(dir.path(), "seg").unwrap();
        (dir, s)
    }

    /// Excluding is per SEGMENT: dropping a zone random from tonight's raid
    /// must not touch last week's, where they may have been a real member.
    #[test]
    fn exclusions_do_not_leak_between_segments() {
        let (_d, s) = store();
        s.exclude(1000, "Randomguy").unwrap();
        let all = s.list().unwrap();
        assert_eq!(all.len(), 1);
        assert_eq!(all[0].seg_ts, 1000);
        assert_eq!(all[0].name, "Randomguy", "display case is preserved");

        s.exclude(2000, "Someoneelse").unwrap();
        let by_seg: Vec<_> = s.list().unwrap().iter().map(|e| e.seg_ts).collect();
        assert_eq!(by_seg, vec![1000, 2000]);
    }

    /// Log lines flip case on names; one player must not become two rows.
    #[test]
    fn the_same_player_in_either_case_is_one_row() {
        let (_d, s) = store();
        s.exclude(1000, "Zarri").unwrap();
        s.exclude(1000, "zarri").unwrap();
        assert_eq!(s.list().unwrap().len(), 1);
        assert!(
            s.include(1000, "ZARRI").unwrap(),
            "matched case-insensitively"
        );
        assert!(s.list().unwrap().is_empty());
    }

    /// Re-including someone who was never excluded is not an error — the
    /// viewer sends the whole picked set, not a diff.
    #[test]
    fn including_an_already_included_player_is_harmless() {
        let (_d, s) = store();
        assert!(!s.include(1000, "Nobody").unwrap());
        assert!(s.list().unwrap().is_empty());
    }
}
