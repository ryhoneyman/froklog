use std::path::Path;
use std::time::Duration;

use chrono::{Local, NaiveDateTime, NaiveTime};
use crossbeam_channel::Sender;
use tokio::fs::File;
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncSeekExt, BufReader, SeekFrom};
use tracing::{info, warn};

// ── Public config types ───────────────────────────────────────────────────────

pub enum TailFrom {
    End,
    Start,
    Date(NaiveDateTime),
}

pub struct TailConfig {
    pub from: TailFrom,
    pub to: Option<NaiveDateTime>,
    /// Replay speed multiplier (1.0 = real-time, 2.0 = 2× faster). None = no pacing.
    pub speed: Option<f64>,
    /// Dump mode: send lines as fast as possible and stop at EOF.
    pub dump: bool,
}

impl Default for TailConfig {
    fn default() -> Self {
        Self { from: TailFrom::End, to: None, speed: None, dump: false }
    }
}

// ── Entry point ───────────────────────────────────────────────────────────────

pub async fn tail(path: String, config: TailConfig, tx: Sender<String>) {
    let p = Path::new(&path);

    loop {
        if p.exists() { break; }
        warn!("Log file not found: {path} — retrying in 2s");
        tokio::time::sleep(Duration::from_secs(2)).await;
    }

    let mut file = File::open(&path).await.expect("open log file");

    let mut skipping = false;
    match &config.from {
        TailFrom::End => {
            file.seek(SeekFrom::End(0)).await.expect("seek to end");
            info!("Tailing {path} from EOF");
        }
        TailFrom::Start => {
            info!("Replaying {path} from the beginning");
        }
        TailFrom::Date(dt) => {
            info!("Seeking {path} to {dt}");
            seek_to_date(&mut file, *dt).await;
            skipping = true; // linear scan will align to exact line
        }
    }

    // Only stop at EOF when replaying a bounded window (from-start/from-date + --to),
    // or when dump mode is active (read the whole file then exit).
    let stop_at_eof = config.dump || (!matches!(&config.from, TailFrom::End) && config.to.is_some());
    let is_replay   = !config.dump && !matches!(&config.from, TailFrom::End);

    // (log_timestamp_of_first_sent_line, wall_instant_it_was_sent)
    let mut pace_anchor: Option<(NaiveDateTime, tokio::time::Instant)> = None;

    let mut reader = BufReader::new(file);
    let mut line = String::new();

    loop {
        let n = reader.read_line(&mut line).await.expect("read_line");

        if n == 0 {
            if stop_at_eof {
                info!("Replay complete (EOF reached before --to cutoff)");
                return;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
            continue;
        }

        let trimmed = line.trim_end_matches(['\n', '\r']);

        if !trimmed.is_empty() {
            // Skip lines until timestamp >= --from date (binary search may overshoot).
            if skipping {
                if let TailFrom::Date(from_dt) = &config.from {
                    if let Some(ts) = parse_eq_timestamp(trimmed) {
                        if ts >= *from_dt { skipping = false; }
                    }
                }
            }

            if !skipping {
                // Stop when we pass the --to cutoff.
                if let Some(to_dt) = &config.to {
                    if let Some(ts) = parse_eq_timestamp(trimmed) {
                        if ts > *to_dt {
                            info!("Replay complete (reached --to {to_dt})");
                            return;
                        }
                    }
                }

                if is_replay {
                    // Default to real-time (1.0×) when no --speed is given.
                    let speed = config.speed.unwrap_or(1.0);
                    if let Some(ts) = parse_eq_timestamp(trimmed) {
                        let anchor = pace_anchor
                            .get_or_insert_with(|| (ts, tokio::time::Instant::now()));
                        let log_ms = (ts - anchor.0).num_milliseconds() as f64;
                        let wall_target = anchor.1
                            + Duration::from_secs_f64(log_ms / 1000.0 / speed);
                        tokio::time::sleep_until(wall_target).await;
                    }
                }

                if tx.send(trimmed.to_owned()).is_err() {
                    return;
                }
            }
        }

        line.clear();
    }
}

// ── Binary seek ───────────────────────────────────────────────────────────────

/// Seek `file` to the byte just before the first line whose timestamp >= `target`.
/// Reads 512-byte chunks at binary-search midpoints; linear scan in the caller
/// aligns to the exact line.
async fn seek_to_date(file: &mut File, target: NaiveDateTime) {
    let file_len = match file.seek(SeekFrom::End(0)).await {
        Ok(n) => n,
        Err(_) => { let _ = file.seek(SeekFrom::Start(0)).await; return; }
    };

    let mut lo: u64 = 0;
    let mut hi: u64 = file_len;

    while hi.saturating_sub(lo) > 8192 {
        let mid = lo + (hi - lo) / 2;
        let _ = file.seek(SeekFrom::Start(mid)).await;

        let mut buf = [0u8; 512];
        let n = match file.read(&mut buf).await {
            Ok(0) | Err(_) => { hi = mid; continue; }
            Ok(n) => n,
        };

        // Skip the partial first line (we landed mid-line); use the next complete line.
        let text = String::from_utf8_lossy(&buf[..n]);
        let ts = text.split('\n').nth(1).and_then(|l| parse_eq_timestamp(l));

        match ts {
            Some(t) if t < target => lo = mid,
            Some(_) => hi = mid,
            None => lo = mid + 1,
        }
    }

    let _ = file.seek(SeekFrom::Start(lo)).await;
    info!("Binary search done, linear scan from byte {lo}");
}

// ── Timestamp parsing ─────────────────────────────────────────────────────────

/// Extract the EQ log timestamp from a raw line like `[Tue Jan 01 00:00:01 2000] ...`.
pub fn parse_eq_timestamp(line: &str) -> Option<NaiveDateTime> {
    if line.len() < 26 || !line.starts_with('[') { return None; }
    let ts = &line[1..25];
    // EQ zero-pads days in some versions and space-pads in others.
    NaiveDateTime::parse_from_str(ts, "%a %b %d %H:%M:%S %Y")
        .or_else(|_| NaiveDateTime::parse_from_str(ts, "%a %b %e %H:%M:%S %Y"))
        .ok()
}

// ── CLI helpers (called from main) ───────────────────────────────────────────

/// Parse a user-supplied date string into a `NaiveDateTime`.
///
/// Accepted formats (case-insensitive separators are fine):
/// - `YYYY-MM-DD HH:MM:SS`
/// - `YYYY-MM-DD HH:MM`
/// - `YYYY-MM-DD`  (time defaults to 00:00:00)
/// - `HH:MM:SS`    (date defaults to today)
/// - `HH:MM`       (date defaults to today)
pub fn parse_user_date(s: &str) -> Result<NaiveDateTime, String> {
    let s = s.trim();

    // Full datetime variants
    for fmt in &["%Y-%m-%d %H:%M:%S", "%Y-%m-%d %H:%M"] {
        if let Ok(dt) = NaiveDateTime::parse_from_str(s, fmt) {
            return Ok(dt);
        }
    }

    // Date-only → midnight
    if let Ok(d) = chrono::NaiveDate::parse_from_str(s, "%Y-%m-%d") {
        return Ok(d.and_hms_opt(0, 0, 0).unwrap());
    }

    // Time-only → today's date
    for fmt in &["%H:%M:%S", "%H:%M"] {
        if let Ok(t) = NaiveTime::parse_from_str(s, fmt) {
            return Ok(Local::now().date_naive().and_time(t));
        }
    }

    Err(format!(
        "Cannot parse '{s}' as a date. Try: YYYY-MM-DD HH:MM:SS  or  HH:MM:SS"
    ))
}

/// Parse a duration string like `1h30m`, `90m`, `3600s`, or a bare number of seconds.
pub fn parse_duration_str(s: &str) -> Result<chrono::Duration, String> {
    let s = s.trim();
    let mut total_secs: i64 = 0;
    let mut current: i64 = 0;
    let mut has_unit = false;

    for ch in s.chars() {
        match ch {
            '0'..='9' => current = current * 10 + (ch as i64 - '0' as i64),
            'h' | 'H' => { total_secs += current * 3600; current = 0; has_unit = true; }
            'm' | 'M' => { total_secs += current * 60;   current = 0; has_unit = true; }
            's' | 'S' => { total_secs += current;         current = 0; has_unit = true; }
            _ => return Err(format!(
                "Invalid duration '{s}': use e.g. 1h30m, 90m, 45s, or bare seconds"
            )),
        }
    }

    // Bare number with no unit = seconds
    if !has_unit { total_secs += current; }

    if total_secs <= 0 {
        return Err(format!("Duration '{s}' must be greater than zero"));
    }

    Ok(chrono::Duration::seconds(total_secs))
}
