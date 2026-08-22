// Refresh scheduling math and the pure decisions of the fetch pipeline.
//
// The timing semantics mirror the reference extension exactly
// (`extension.js:396-407`, `_restartTimeoutFromLongDate`): the next refresh
// is due 86 400 s after Bing's `fullstartdate`; an out-of-range difference
// is *reset* to 60 s (not clamped), and a 5-minute fudge offset is added
// afterwards in case of an inaccurate local clock.

use std::time::Duration;

use chrono::{DateTime, NaiveDateTime, Utc};

/// Cold start (empty catalogue): fetch shortly after startup.
pub const COLD_START_DELAY: Duration = Duration::from_secs(5);

/// Retry delay after an HTTP/parse error — one hour, matching the
/// reference's `TIMEOUT_SECONDS_ON_HTTP_ERROR`. Also used for a malformed
/// `fullstartdate` (an error path too: bad data from Bing or a corrupt
/// catalogue entry).
pub const ERROR_RETRY_DELAY: Duration = Duration::from_secs(3600);

/// Bing publishes a new image every 24 h after `fullstartdate`.
const REFRESH_PERIOD_SECS: i64 = 86_400;

/// Differences outside `MIN_DIFF_SECS..=MAX_DIFF_SECS` are reset to this.
const RESET_DIFF_SECS: i64 = 60;
const MIN_DIFF_SECS: i64 = 60;
const MAX_DIFF_SECS: i64 = 86_400;

/// Fudge offset added *after* the range check (`extension.js:405`).
const FUDGE_SECS: i64 = 300;

/// How long to wait before the next refresh.
///
/// `None` (cold start, nothing downloaded yet) → [`COLD_START_DELAY`].
/// Otherwise, the reference's exact math on the newest image's
/// `fullstartdate` (`YYYYMMDDHHMM`, UTC): `diff = (fullstartdate + 86400s)
/// - now; if diff < 60 || diff > 86400 { diff = 60 }; diff += 300`.
/// A `fullstartdate` that does not parse → [`ERROR_RETRY_DELAY`].
///
/// That error branch is not dead code: `bing::parse_image_list` rejects any
/// response entry whose `fullstartdate` is not 12 ASCII digits, so a
/// *shape* failure is reachable only from a corrupt catalogue entry
/// (hand-edited JSON), while 12 digits that are not a real date
/// (`202613450000`) still arrive here from either source.
pub fn next_refresh(newest_fullstartdate: Option<&str>, now: DateTime<Utc>) -> Duration {
    let Some(fullstartdate) = newest_fullstartdate else {
        return COLD_START_DELAY;
    };
    let Ok(start) = NaiveDateTime::parse_from_str(fullstartdate, "%Y%m%d%H%M") else {
        return ERROR_RETRY_DELAY;
    };
    let due = start.and_utc() + chrono::Duration::seconds(REFRESH_PERIOD_SECS);
    let mut diff = (due - now).num_seconds();
    if !(MIN_DIFF_SECS..=MAX_DIFF_SECS).contains(&diff) {
        diff = RESET_DIFF_SECS; // reset, not clamp — reference semantics
    }
    diff += FUDGE_SECS;
    Duration::from_secs(diff as u64) // diff >= 360 by construction
}

/// How many of the newest archive positions a refresh considers for
/// download. The list request itself is always the full supported window
/// (`bing::ARCHIVE_WINDOW`, eight entries — a few KB of JSON); retention
/// only decides which of those positions are worth ~5 MB each: 1–7 days
/// → that many newest positions (no point downloading eight just to prune
/// most of them), otherwise all eight (also for `0` = keep forever).
pub fn download_horizon(retention_days: u16) -> u8 {
    match retention_days {
        1..=7 => retention_days as u8,
        _ => crate::bing::ARCHIVE_WINDOW,
    }
}

/// Whether a retention change made the policy stricter — i.e. images that
/// were within the old limit may now be over the new one, so prune runs
/// immediately instead of waiting for the next fetch. `0` = keep forever:
/// never stricter as the new value, always loosened *from* (any finite new
/// value can only cut into a previously unbounded set).
pub fn retention_reduced(old_days: u16, new_days: u16) -> bool {
    if new_days == 0 {
        return false;
    }
    old_days == 0 || new_days < old_days
}

/// Fallback when a hand-edited `shuffle_interval_secs` is `0` (the daily
/// default, matching the dropdown's garbage fallback).
const SHUFFLE_FALLBACK_SECS: u64 = 86_400;

/// Floor for non-zero hand-edited intervals: anything shorter would strobe
/// the wallpaper.
const SHUFFLE_MIN_SECS: u64 = 60;

/// The shuffle timer's arming delay: one full sanitized interval. Enabling
/// shuffle, changing the interval, and manual navigation all re-arm the
/// timer, so each acts as a countdown reset; after every automatic tick
/// the next cycle is again one full interval. Hand-edited config values
/// are sanitized here — `0` falls back to the daily default and anything
/// under a minute is floored — so a bad value can never produce a
/// zero-delay wallpaper-strobe loop.
pub fn shuffle_interval(interval_secs: u32) -> Duration {
    match u64::from(interval_secs) {
        0 => Duration::from_secs(SHUFFLE_FALLBACK_SECS),
        s => Duration::from_secs(s.max(SHUFFLE_MIN_SECS)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    fn now() -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 8, 7, 12, 0, 0).unwrap()
    }

    #[test]
    fn cold_start_fires_in_five_seconds() {
        assert_eq!(next_refresh(None, now()), Duration::from_secs(5));
        assert_eq!(next_refresh(None, now()), COLD_START_DELAY);
    }

    #[test]
    fn normal_case_due_plus_fudge() {
        // Newest image started 2026-08-07 07:00 UTC → due 2026-08-08 07:00.
        // diff = 19 h = 68 400 s (in range) → + 300 fudge.
        assert_eq!(
            next_refresh(Some("202608070700"), now()),
            Duration::from_secs(68_400 + 300)
        );
    }

    #[test]
    fn in_the_past_resets_to_60_plus_fudge() {
        // Due long ago → negative diff → reset to 60, then + 300.
        assert_eq!(
            next_refresh(Some("202608010700"), now()),
            Duration::from_secs(60 + 300)
        );
    }

    #[test]
    fn far_future_resets_to_60_plus_fudge() {
        // Start a year ahead → diff far beyond 86 400 → reset, not clamp.
        assert_eq!(
            next_refresh(Some("202708070700"), now()),
            Duration::from_secs(60 + 300)
        );
    }

    #[test]
    fn boundary_diffs_are_kept_not_reset() {
        // diff exactly 60 (due = now + 60 s): kept → 60 + 300.
        assert_eq!(
            next_refresh(Some("202608061201"), now()), // + 86400 = 12:01
            Duration::from_secs(60 + 300)
        );
        // diff exactly 86 400 (start == now): kept → 86 400 + 300.
        assert_eq!(
            next_refresh(Some("202608071200"), now()),
            Duration::from_secs(86_400 + 300)
        );
        // `fullstartdate` has minute granularity: one minute past the upper
        // boundary resets. diff = 86 460 > 86 400 → 60 + 300.
        assert_eq!(
            next_refresh(Some("202608071201"), now()),
            Duration::from_secs(60 + 300)
        );
        // diff = 0 (due == now) < 60 → reset.
        assert_eq!(
            next_refresh(Some("202608061200"), now()),
            Duration::from_secs(60 + 300)
        );
    }

    #[test]
    fn malformed_date_backs_off_an_hour() {
        for bad in ["", "not-a-date", "2026", "20260807", "20261340070000x"] {
            assert_eq!(next_refresh(Some(bad), now()), ERROR_RETRY_DELAY, "{bad}");
        }
        // Month 13 parses as digits but is not a real date.
        assert_eq!(next_refresh(Some("202613070700"), now()), ERROR_RETRY_DELAY);
    }

    #[test]
    fn download_horizon_follows_retention() {
        assert_eq!(download_horizon(1), 1);
        assert_eq!(download_horizon(3), 3);
        assert_eq!(download_horizon(7), 7);
        assert_eq!(download_horizon(8), 8);
        assert_eq!(download_horizon(30), 8); // capped at the archive window
        assert_eq!(download_horizon(0), 8); // forever → full window
    }

    #[test]
    fn retention_reduced_only_when_the_new_policy_is_stricter() {
        // Stricter: fewer days, or finite where it was forever.
        assert!(retention_reduced(8, 3));
        assert!(retention_reduced(30, 8));
        assert!(retention_reduced(0, 30));
        assert!(retention_reduced(0, 3));
        // Loosened or unchanged: wait for the next fetch's routine prune.
        assert!(!retention_reduced(3, 8));
        assert!(!retention_reduced(8, 30));
        assert!(!retention_reduced(30, 0)); // forever keeps everything
        assert!(!retention_reduced(0, 0));
        assert!(!retention_reduced(8, 8));
    }

    #[test]
    fn shuffle_interval_passes_real_choices_through() {
        for secs in [1_800u32, 3_600, 21_600, 86_400] {
            assert_eq!(
                shuffle_interval(secs),
                Duration::from_secs(u64::from(secs)),
                "{secs}"
            );
        }
    }

    #[test]
    fn shuffle_interval_sanitizes_hand_edited_garbage() {
        // A hand-edited 0 must never yield a zero-delay strobe loop.
        assert_eq!(shuffle_interval(0), Duration::from_secs(86_400));
        // Tiny non-zero values are floored to a minute.
        assert_eq!(shuffle_interval(1), Duration::from_secs(60));
        assert_eq!(shuffle_interval(59), Duration::from_secs(60));
        assert_eq!(shuffle_interval(60), Duration::from_secs(60));
        // Custom-but-sane values are honored as-is.
        assert_eq!(shuffle_interval(7_200), Duration::from_secs(7_200));
    }
}
