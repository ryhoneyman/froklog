/// Per-stream time markers: user-defined boundaries like "raid start" /
/// "raid end" that slice the timeline independently of automatic sessions.
///
/// Stored in the stream's SQLite database (`markers` table, schema created in
/// `journal.rs::open_db`). Timestamps are true-epoch log time — the same
/// domain as event timestamps — so a marker pair can scope the viewer exactly
/// like a session does.
///
/// No in-memory cache: markers are written a handful of times per play
/// session and read when a viewer opens the list, so direct queries are the
/// simplest correct thing.
use std::sync::{Arc, Mutex};

use rusqlite::Connection;
use serde::Serialize;

#[derive(Debug, Clone, Serialize)]
pub struct Marker {
    pub id: i64,
    /// True-epoch unix seconds (log-time domain).
    pub ts: u64,
    /// Free-form kind, e.g. "raid_start", "raid_end", "group_start".
    pub kind: String,
    pub label: String,
}

pub struct Markers {
    conn: Mutex<Connection>,
}

fn sql_err(e: rusqlite::Error) -> std::io::Error {
    std::io::Error::other(e)
}

impl Markers {
    pub fn open(data_dir: &std::path::Path, stream_id: &str) -> std::io::Result<Self> {
        let conn = crate::journal::open_db(data_dir, stream_id)?;
        Ok(Self {
            conn: Mutex::new(conn),
        })
    }

    /// Insert a marker and return it with its assigned id.
    pub fn add(&self, ts: u64, kind: &str, label: &str) -> std::io::Result<Marker> {
        let conn = self.conn.lock().expect("markers db mutex");
        conn.prepare_cached("INSERT INTO markers (ts, kind, label) VALUES (?1, ?2, ?3)")
            .map_err(sql_err)?
            .execute(rusqlite::params![ts as i64, kind, label])
            .map_err(sql_err)?;
        Ok(Marker {
            id: conn.last_insert_rowid(),
            ts,
            kind: kind.to_string(),
            label: label.to_string(),
        })
    }

    /// All markers in time order.
    pub fn list(&self) -> std::io::Result<Vec<Marker>> {
        let conn = self.conn.lock().expect("markers db mutex");
        let mut stmt = conn
            .prepare_cached("SELECT id, ts, kind, label FROM markers ORDER BY ts, id")
            .map_err(sql_err)?;
        let rows = stmt
            .query_map([], |row| {
                Ok(Marker {
                    id: row.get(0)?,
                    ts: row.get::<_, i64>(1)? as u64,
                    kind: row.get(2)?,
                    label: row.get(3)?,
                })
            })
            .map_err(sql_err)?;
        let mut out = Vec::new();
        for r in rows {
            out.push(r.map_err(sql_err)?);
        }
        Ok(out)
    }

    /// Delete a marker by id. Returns true when something was deleted.
    pub fn delete(&self, id: i64) -> std::io::Result<bool> {
        let conn = self.conn.lock().expect("markers db mutex");
        let n = conn
            .execute("DELETE FROM markers WHERE id = ?1", [id])
            .map_err(sql_err)?;
        Ok(n > 0)
    }

    /// Delete markers older than a cutoff (retention/prune support).
    pub fn prune(&self, cutoff_ts: u64) -> std::io::Result<usize> {
        let conn = self.conn.lock().expect("markers db mutex");
        conn.execute("DELETE FROM markers WHERE ts < ?1", [cutoff_ts as i64])
            .map_err(sql_err)
    }
}

/// Thread-safe handle (interior mutex, so no outer RwLock needed).
pub type SharedMarkers = Arc<Markers>;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn add_list_delete_roundtrip() {
        let dir = tempfile::tempdir().unwrap();
        let m = Markers::open(dir.path(), "s1").unwrap();
        let a = m.add(1000, "raid_start", "Naggy raid").unwrap();
        let b = m.add(2000, "raid_end", "").unwrap();
        assert!(a.id < b.id);

        let list = m.list().unwrap();
        assert_eq!(list.len(), 2);
        assert_eq!(list[0].kind, "raid_start");
        assert_eq!(list[0].ts, 1000);
        assert_eq!(list[1].label, "");

        assert!(m.delete(a.id).unwrap());
        assert!(!m.delete(a.id).unwrap()); // already gone
        assert_eq!(m.list().unwrap().len(), 1);
    }

    #[test]
    fn prune_removes_old() {
        let dir = tempfile::tempdir().unwrap();
        let m = Markers::open(dir.path(), "s2").unwrap();
        m.add(100, "raid_start", "").unwrap();
        m.add(200, "raid_end", "").unwrap();
        m.add(900, "group_start", "").unwrap();
        assert_eq!(m.prune(500).unwrap(), 2);
        let rest = m.list().unwrap();
        assert_eq!(rest.len(), 1);
        assert_eq!(rest[0].kind, "group_start");
    }

    #[test]
    fn persists_across_reopen() {
        let dir = tempfile::tempdir().unwrap();
        {
            let m = Markers::open(dir.path(), "s3").unwrap();
            m.add(42, "raid_start", "x").unwrap();
        }
        let m2 = Markers::open(dir.path(), "s3").unwrap();
        assert_eq!(m2.list().unwrap().len(), 1);
    }
}
