/// Persistent on-disk journal for a single stream.
///
/// Layout: `<data_dir>/<stream_id>/journal.jsonl`
///
/// New records use a 25-byte binary header (magic=0x01) followed by
/// zlib-compressed batch JSON. Legacy records are newline-delimited JSON
/// (magic=`{`) and remain readable indefinitely. Both formats coexist in the
/// same file; `open()` detects format per-record from the first byte.
///
/// Binary record layout:
///   [u8  0x01]  magic
///   [u64 LE  ]  wall_ts  (unix seconds)
///   [u64 LE  ]  log_ts   (EQ log unix seconds; 0 = absent)
///   [u32 LE  ]  seq
///   [u32 LE  ]  clen     (byte length of compressed payload)
///   [N bytes ]  zlib-compressed batch JSON
///
/// An in-memory index maps wall_ts → byte offset so seek is O(log n).
use std::io::{BufRead, Read, Seek, SeekFrom, Write};
use std::path::PathBuf;
use std::sync::Arc;

use flate2::read::ZlibDecoder;
use flate2::write::ZlibEncoder;
use flate2::Compression;
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

/// Read one record (JSONL or binary) from `reader`, returning the batch JSON string.
fn read_one_record<R: BufRead>(reader: &mut R) -> Option<String> {
    let mut magic = [0u8; 1];
    reader.read_exact(&mut magic).ok()?;
    match magic[0] {
        b'{' => {
            // Legacy JSONL: read rest of line, parse JournalLine, return batch field.
            let mut rest = Vec::new();
            reader.read_until(b'\n', &mut rest).ok()?;
            let mut line = vec![b'{'];
            line.extend_from_slice(&rest);
            let s = std::str::from_utf8(&line).ok()?;
            let jl: JournalLine = serde_json::from_str(s.trim_end()).ok()?;
            serde_json::to_string(&jl.batch).ok()
        }
        0x01 => {
            // Binary: 24-byte header then compressed payload.
            let mut hdr = [0u8; 24];
            reader.read_exact(&mut hdr).ok()?;
            let clen = u32::from_le_bytes(hdr[20..24].try_into().unwrap()) as usize;
            let mut compressed = vec![0u8; clen];
            reader.read_exact(&mut compressed).ok()?;
            decompress_batch(&compressed)
        }
        _ => None,
    }
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

            loop {
                let record_offset = offset;
                let mut magic = [0u8; 1];
                match reader.read_exact(&mut magic) {
                    Ok(()) => {}
                    Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => break,
                    Err(e) => return Err(e),
                }
                offset += 1;

                if magic[0] == b'{' {
                    // Legacy JSONL record.
                    let mut rest = Vec::new();
                    let n = reader.read_until(b'\n', &mut rest)?;
                    offset += n as u64;
                    let mut line = vec![b'{'];
                    line.extend_from_slice(&rest);
                    if let Ok(s) = std::str::from_utf8(&line) {
                        if let Ok(jl) = serde_json::from_str::<JournalLine>(s.trim_end()) {
                            index.push(IndexEntry {
                                wall_ts: jl.wall_ts,
                                log_ts: jl.log_ts,
                                byte_offset: record_offset,
                            });
                        }
                    }
                } else if magic[0] == 0x01 {
                    // Binary record: read 24-byte header, skip compressed payload.
                    let mut hdr = [0u8; 24];
                    match reader.read_exact(&mut hdr) {
                        Ok(()) => {}
                        Err(_) => break,
                    }
                    offset += 24;
                    let wall_ts = u64::from_le_bytes(hdr[0..8].try_into().unwrap());
                    let log_ts_raw = u64::from_le_bytes(hdr[8..16].try_into().unwrap());
                    let clen = u32::from_le_bytes(hdr[20..24].try_into().unwrap()) as usize;
                    let mut skip = vec![0u8; clen];
                    match reader.read_exact(&mut skip) {
                        Ok(()) => {}
                        Err(_) => break,
                    }
                    offset += clen as u64;
                    index.push(IndexEntry {
                        wall_ts,
                        log_ts: if log_ts_raw == 0 {
                            None
                        } else {
                            Some(log_ts_raw)
                        },
                        byte_offset: record_offset,
                    });
                } else {
                    // Unknown byte — skip to next newline as best-effort recovery.
                    let mut rest = Vec::new();
                    let n = reader.read_until(b'\n', &mut rest)?;
                    offset += n as u64;
                }
            }
            info!(
                "Journal [{stream_id}]: loaded {} batches from disk",
                index.len()
            );
        }

        Ok(Self { path, index })
    }

    /// Append a raw EventBatch JSON string received at `wall_ts` to disk and update the index.
    /// `log_ts` is the max EQ log-event unix timestamp from the batch (used for replay pacing).
    pub fn append(
        &mut self,
        wall_ts: u64,
        log_ts: Option<u64>,
        seq: u32,
        batch_json: &str,
    ) -> std::io::Result<()> {
        let compressed = compress_batch(batch_json)?;
        let clen = compressed.len() as u32;

        let mut header = [0u8; 25];
        header[0] = 0x01;
        header[1..9].copy_from_slice(&wall_ts.to_le_bytes());
        header[9..17].copy_from_slice(&log_ts.unwrap_or(0).to_le_bytes());
        header[17..21].copy_from_slice(&seq.to_le_bytes());
        header[21..25].copy_from_slice(&clen.to_le_bytes());

        let mut file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.path)?;

        let byte_offset = file.seek(SeekFrom::End(0))?;
        file.write_all(&header)?;
        file.write_all(&compressed)?;
        file.flush()?;

        self.index.push(IndexEntry {
            wall_ts,
            log_ts,
            byte_offset,
        });
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

    /// Read a single batch JSON from disk by its index position.
    /// Returns `None` if the position is out of range or the file cannot be read.
    pub fn read_at(&self, pos: usize) -> Option<Arc<String>> {
        let entry = self.index.get(pos)?;
        let mut file = std::fs::File::open(&self.path).ok()?;
        file.seek(SeekFrom::Start(entry.byte_offset)).ok()?;
        let mut reader = std::io::BufReader::new(file);
        read_one_record(&mut reader).map(Arc::new)
    }

    /// Read up to `count` batches sequentially from disk starting at index position `pos`.
    /// Opens the file once and reads contiguous records, avoiding per-batch file overhead.
    /// Returns fewer than `count` entries only when the journal is exhausted.
    pub fn read_burst(&self, pos: usize, count: usize) -> Vec<Arc<String>> {
        if pos >= self.index.len() || count == 0 {
            return Vec::new();
        }
        let end = (pos + count).min(self.index.len());
        let entry = &self.index[pos];
        let mut file = match std::fs::File::open(&self.path) {
            Ok(f) => f,
            Err(_) => return Vec::new(),
        };
        if file.seek(SeekFrom::Start(entry.byte_offset)).is_err() {
            return Vec::new();
        }
        let mut reader = std::io::BufReader::new(file);
        let mut results = Vec::with_capacity(end - pos);
        for _ in pos..end {
            match read_one_record(&mut reader) {
                Some(json) => results.push(Arc::new(json)),
                None => break,
            }
        }
        results
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

    /// Truncate the on-disk journal to zero bytes and clear the in-memory index.
    pub fn clear(&mut self) -> std::io::Result<()> {
        if self.path.exists() {
            std::fs::OpenOptions::new()
                .write(true)
                .truncate(true)
                .open(&self.path)?;
        }
        self.index.clear();
        Ok(())
    }
}

/// Thread-safe wrapper used from async code.
pub type SharedJournal = Arc<RwLock<Journal>>;

#[cfg(test)]
mod tests {
    use super::*;

    fn make_journal(entries: &[(u64, Option<u64>)]) -> Journal {
        Journal {
            path: std::path::PathBuf::new(),
            index: entries
                .iter()
                .map(|&(wall_ts, log_ts)| IndexEntry {
                    wall_ts,
                    log_ts,
                    byte_offset: 0,
                })
                .collect(),
        }
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
        // read_at returns the batch field, not the full journal line
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
}
