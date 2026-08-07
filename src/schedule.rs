// Refresh scheduling math and the pure decisions of the fetch pipeline.
//
// The timing semantics mirror the reference extension exactly
// (`extension.js:396-407`, `_restartTimeoutFromLongDate`): the next refresh
// is due 86 400 s after Bing's `fullstartdate`; an out-of-range difference
// is *reset* to 60 s (not clamped), and a 5-minute fudge offset is added
// afterwards in case of an inaccurate local clock.

use std::path::Path;
use std::time::Duration;

use chrono::{DateTime, NaiveDateTime, Utc};

use crate::wallpaper;

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

/// How many images to request from Bing: retention-sized when retention is
/// 1–8 days (no point downloading 8 × ~5 MB just to prune most of them),
/// otherwise the API maximum of 8 (also for 0 = keep forever).
pub fn fetch_count(retention_days: u16) -> u8 {
    match retention_days {
        1..=8 => retention_days as u8,
        _ => 8,
    }
}

/// The "don't clobber" auto-apply rule: apply the freshly fetched image iff
/// (a) this is the very first successful fetch after a cold start (the
/// reason the user installed the applet), or (b) the currently applied
/// wallpaper is a file inside our download folder. If the user picked
/// another wallpaper in COSMIC Settings (or uses a color/per-output setup,
/// where `current_source` is `None`), the applet downloads but does not
/// apply until they act.
pub fn should_auto_apply(cold_start_first_fetch: bool, current_source: Option<&Path>) -> bool {
    cold_start_first_fetch || current_source.is_some_and(wallpaper::is_ours)
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
    fn fetch_count_follows_retention() {
        assert_eq!(fetch_count(1), 1);
        assert_eq!(fetch_count(3), 3);
        assert_eq!(fetch_count(8), 8);
        assert_eq!(fetch_count(30), 8); // capped at the API max
        assert_eq!(fetch_count(0), 8); // forever → full window
    }

    #[test]
    fn auto_apply_on_cold_start_first_fetch_regardless_of_current() {
        assert!(should_auto_apply(true, None));
        assert!(should_auto_apply(
            true,
            Some(Path::new("/usr/share/backgrounds/cosmic/x.jpg"))
        ));
    }

    #[test]
    fn auto_apply_when_current_is_ours() {
        let ours = wallpaper::download_dir().join("20260807-Foo_UHD.jpg");
        assert!(should_auto_apply(false, Some(&ours)));
    }

    #[test]
    fn no_auto_apply_when_current_is_not_ours() {
        // The user's own wallpaper must not be clobbered.
        assert!(!should_auto_apply(
            false,
            Some(Path::new("/usr/share/backgrounds/cosmic/x.jpg"))
        ));
        // Unknown current (color source, per-output mode, unreadable
        // config) is conservatively "not ours".
        assert!(!should_auto_apply(false, None));
        // The download dir itself (slideshow source) is not "our image".
        assert!(!should_auto_apply(false, Some(&wallpaper::download_dir())));
    }
}
