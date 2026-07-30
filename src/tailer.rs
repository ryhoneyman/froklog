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
        Self {
            from: TailFrom::End,
            to: None,
            speed: None,
            dump: false,
        }
    }
}

// ── Entry point ───────────────────────────────────────────────────────────────

pub async fn tail(path: String, config: TailConfig, tx: Sender<String>) {
    let p = Path::new(&path);

    loop {
        if p.exists() {
            break;
        }
        warn!("Log file not found: {path} — retrying in 2s");
        tokio::time::sleep(Duration::from_secs(2)).await;
    }

    // Retry instead of panicking: the file existing a moment ago doesn't
    // guarantee it opens (permissions, still being created).
    let mut file = loop {
        match File::open(&path).await {
            Ok(f) => break f,
            Err(e) => {
                warn!("Cannot open {path}: {e} — retrying in 2s");
                tokio::time::sleep(Duration::from_secs(2)).await;
            }
        }
    };

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
    let stop_at_eof =
        config.dump || (!matches!(&config.from, TailFrom::End) && config.to.is_some());
    let is_replay = !config.dump && !matches!(&config.from, TailFrom::End);

    // (log_timestamp_of_first_sent_line, wall_instant_it_was_sent)
    let mut pace_anchor: Option<(NaiveDateTime, tokio::time::Instant)> = None;

    // Byte offset we have consumed so far — compared against the file's
    // current length to detect truncation (the classic EQ habit of deleting
    // the log; the client recreates it and this tailer must follow).
    let mut pos = file.stream_position().await.unwrap_or(0);

    let mut reader = BufReader::new(file);
    // Raw bytes, decoded lossily per line: EQ logs are Windows-1252 and a
    // single accented character used to kill this task via read_line's
    // strict-UTF-8 error path.
    let mut buf: Vec<u8> = Vec::new();

    loop {
        let n = match reader.read_until(b'\n', &mut buf).await {
            Ok(n) => n,
            Err(e) => {
                warn!("Tail read error on {path}: {e}");
                0
            }
        };
        pos += n as u64;

        // A line is complete only when the newline arrived; at EOF the game
        // may still be flushing the line, so keep accumulating rather than
        // emitting a fragment.
        let at_eof_final = n == 0 && stop_at_eof;
        if !buf.ends_with(b"\n") && !(at_eof_final && !buf.is_empty()) {
            if at_eof_final {
                info!("Replay complete (EOF reached before --to cutoff)");
                return;
            }
            if n == 0 {
                // Live tail idle: watch for truncation/recreation. Portable
                // size-regression check — no inodes, works on Windows too.
                if let Ok(md) = tokio::fs::metadata(&path).await {
                    if md.len() < pos {
                        warn!("{path} truncated or recreated — reopening from start");
                        match File::open(&path).await {
                            Ok(f) => {
                                reader = BufReader::new(f);
                                pos = 0;
                                buf.clear();
                            }
                            Err(e) => warn!("Reopen failed: {e} — will retry"),
                        }
                    }
                }
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
            continue;
        }

        let decoded = String::from_utf8_lossy(&buf);
        let trimmed = decoded.trim_end_matches(['\n', '\r']);

        if !trimmed.is_empty() {
            // Skip lines until timestamp >= --from date (binary search may overshoot).
            if skipping {
                if let TailFrom::Date(from_dt) = &config.from {
                    if let Some(ts) = parse_eq_timestamp(trimmed) {
                        if ts >= *from_dt {
                            skipping = false;
                        }
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
                        let anchor =
                            pace_anchor.get_or_insert_with(|| (ts, tokio::time::Instant::now()));
                        let log_ms = (ts - anchor.0).num_milliseconds() as f64;
                        let wall_target =
                            anchor.1 + Duration::from_secs_f64(log_ms / 1000.0 / speed);
                        tokio::time::sleep_until(wall_target).await;
                    }
                }

                if tx.send(trimmed.to_owned()).is_err() {
                    return;
                }
            }
        }

        if at_eof_final {
            info!("Replay complete (EOF reached before --to cutoff)");
            return;
        }
        buf.clear();
    }
}

// ── Binary seek ───────────────────────────────────────────────────────────────

/// Seek `file` to the byte just before the first line whose timestamp >= `target`.
/// Reads 512-byte chunks at binary-search midpoints; linear scan in the caller
/// aligns to the exact line.
async fn seek_to_date(file: &mut File, target: NaiveDateTime) {
    let file_len = match file.seek(SeekFrom::End(0)).await {
        Ok(n) => n,
        Err(_) => {
            let _ = file.seek(SeekFrom::Start(0)).await;
            return;
        }
    };

    let mut lo: u64 = 0;
    let mut hi: u64 = file_len;

    while hi.saturating_sub(lo) > 8192 {
        let mid = lo + (hi - lo) / 2;
        let _ = file.seek(SeekFrom::Start(mid)).await;

        let mut buf = [0u8; 512];
        let n = match file.read(&mut buf).await {
            Ok(0) | Err(_) => {
                hi = mid;
                continue;
            }
            Ok(n) => n,
        };

        // Skip the partial first line (we landed mid-line); use the next complete line.
        let text = String::from_utf8_lossy(&buf[..n]);
        let ts = text.split('\n').nth(1).and_then(parse_eq_timestamp);

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
    if line.len() < 26 || !line.starts_with('[') {
        return None;
    }
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
            'h' | 'H' => {
                total_secs += current * 3600;
                current = 0;
                has_unit = true;
            }
            'm' | 'M' => {
                total_secs += current * 60;
                current = 0;
                has_unit = true;
            }
            's' | 'S' => {
                total_secs += current;
                current = 0;
                has_unit = true;
            }
            _ => {
                return Err(format!(
                    "Invalid duration '{s}': use e.g. 1h30m, 90m, 45s, or bare seconds"
                ))
            }
        }
    }

    // Bare number with no unit = seconds
    if !has_unit {
        total_secs += current;
    }

    if total_secs <= 0 {
        return Err(format!("Duration '{s}' must be greater than zero"));
    }

    Ok(chrono::Duration::seconds(total_secs))
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::{Datelike, Timelike};

    // ── parse_eq_timestamp ────────────────────────────────────────────────────────
    #[test]
    fn timestamp_zero_padded_day() {
        // Feb 1, 2026 is a Sunday — day "01" (zero-padded)
        let dt = parse_eq_timestamp("[Sun Feb 01 22:00:07 2026] text").unwrap();
        assert_eq!(dt.day(), 1);
        assert_eq!(dt.month(), 2);
        assert_eq!(dt.year(), 2026);
    }
    #[test]
    fn timestamp_space_padded_day() {
        // Feb 7, 2026 is a Saturday — day " 7" (space-padded — some EQ versions)
        let dt = parse_eq_timestamp("[Sat Feb  7 22:00:07 2026] text").unwrap();
        assert_eq!(dt.day(), 7);
    }
    #[test]
    fn timestamp_two_digit_day() {
        // Feb 27, 2026 is a Friday
        let dt = parse_eq_timestamp("[Fri Feb 27 22:00:07 2026] body").unwrap();
        assert_eq!(dt.day(), 27);
        assert_eq!(dt.hour(), 22);
        assert_eq!(dt.minute(), 0);
        assert_eq!(dt.second(), 7);
    }
    #[test]
    fn timestamp_invalid() {
        assert!(parse_eq_timestamp("Not a log line").is_none());
    }
    #[test]
    fn timestamp_too_short() {
        assert!(parse_eq_timestamp("[short]").is_none());
    }
    #[test]
    fn timestamp_no_bracket() {
        assert!(parse_eq_timestamp("Fri Feb 27 22:00:07 2026] body").is_none());
    }

    // ── parse_user_date ───────────────────────────────────────────────────────────
    #[test]
    fn user_date_full_datetime() {
        let dt = parse_user_date("2026-05-14 10:30:00").unwrap();
        assert_eq!(dt.year(), 2026);
        assert_eq!(dt.month(), 5);
        assert_eq!(dt.day(), 14);
        assert_eq!(dt.hour(), 10);
        assert_eq!(dt.minute(), 30);
        assert_eq!(dt.second(), 0);
    }
    #[test]
    fn user_date_no_seconds() {
        let dt = parse_user_date("2026-05-14 10:30").unwrap();
        assert_eq!(dt.minute(), 30);
    }
    #[test]
    fn user_date_only() {
        let dt = parse_user_date("2026-05-14").unwrap();
        assert_eq!(dt.hour(), 0);
        assert_eq!(dt.second(), 0);
    }
    #[test]
    fn user_date_time_only_hhmm() {
        // Should succeed and use today's date
        assert!(parse_user_date("10:30").is_ok());
    }
    #[test]
    fn user_date_time_only_hhmmss() {
        assert!(parse_user_date("10:30:45").is_ok());
    }
    #[test]
    fn user_date_invalid() {
        assert!(parse_user_date("not-a-date").is_err());
    }

    // ── parse_duration_str ────────────────────────────────────────────────────────
    #[test]
    fn duration_hours() {
        assert_eq!(parse_duration_str("2h").unwrap().num_seconds(), 7200);
    }
    #[test]
    fn duration_minutes() {
        assert_eq!(parse_duration_str("90m").unwrap().num_seconds(), 5400);
    }
    #[test]
    fn duration_seconds() {
        assert_eq!(parse_duration_str("45s").unwrap().num_seconds(), 45);
    }
    #[test]
    fn duration_combined() {
        assert_eq!(parse_duration_str("1h30m").unwrap().num_seconds(), 5400);
    }
    #[test]
    fn duration_hms() {
        assert_eq!(parse_duration_str("1h2m3s").unwrap().num_seconds(), 3723);
    }
    #[test]
    fn duration_bare_secs() {
        assert_eq!(parse_duration_str("3600").unwrap().num_seconds(), 3600);
    }
    #[test]
    fn duration_uppercase() {
        assert_eq!(parse_duration_str("1H30M").unwrap().num_seconds(), 5400);
    }
    #[test]
    fn duration_zero_err() {
        assert!(parse_duration_str("0").is_err());
    }
    #[test]
    fn duration_invalid() {
        assert!(parse_duration_str("1x").is_err());
    }
    #[test]
    fn duration_empty_err() {
        assert!(parse_duration_str("").is_err());
    }
}
