/// Per-stream named-NPC curation: the stream owner's judgment layered over
/// the viewer's automatic named/trash heuristic.
///
/// The heuristic (capitalized proper noun, corpse-concurrency demotion) is
/// right most of the time, but the game itself offers no authoritative list —
/// so the owner can pin any NPC: `named` forces the ★ treatment, `trash`
/// strips it (capitalized farm-elites like Innoruuk`s Chosen), and deleting
/// the row returns the NPC to automatic.
///
/// Stored in the stream's SQLite database (`mob_overrides` table, schema in
/// `journal.rs::open_db`), keyed by lowercased name so log-line case flips
/// ("Orc slaver" / "orc slaver") share one row. Like markers: written rarely,
/// read when a viewer loads — direct queries, no cache.
use std::sync::{Arc, Mutex};

use rusqlite::Connection;
use serde::Serialize;

#[derive(Debug, Clone, Serialize)]
pub struct MobOverride {
    /// Lowercased NPC name (the match key).
    pub name: String,
    /// "named" or "trash".
    pub kind: String,
}

pub struct MobOverrides {
    conn: Mutex<Connection>,
}

fn sql_err(e: rusqlite::Error) -> std::io::Error {
    std::io::Error::other(e)
}

impl MobOverrides {
    pub fn open(data_dir: &std::path::Path, stream_id: &str) -> std::io::Result<Self> {
        let conn = crate::journal::open_db(data_dir, stream_id)?;
        Ok(Self {
            conn: Mutex::new(conn),
        })
    }

    /// Set an override. `kind` must be "named" or "trash" (the handler
    /// validates); upserts on the lowercased name.
    pub fn set(&self, name: &str, kind: &str) -> std::io::Result<()> {
        let conn = self.conn.lock().expect("mob_overrides db mutex");
        conn.prepare_cached(
            "INSERT INTO mob_overrides (name, kind) VALUES (?1, ?2)
             ON CONFLICT(name) DO UPDATE SET kind = excluded.kind",
        )
        .map_err(sql_err)?
        .execute(rusqlite::params![name.to_lowercase(), kind])
        .map_err(sql_err)?;
        Ok(())
    }

    /// Remove an override — the NPC goes back to the automatic heuristic.
    /// Returns true when a row was deleted.
    pub fn clear(&self, name: &str) -> std::io::Result<bool> {
        let conn = self.conn.lock().expect("mob_overrides db mutex");
        let n = conn
            .execute(
                "DELETE FROM mob_overrides WHERE name = ?1",
                [name.to_lowercase()],
            )
            .map_err(sql_err)?;
        Ok(n > 0)
    }

    /// All overrides, name order.
    pub fn list(&self) -> std::io::Result<Vec<MobOverride>> {
        let conn = self.conn.lock().expect("mob_overrides db mutex");
        let mut stmt = conn
            .prepare_cached("SELECT name, kind FROM mob_overrides ORDER BY name")
            .map_err(sql_err)?;
        let rows = stmt
            .query_map([], |row| {
                Ok(MobOverride {
                    name: row.get(0)?,
                    kind: row.get(1)?,
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
pub type SharedMobOverrides = Arc<MobOverrides>;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn set_upserts_and_lowercases() {
        let dir = tempfile::tempdir().unwrap();
        let o = MobOverrides::open(dir.path(), "s1").unwrap();
        o.set("Emperor Crush", "named").unwrap();
        o.set("EMPEROR CRUSH", "trash").unwrap(); // upsert, same key
        let list = o.list().unwrap();
        assert_eq!(list.len(), 1);
        assert_eq!(list[0].name, "emperor crush");
        assert_eq!(list[0].kind, "trash");
    }

    #[test]
    fn clear_returns_to_automatic() {
        let dir = tempfile::tempdir().unwrap();
        let o = MobOverrides::open(dir.path(), "s2").unwrap();
        o.set("Innoruuk`s Chosen", "trash").unwrap();
        assert!(o.clear("innoruuk`s chosen").unwrap());
        assert!(!o.clear("innoruuk`s chosen").unwrap()); // already gone
        assert!(o.list().unwrap().is_empty());
    }

    #[test]
    fn persists_across_reopen() {
        let dir = tempfile::tempdir().unwrap();
        {
            let o = MobOverrides::open(dir.path(), "s3").unwrap();
            o.set("Chokehold", "named").unwrap();
        }
        let o2 = MobOverrides::open(dir.path(), "s3").unwrap();
        assert_eq!(o2.list().unwrap().len(), 1);
    }
}
