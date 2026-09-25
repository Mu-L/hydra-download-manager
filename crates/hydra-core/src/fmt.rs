// Copyright (C) 2026 Javad Rajabzadeh
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Human-readable sizes, rates and durations, in the spellings the front-ends
//! share: `1.50 KiB` / `12.3 MiB` for the terminal, `121.66 MB` and
//! `1.027 MB/sec` for the download list, `3m02s` and `3 min 2 sec` for an ETA.

const IEC: [&str; 7] = ["B", "KiB", "MiB", "GiB", "TiB", "PiB", "EiB"];

/// `512 B`, `1.50 KiB`, `12.3 MiB`: two decimals below ten, one above.
///
/// The ladder runs to EiB so no input can widen an aligned column: `u64::MAX`
/// in TiB would be fifteen characters.
pub fn bytes(n: u64) -> String {
    let mut v = n as f64;
    let mut i = 0;
    while v >= 1024.0 && i < IEC.len() - 1 {
        v /= 1024.0;
        i += 1;
    }
    if i == 0 {
        format!("{n} B")
    } else if v < 10.0 {
        format!("{v:.2} {}", IEC[i])
    } else {
        format!("{v:.1} {}", IEC[i])
    }
}

/// `121.66 MB` with a fixed number of decimals: 1024-based, labelled `KB`,
/// `MB`, `GB`, and stopping at `GB`. Below one KiB the count is printed as is.
pub fn bytes_fixed(n: u64, decimals: usize) -> String {
    const KB: f64 = 1024.0;
    const MB: f64 = KB * 1024.0;
    const GB: f64 = MB * 1024.0;
    let b = n as f64;
    if b >= GB {
        format!("{:.decimals$} GB", b / GB)
    } else if b >= MB {
        format!("{:.decimals$} MB", b / MB)
    } else if b >= KB {
        format!("{:.decimals$} KB", b / KB)
    } else {
        format!("{n} B")
    }
}

/// `1.50 KiB/s`: [`bytes`] with a per-second suffix.
pub fn rate(bytes_per_s: f64) -> String {
    format!("{}/s", bytes(bytes_per_s.max(0.0) as u64))
}

/// `1.027 MB/sec`, `200.666 KB/sec`: 1024-based, `KB` up to one MiB and `MB`
/// above it. Empty below one byte per second, so an idle transfer shows no
/// figure at all.
pub fn rate_fixed(bytes_per_s: f64, decimals: usize) -> String {
    const KB: f64 = 1024.0;
    const MB: f64 = KB * 1024.0;
    if bytes_per_s >= MB {
        format!("{:.decimals$} MB/sec", bytes_per_s / MB)
    } else if bytes_per_s >= 1.0 {
        format!("{:.decimals$} KB/sec", bytes_per_s / KB)
    } else {
        String::new()
    }
}

/// `45.0s`, `3m02s`, `1h12m`; `?` when `secs` is not finite, which is how a
/// caller says it has no estimate yet.
pub fn duration(secs: f64) -> String {
    if !secs.is_finite() {
        return "?".into();
    }
    let s = secs.max(0.0);
    if s < 60.0 {
        format!("{s:.1}s")
    } else if s < 3600.0 {
        format!("{}m{:02}s", (s / 60.0) as u64, (s % 60.0) as u64)
    } else {
        format!(
            "{}h{:02}m",
            (s / 3600.0) as u64,
            ((s % 3600.0) / 60.0) as u64
        )
    }
}

/// The unit labels [`duration_long_with`] spells a duration in.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DurationUnits<'a> {
    pub hour: &'a str,
    pub minute: &'a str,
    pub second: &'a str,
}

impl DurationUnits<'static> {
    /// `hr`, `min`, `sec`.
    pub const ENGLISH: Self = DurationUnits {
        hour: "hr",
        minute: "min",
        second: "sec",
    };
}

/// `45 sec`, `3 min 2 sec`, `1 hr 12 min`.
pub fn duration_long(secs: u64) -> String {
    duration_long_with(secs, DurationUnits::ENGLISH)
}

/// [`duration_long`] with the caller's own unit labels, for a localised UI.
///
/// Two units at most, the larger first: seconds are dropped once the estimate
/// is an hour or more.
pub fn duration_long_with(secs: u64, units: DurationUnits<'_>) -> String {
    if secs >= 3600 {
        format!(
            "{} {} {} {}",
            secs / 3600,
            units.hour,
            (secs % 3600) / 60,
            units.minute
        )
    } else if secs >= 60 {
        format!(
            "{} {} {} {}",
            secs / 60,
            units.minute,
            secs % 60,
            units.second
        )
    } else {
        format!("{secs} {}", units.second)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const MIB: u64 = 1024 * 1024;
    const GIB: u64 = MIB * 1024;

    #[test]
    fn iec_bytes_across_every_boundary() {
        for (n, want) in [
            (0, "0 B"),
            (1, "1 B"),
            (1023, "1023 B"),
            (1024, "1.00 KiB"),
            (1536, "1.50 KiB"),
            (10 * 1024, "10.0 KiB"),
            (MIB - 1, "1024.0 KiB"),
            (MIB, "1.00 MiB"),
            (GIB * 3 / 2, "1.50 GiB"),
            (GIB * 1024 * 12, "12.0 TiB"),
            (u64::MAX, "16.0 EiB"),
        ] {
            assert_eq!(bytes(n), want, "{n}");
        }
    }

    #[test]
    fn fixed_bytes_use_kb_mb_gb_labels_and_stop_at_gb() {
        for (n, decimals, want) in [
            (0, 2, "0 B"),
            (1023, 2, "1023 B"),
            (1024, 2, "1.00 KB"),
            (MIB - 1, 2, "1024.00 KB"),
            (MIB, 3, "1.000 MB"),
            (127_565_824, 2, "121.66 MB"),
            (GIB, 2, "1.00 GB"),
            (u64::MAX, 1, "17179869184.0 GB"),
        ] {
            assert_eq!(bytes_fixed(n, decimals), want, "{n}");
        }
    }

    #[test]
    fn rates_carry_their_suffix() {
        assert_eq!(rate(0.0), "0 B/s");
        assert_eq!(rate(-5.0), "0 B/s");
        assert_eq!(rate(1536.0), "1.50 KiB/s");
        assert_eq!(rate(12.0 * MIB as f64), "12.0 MiB/s");
    }

    #[test]
    fn fixed_rates_switch_unit_at_one_mib_and_hide_an_idle_transfer() {
        assert_eq!(rate_fixed(0.0, 3), "");
        assert_eq!(rate_fixed(0.999, 3), "");
        assert_eq!(rate_fixed(1.0, 3), "0.001 KB/sec");
        assert_eq!(rate_fixed(205_482.0, 3), "200.666 KB/sec");
        assert_eq!(rate_fixed(MIB as f64 - 1.0, 3), "1023.999 KB/sec");
        assert_eq!(rate_fixed(1_076_887.0, 3), "1.027 MB/sec");
    }

    #[test]
    fn short_durations_keep_two_fields() {
        for (secs, want) in [
            (0.0, "0.0s"),
            (-3.0, "0.0s"),
            (45.25, "45.2s"),
            (59.99, "60.0s"),
            (60.0, "1m00s"),
            (182.0, "3m02s"),
            (3599.0, "59m59s"),
            (3600.0, "1h00m"),
            (4320.0, "1h12m"),
            (f64::NAN, "?"),
            (f64::INFINITY, "?"),
        ] {
            assert_eq!(duration(secs), want, "{secs}");
        }
    }

    #[test]
    fn long_durations_spell_their_units() {
        for (secs, want) in [
            (0, "0 sec"),
            (45, "45 sec"),
            (60, "1 min 0 sec"),
            (182, "3 min 2 sec"),
            (3599, "59 min 59 sec"),
            (3600, "1 hr 0 min"),
            (4320, "1 hr 12 min"),
            (u64::MAX, "5124095576030431 hr 0 min"),
        ] {
            assert_eq!(duration_long(secs), want, "{secs}");
        }
    }

    #[test]
    fn a_localised_ui_supplies_its_own_labels() {
        let units = DurationUnits {
            hour: "Std",
            minute: "Min",
            second: "Sek",
        };
        assert_eq!(duration_long_with(4320, units), "1 Std 12 Min");
        assert_eq!(duration_long_with(182, units), "3 Min 2 Sek");
        assert_eq!(duration_long_with(7, units), "7 Sek");
    }
}
