// Copyright (C) 2026 Javad Rajabzadeh
// SPDX-License-Identifier: GPL-3.0-or-later

//! Human formatting: `121.66 MB`, `1.027 MB/sec`,
//! `3 min 32 sec`, `Aug 17 15:48:32 2026`.

use std::time::{Duration, SystemTime, UNIX_EPOCH};

use chrono::{DateTime, Local, TimeZone};
use hya_core::fmt::DurationUnits;

use crate::i18n::tr;

/// `121.66 MB` (two decimals — the download-list spelling).
pub fn size2(bytes: u64) -> String {
    size_n(bytes, 2)
}

/// `121.665 MB` (three decimals — the progress-dialog spelling).
pub fn size3(bytes: u64) -> String {
    size_n(bytes, 3)
}

fn size_n(bytes: u64, prec: usize) -> String {
    hya_core::fmt::bytes_fixed(bytes, prec)
}

/// `1.027 MB/sec`, `200.666 KB/sec`.
pub fn rate(bytes_per_sec: f64) -> String {
    hya_core::fmt::rate_fixed(bytes_per_sec, 3)
}

/// The transfer rate as the reading a capped transfer should show.
///
/// A capped transfer sits AT its cap, and the measurement jitters a percent or
/// two either side of it. Printing that measurement makes a steady, deliberately
/// limited transfer look unsteady — the figure flickers between 97 and 103
/// KB/sec while the user is looking at the 100 they typed. So the reading snaps
/// to the cap once the transfer is running at it, and falls back to the real
/// number when the transfer genuinely cannot reach the cap (a slow origin, a
/// congested link): a cap is a ceiling, not a promise, and hiding a shortfall
/// behind the requested figure would be the worse lie.
fn at_cap(bytes_per_sec: f64, cap: u64) -> f64 {
    let c = cap as f64;
    // Within a tenth of the cap counts as "at the cap". Wide enough to absorb
    // the smoothing window's ripple, narrow enough that a transfer running at
    // four fifths of what was asked still reports four fifths.
    if bytes_per_sec >= c * 0.9 {
        c
    } else {
        bytes_per_sec
    }
}

/// `100.000 KB/sec` — steady under a cap, honest below it. No suffix: for the
/// download list, whose Transfer rate column has no room for one.
pub fn rate_steady(bytes_per_sec: f64, cap: Option<u64>) -> String {
    match cap.filter(|c| *c > 0) {
        Some(c) => rate(at_cap(bytes_per_sec, c)),
        None => rate(bytes_per_sec),
    }
}

/// `100.000 KB/sec (Limited)` — as [`rate_steady`], saying why it is steady.
///
/// The suffix stays on even when the transfer is running below its cap, because
/// that is exactly when the user most needs to know a cap is in force: a slow
/// download with the Speed Limiter forgotten in the Options menu is otherwise
/// indistinguishable from a slow server.
pub fn rate_capped(bytes_per_sec: f64, cap: Option<u64>) -> String {
    let Some(cap) = cap.filter(|c| *c > 0) else {
        return rate(bytes_per_sec);
    };
    if bytes_per_sec < 1.0 {
        return String::new();
    }
    format!("{} {}", rate(at_cap(bytes_per_sec, cap)), tr("(Limited)"))
}

/// `500 KB/s`, `1.5 MB/s`, `Unlimited` — the Speed Limiter's spelling.
///
/// Separate from [`rate`] because this one labels a button rather than
/// reports a measurement: three decimals are noise on a figure the user
/// typed, and a toolbar label has no room for them.
pub fn limit(cap: Option<u64>) -> String {
    const KB: f64 = 1024.0;
    const MB: f64 = 1024.0 * 1024.0;
    let Some(b) = cap.filter(|c| *c > 0) else {
        return tr("Unlimited");
    };
    let b = b as f64;
    let (v, unit) = if b >= MB {
        (b / MB, "MB/s")
    } else {
        (b / KB, "KB/s")
    };
    if v.fract() < 0.05 {
        format!("{v:.0} {unit}")
    } else {
        format!("{v:.1} {unit}")
    }
}

/// `3 min 32 sec`, `1 hr 12 min`, `45 sec`, in the catalogue's unit labels.
pub fn eta(secs: u64) -> String {
    let (hour, minute, second) = (tr("hr"), tr("min"), tr("sec"));
    hya_core::fmt::duration_long_with(
        secs,
        DurationUnits {
            hour: &hour,
            minute: &minute,
            second: &second,
        },
    )
}

/// `5.80%` — the Status column while a transfer runs.
pub fn pct(done: u64, size: u64) -> String {
    if size == 0 {
        return String::new();
    }
    format!("{:.2}%", done as f64 * 100.0 / size as f64)
}

/// `Aug 17 15:48:32 2026` — the Last Try Date column.
pub fn date(unix: i64) -> String {
    match Local.timestamp_opt(unix, 0) {
        chrono::LocalResult::Single(t) => t.format("%b %d %H:%M:%S %Y").to_string(),
        _ => String::new(),
    }
}

/// Current unix time, seconds.
pub fn now_unix() -> i64 {
    since_epoch().as_secs() as i64
}

/// Wall-clock time since the unix epoch; zero if the clock is set before it.
pub fn since_epoch() -> Duration {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
}

/// `15:48` for scheduler time fields.
pub fn hhmm(t: &DateTime<Local>) -> String {
    t.format("%H:%M").to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sizes_rates_and_etas_keep_the_download_list_spelling() {
        assert_eq!(size2(127_571_657), "121.66 MB");
        assert_eq!(size3(127_571_657), "121.662 MB");
        assert_eq!(size2(512), "512 B");
        assert_eq!(size2(3 * 1024 * 1024 * 1024), "3.00 GB");
        assert_eq!(rate(1_076_887.0), "1.027 MB/sec");
        assert_eq!(rate(205_482.0), "200.666 KB/sec");
        assert_eq!(rate(0.5), "");
        assert_eq!(eta(45), "45 sec");
        assert_eq!(eta(212), "3 min 32 sec");
        assert_eq!(eta(4320), "1 hr 12 min");
    }

    #[test]
    fn now_unix_is_the_clock_in_seconds() {
        let secs = now_unix();
        assert_eq!(secs, since_epoch().as_secs() as i64);
        assert!(secs > 1_700_000_000);
    }

    /// A transfer sitting at its cap must read as the cap, not as the ripple
    /// around it — and a transfer that cannot reach the cap must read as itself.
    #[test]
    fn a_capped_rate_reads_steady_but_never_flatters() {
        let cap = Some(100 * 1024);
        // Measurement noise either side of the cap: all one figure.
        for measured in [97_000.0, 102_400.0, 105_000.0] {
            assert_eq!(rate_steady(measured, cap), "100.000 KB/sec");
        }
        // Genuinely slower than the cap: report what is really happening.
        assert_eq!(rate_steady(40.0 * 1024.0, cap), rate(40.0 * 1024.0));
        // No cap at all: unchanged from the plain reading.
        assert_eq!(rate_steady(1234.0, None), rate(1234.0));
        assert_eq!(rate_steady(1234.0, Some(0)), rate(1234.0));
    }

    #[test]
    fn the_limited_suffix_marks_a_cap_that_is_in_force() {
        let cap = Some(100 * 1024);
        assert_eq!(rate_capped(102_400.0, cap), "100.000 KB/sec (Limited)");
        // Below the cap the real figure is shown, still marked as capped.
        assert!(rate_capped(40.0 * 1024.0, cap).ends_with("(Limited)"));
        assert!(rate_capped(40.0 * 1024.0, cap).starts_with("40.000 KB/sec"));
        // An idle transfer has no rate to report, capped or not.
        assert!(rate_capped(0.0, cap).is_empty());
        assert_eq!(rate_capped(102_400.0, None), "100.000 KB/sec");
    }

    /// The Speed Limit button's label: a figure the user typed, spelled back
    /// the way they typed it, short enough to sit under a toolbar icon.
    #[test]
    fn a_cap_reads_back_as_the_round_number_it_was_set_to() {
        assert_eq!(limit(Some(500 * 1024)), "500 KB/s");
        assert_eq!(limit(Some(1024 * 1024)), "1 MB/s");
        assert_eq!(limit(Some(5 * 1024 * 1024)), "5 MB/s");
        // A cap between the two units keeps one decimal rather than rounding
        // to a figure the user never asked for.
        assert_eq!(limit(Some(1536 * 1024)), "1.5 MB/s");
        assert_eq!(limit(Some(1)), "0 KB/s");
        // No cap, and the degenerate zero one, both read as no limit.
        assert_eq!(limit(None), "Unlimited");
        assert_eq!(limit(Some(0)), "Unlimited");
    }
}
