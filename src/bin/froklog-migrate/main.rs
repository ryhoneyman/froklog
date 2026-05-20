/// One-shot migration tool: convert JSONL journal files to the binary format.
///
/// Usage: froklog-migrate [streams-dir]
///
/// Reads each <streams-dir>/<id>/journal.jsonl that still has legacy JSONL
/// records (first byte == `{`), rewrites it as an all-binary file, then
/// atomically renames the temp file over the original.  Binary records that
/// are already in the new format are decoded and re-encoded (preserving all
/// fields).  Run with the server stopped to avoid concurrent write races.
use flate2::read::ZlibDecoder;
use flate2::write::ZlibEncoder;
use flate2::Compression;
use serde::Deserialize;
use std::io::{BufRead, BufWriter, Read, Write};
use std::path::Path;

#[derive(Deserialize)]
struct LegacyRecord {
    wall_ts: u64,
    #[serde(default)]
    log_ts: Option<u64>,
    #[serde(default)]
    seq: u32,
    batch: serde_json::Value,
}

struct Record {
    wall_ts: u64,
    log_ts: u64,
    seq: u32,
    batch_json: String,
}

fn read_all_records(path: &Path) -> std::io::Result<Vec<Record>> {
    let file = std::fs::File::open(path)?;
    let mut reader = std::io::BufReader::new(file);
    let mut records = Vec::new();

    loop {
        let mut magic = [0u8; 1];
        match reader.read_exact(&mut magic) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => break,
            Err(e) => return Err(e),
        }

        if magic[0] == b'{' {
            let mut rest = Vec::new();
            reader.read_until(b'\n', &mut rest)?;
            let mut line = vec![b'{'];
            line.extend_from_slice(&rest);
            if let Ok(s) = std::str::from_utf8(&line) {
                match serde_json::from_str::<LegacyRecord>(s.trim_end()) {
                    Ok(rec) => {
                        let batch_json = serde_json::to_string(&rec.batch)
                            .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
                        records.push(Record {
                            wall_ts: rec.wall_ts,
                            log_ts: rec.log_ts.unwrap_or(0),
                            seq: rec.seq,
                            batch_json,
                        });
                    }
                    Err(e) => eprintln!("  Skipping malformed JSONL record: {e}"),
                }
            }
        } else if magic[0] == 0x01 {
            let mut hdr = [0u8; 24];
            match reader.read_exact(&mut hdr) {
                Ok(()) => {}
                Err(_) => break,
            }
            let wall_ts = u64::from_le_bytes(hdr[0..8].try_into().unwrap());
            let log_ts = u64::from_le_bytes(hdr[8..16].try_into().unwrap());
            let seq = u32::from_le_bytes(hdr[16..20].try_into().unwrap());
            let clen = u32::from_le_bytes(hdr[20..24].try_into().unwrap()) as usize;
            let mut compressed = vec![0u8; clen];
            match reader.read_exact(&mut compressed) {
                Ok(()) => {}
                Err(_) => break,
            }
            let mut dec = ZlibDecoder::new(compressed.as_slice());
            let mut batch_json = String::new();
            if dec.read_to_string(&mut batch_json).is_ok() {
                records.push(Record {
                    wall_ts,
                    log_ts,
                    seq,
                    batch_json,
                });
            } else {
                eprintln!("  Skipping binary record with corrupt payload");
            }
        } else {
            // Unknown byte — skip to next newline as best-effort recovery.
            let mut rest = Vec::new();
            reader.read_until(b'\n', &mut rest)?;
            eprintln!("  Skipping unknown record magic 0x{:02x}", magic[0]);
        }
    }

    Ok(records)
}

fn write_binary_record<W: Write>(writer: &mut W, rec: &Record) -> std::io::Result<()> {
    let mut enc = ZlibEncoder::new(Vec::new(), Compression::fast());
    enc.write_all(rec.batch_json.as_bytes())?;
    let compressed = enc.finish()?;
    let clen = compressed.len() as u32;

    let mut header = [0u8; 25];
    header[0] = 0x01;
    header[1..9].copy_from_slice(&rec.wall_ts.to_le_bytes());
    header[9..17].copy_from_slice(&rec.log_ts.to_le_bytes());
    header[17..21].copy_from_slice(&rec.seq.to_le_bytes());
    header[21..25].copy_from_slice(&clen.to_le_bytes());

    writer.write_all(&header)?;
    writer.write_all(&compressed)?;
    Ok(())
}

fn has_legacy_records(path: &Path) -> std::io::Result<bool> {
    let mut f = std::fs::File::open(path)?;
    let mut byte = [0u8; 1];
    match f.read_exact(&mut byte) {
        Ok(()) => Ok(byte[0] == b'{'),
        Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => Ok(false),
        Err(e) => Err(e),
    }
}

fn migrate_journal(path: &Path) -> std::io::Result<()> {
    let original_size = std::fs::metadata(path)?.len();

    if !has_legacy_records(path)? {
        println!("  Already binary, skipping ({original_size} bytes)");
        return Ok(());
    }

    println!("  Reading {original_size} bytes ...");
    let records = read_all_records(path)?;
    println!("  Parsed {} records", records.len());

    let tmp_path = path.with_extension("jsonl.tmp");
    {
        let out_file = std::fs::File::create(&tmp_path)?;
        let mut writer = BufWriter::new(out_file);
        for rec in &records {
            write_binary_record(&mut writer, rec)?;
        }
        writer.flush()?;
    }

    let new_size = std::fs::metadata(&tmp_path)?.len();
    std::fs::rename(&tmp_path, path)?;
    println!(
        "  Done: {original_size} → {new_size} bytes ({:.1}% of original)",
        100.0 * new_size as f64 / original_size as f64
    );
    Ok(())
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let data_dir = std::path::PathBuf::from(if args.len() > 1 { &args[1] } else { "streams" });

    println!("Migrating journals in: {}", data_dir.display());

    let entries = match std::fs::read_dir(&data_dir) {
        Ok(e) => e,
        Err(e) => {
            eprintln!("Cannot read {}: {e}", data_dir.display());
            std::process::exit(1);
        }
    };

    let mut ok = 0usize;
    let mut err = 0usize;

    for entry in entries.flatten() {
        let journal = entry.path().join("journal.jsonl");
        if !journal.exists() {
            continue;
        }
        println!("Stream: {}", entry.file_name().to_string_lossy());
        match migrate_journal(&journal) {
            Ok(()) => ok += 1,
            Err(e) => {
                eprintln!("  ERROR: {e}");
                err += 1;
            }
        }
    }

    println!("\nMigration complete: {ok} ok, {err} errors");
    if err > 0 {
        std::process::exit(1);
    }
}
