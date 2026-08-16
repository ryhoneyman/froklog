//! Client-side time model: convert naive EQ-log timestamps to true unix epoch.
//!
//! EverQuest stamps log lines with the machine's local wall-clock and no
//! timezone marker. Historically froklog stored that naive reading as a
//! "fake UTC" epoch (display-preserving, but hours off from real time and
//! non-monotonic across DST fall-back). This module converts naive log time
//! to a **true** epoch instead:
//!
//! 1. The system's IANA timezone (via `iana-time-zone` + `chrono-tz`) supplies
//!    the correct UTC offset *for the date being converted* — so historical
//!    imports apply the DST rule that was in force back then, identically on
//!    Windows and Linux (Windows' own APIs apply current-year rules to old
//!    dates, which is why `chrono::Local` is only the fallback here).
//! 2. A measured server clock skew (see the pusher's time probe) is added on
//!    top, correcting machines whose clock is simply set wrong. One process
//!    talks to one froklog server, so the skew is a process-wide atomic.
//!
//! DST edge cases: during fall-back the same local hour occurs twice — the
//! earlier instant is chosen. During spring-forward a local hour doesn't
//! exist — the reading is shifted forward one hour.

use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::OnceLock;

use chrono::{NaiveDateTime, Offset, TimeZone};

/// Measured offset of the froklog server's clock relative to this machine,
/// in whole seconds (server = local + skew). Updated by the pusher's time
/// probe; zero until a probe completes (and stays zero on a healthy,
/// NTP-synced machine — sub-second skew rounds away against 1 s log lines).
pub static SERVER_SKEW_SECS: AtomicI64 = AtomicI64::new(0);

fn system_tz() -> Option<chrono_tz::Tz> {
    static TZ: OnceLock<Option<chrono_tz::Tz>> = OnceLock::new();
    *TZ.get_or_init(|| {
        iana_time_zone::get_timezone()
            .ok()
            .and_then(|name| name.parse().ok())
    })
}

/// Convert a naive local datetime from the EQ log into a true unix epoch
/// (seconds), applying the per-date timezone rules plus measured server skew.
pub fn naive_log_time_to_epoch(naive: NaiveDateTime) -> i64 {
    let base = system_tz()
        .and_then(|tz| {
            tz.from_local_datetime(&naive)
                .earliest()
                .or_else(|| {
                    // Spring-forward gap: this local time never existed; the
                    // clock had already jumped ahead, so shift one hour.
                    tz.from_local_datetime(&(naive + chrono::Duration::hours(1)))
                        .earliest()
                })
                .map(|dt| dt.timestamp())
        })
        .or_else(|| {
            // No IANA zone resolvable — fall back to the platform-local rules.
            chrono::Local
                .from_local_datetime(&naive)
                .earliest()
                .map(|dt| dt.timestamp())
        })
        .unwrap_or_else(|| naive.and_utc().timestamp());
    base + SERVER_SKEW_SECS.load(Ordering::Relaxed)
}

/// Current UTC offset of the system timezone in seconds (local = UTC + offset).
/// Reported to the server so it can label sessions with the streamer's
/// calendar dates.
pub fn local_utc_offset_secs() -> i64 {
    chrono::Local::now().offset().fix().local_minus_utc() as i64
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::NaiveDate;

    // Serializes tests that touch the process-wide SERVER_SKEW_SECS static,
    // which cargo test's default thread-per-test parallelism would
    // otherwise race (one test observing a skew value another test set and
    // hasn't reset yet).
    static SKEW_TEST_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    #[test]
    fn epoch_is_monotonic_across_consecutive_seconds() {
        let _guard = SKEW_TEST_LOCK.lock().unwrap();
        let d = NaiveDate::from_ymd_opt(2026, 7, 30).unwrap();
        let a = naive_log_time_to_epoch(d.and_hms_opt(20, 15, 1).unwrap());
        let b = naive_log_time_to_epoch(d.and_hms_opt(20, 15, 2).unwrap());
        assert_eq!(b - a, 1);
    }

    #[test]
    fn epoch_differs_from_fake_utc_by_tz_offset() {
        let _guard = SKEW_TEST_LOCK.lock().unwrap();
        // Unless the machine runs in UTC, true epoch != fake-UTC epoch, and
        // the difference is exactly a whole number of minutes (a tz offset).
        let naive = NaiveDate::from_ymd_opt(2026, 1, 15)
            .unwrap()
            .and_hms_opt(12, 0, 0)
            .unwrap();
        let fake = naive.and_utc().timestamp();
        let real = naive_log_time_to_epoch(naive);
        assert_eq!((real - fake) % 60, 0);
    }

    #[test]
    fn skew_is_applied() {
        let _guard = SKEW_TEST_LOCK.lock().unwrap();
        let naive = NaiveDate::from_ymd_opt(2026, 7, 30)
            .unwrap()
            .and_hms_opt(10, 0, 0)
            .unwrap();
        let before = naive_log_time_to_epoch(naive);
        SERVER_SKEW_SECS.store(7, Ordering::Relaxed);
        let after = naive_log_time_to_epoch(naive);
        SERVER_SKEW_SECS.store(0, Ordering::Relaxed);
        assert_eq!(after - before, 7);
    }
}
