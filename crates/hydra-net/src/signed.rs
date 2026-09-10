// Copyright (C) 2026 Javad Rajabzadeh
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! Expiry deadlines carried inside signed ("presigned") URLs.
//!
//! An object store hands out a URL that authorises one download for a bounded
//! window: S3 and everything that speaks its dialect — MinIO, Ceph, Qumulo,
//! Backblaze, DigitalOcean Spaces, Wasabi — sign it with SigV4, while CloudFront
//! and GCS use a bare `Expires` epoch. The window can be very short. A live
//! example measured against `s3q.ait.dtu.dk` allows ten seconds:
//!
//! ```text
//! ?X-Amz-Date=20260910T100706Z&X-Amz-Expires=10&X-Amz-Signature=...
//! ```
//!
//! The deadline applies to each request as it ARRIVES, not to the transfer: a
//! body that started inside the window streams to completion long after it, but
//! every request made after it is refused with `403 AccessDenied / Request has
//! expired`. That asymmetry is the whole reason this module exists — a caller
//! that knows the deadline can start now rather than park the URL behind a
//! dialog, which is the difference between a download and a 403.

/// The instant this URL stops being honoured, as a Unix timestamp.
///
/// `None` means no deadline was found, which is the answer for an ordinary URL
/// and also for a signing scheme not listed here — callers must read it as "no
/// reason to hurry", never as "this URL is permanent".
pub fn deadline(url: &str) -> Option<u64> {
    let query = url.split_once('?').map(|(_, q)| q)?;
    // SigV4 states an issue time and a duration; the pair is the deadline, and
    // neither half means anything alone. Checked first because a signed URL may
    // ALSO carry an unrelated `Expires` for cache control, and the signature's
    // own window is the one that refuses the request.
    let issued = param(query, "X-Amz-Date").and_then(|v| parse_basic_iso8601(&v));
    let window = param(query, "X-Amz-Expires").and_then(|v| v.trim().parse::<u64>().ok());
    if let (Some(t0), Some(dt)) = (issued, window) {
        return Some(t0.saturating_add(dt));
    }
    // CloudFront canned policies and GCS v2 sign a bare absolute epoch.
    param(query, "Expires")
        .and_then(|v| v.trim().parse::<u64>().ok())
        // A plausibility floor: `Expires=0` and `Expires=1` are how a cache-control
        // header spells "already stale", and reading one as a 1970 deadline would
        // mark every such URL permanently expired.
        .filter(|&t| t > 1_000_000_000)
}

/// How much life a signed URL must have left before it is worth relying on.
///
/// A policy, not a property of the URL. A link good for a week can be parked
/// behind a dialog and reused across a long transfer like any other; the
/// fifteen-minute and ten-second windows object stores actually hand out
/// cannot. Erring long is the safe direction — treating a durable URL as
/// perishable costs one redirect per request, while the reverse costs the
/// download.
pub const RENEW_HORIZON: u64 = 3600;

/// Whether this URL is a credential too short-lived to be reused: worth
/// re-deriving per request rather than resolving once and holding on to.
pub fn perishable(url: &str, now_unix: u64) -> bool {
    expires_within(url, now_unix, RENEW_HORIZON)
}

/// Whether this URL's window is short enough that a human confirmation step
/// would outlive it.
///
/// `within` is the caller's policy, not a property of the URL.
pub fn expires_within(url: &str, now_unix: u64, within: u64) -> bool {
    deadline(url).is_some_and(|t| t <= now_unix.saturating_add(within))
}

/// Case-insensitive lookup of one query parameter's raw value.
///
/// Case-insensitive because the parameter names are only conventionally
/// capitalised: SigV4 defines `X-Amz-Expires`, but generators in the wild emit
/// `x-amz-expires`, and a downloader that misses the deadline over a capital
/// letter fails in the least explicable way possible.
fn param(query: &str, name: &str) -> Option<String> {
    query.split('&').find_map(|kv| {
        let (k, v) = kv.split_once('=')?;
        k.eq_ignore_ascii_case(name).then(|| v.to_string())
    })
}

/// Parse SigV4's `YYYYMMDDTHHMMSSZ` into a Unix timestamp.
///
/// The basic (separator-free) ISO 8601 form, which is the only one SigV4 emits.
fn parse_basic_iso8601(s: &str) -> Option<u64> {
    let s = s.trim();
    let (date, time) = s.split_once('T')?;
    let time = time.strip_suffix('Z').unwrap_or(time);
    if date.len() != 8 || time.len() != 6 || !date.as_bytes().iter().all(u8::is_ascii_digit) {
        return None;
    }
    if !time.as_bytes().iter().all(u8::is_ascii_digit) {
        return None;
    }
    let num = |s: &str, a: usize, b: usize| s[a..b].parse::<u64>().ok();
    let (y, mo, d) = (num(date, 0, 4)?, num(date, 4, 6)?, num(date, 6, 8)?);
    let (h, mi, sec) = (num(time, 0, 2)?, num(time, 2, 4)?, num(time, 4, 6)?);
    if !(1..=12).contains(&mo) || !(1..=31).contains(&d) || h > 23 || mi > 59 || sec > 60 {
        return None;
    }
    Some(crate::polite::days_from_civil(y, mo, d) * 86_400 + h * 3600 + mi * 60 + sec)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The URL from the bug report, verbatim. Ten seconds is not a typo on the
    /// reporter's part — it is what the origin really issues, and it is why a
    /// capture that waits for a click can never succeed against this host.
    const DTU: &str = "https://s3q.ait.dtu.dk:9000/figshare/26003087/TEP_Mode1.h5\
?X-Amz-Algorithm=AWS4-HMAC-SHA256\
&X-Amz-Credential=00000005001330512eb1/20260910/oha/s3/aws4_request\
&X-Amz-Date=20260910T100706Z&X-Amz-Expires=10&X-Amz-SignedHeaders=host\
&X-Amz-Signature=3293a0340ab09bbae10302d048030682102b2dea731da6131c2100c7e047684e";

    #[test]
    fn sigv4_deadline_is_the_issue_time_plus_the_window() {
        // 2026-09-10T10:07:06Z is 1789034826; the URL allows ten more seconds.
        assert_eq!(parse_basic_iso8601("20260910T100706Z"), Some(1_789_034_826));
        assert_eq!(deadline(DTU), Some(1_789_034_836));
        assert!(expires_within(DTU, 1_789_034_830, 60));
        assert!(!expires_within(DTU, 1_789_030_000, 60));
    }

    #[test]
    fn epoch_dates_round_trip_against_the_http_date_parser() {
        // Two independent parsers of the same instant must agree, or one of them
        // is wrong about leap years and nothing downstream will notice.
        for (iso, imf) in [
            ("19700101T000000Z", "Thu, 01 Jan 1970 00:00:00 GMT"),
            ("20000229T123456Z", "Tue, 29 Feb 2000 12:34:56 GMT"),
            ("20240229T000000Z", "Thu, 29 Feb 2024 00:00:00 GMT"),
            ("20261231T235959Z", "Thu, 31 Dec 2026 23:59:59 GMT"),
        ] {
            assert_eq!(
                parse_basic_iso8601(iso),
                crate::polite::parse_http_date(imf),
                "{iso} vs {imf}"
            );
        }
    }

    #[test]
    fn a_bare_expires_epoch_is_a_deadline_and_a_cache_control_zero_is_not() {
        assert_eq!(
            deadline("https://d1.cloudfront.net/f.bin?Expires=1789034836&Signature=x"),
            Some(1_789_034_836)
        );
        // `Expires=0` is how a cache header spells "stale", not a 1970 deadline.
        assert_eq!(deadline("https://h/f?Expires=0"), None);
        assert_eq!(deadline("https://h/f?Expires=1"), None);
    }

    #[test]
    fn the_signature_window_wins_over_a_cache_expires_on_the_same_url() {
        // Both parameters present: the one that refuses the request is SigV4's.
        let u = "https://h/f?Expires=1789999999&X-Amz-Date=20260910T100706Z&X-Amz-Expires=10";
        assert_eq!(deadline(u), Some(1_789_034_836));
    }

    #[test]
    fn parameter_names_are_matched_without_regard_to_case() {
        let u = "https://h/f?x-amz-date=20260910T100706Z&x-amz-expires=10";
        assert_eq!(deadline(u), Some(1_789_034_836));
    }

    #[test]
    fn an_unsigned_url_has_no_deadline() {
        for u in [
            "https://example.com/f.bin",
            "https://example.com/f.bin?a=1&b=2",
            // Half a SigV4 pair says nothing: a date with no window, or a window
            // with no date, cannot be turned into an instant.
            "https://h/f?X-Amz-Date=20260910T100706Z",
            "https://h/f?X-Amz-Expires=10",
            // Malformed halves must not silently become epoch zero.
            "https://h/f?X-Amz-Date=not-a-date&X-Amz-Expires=10",
            "https://h/f?X-Amz-Date=20261301T100706Z&X-Amz-Expires=10",
            "https://h/f?X-Amz-Date=20260910T100706Z&X-Amz-Expires=soon",
            // Right shape, wrong lengths: a short date or a long time would
            // slice out of bounds if the guard were not there.
            "https://h/f?X-Amz-Date=2026091T100706Z&X-Amz-Expires=10",
            "https://h/f?X-Amz-Date=20260910T10070Z&X-Amz-Expires=10",
            // Right lengths, non-digits: the parse must refuse rather than
            // silently reading the digits it happens to find.
            "https://h/f?X-Amz-Date=2026o910T100706Z&X-Amz-Expires=10",
            "https://h/f?X-Amz-Date=20260910T1007o6Z&X-Amz-Expires=10",
            // Out-of-range components the range check exists for.
            "https://h/f?X-Amz-Date=20260910T250000Z&X-Amz-Expires=10",
            "https://h/f?X-Amz-Date=20260910T106000Z&X-Amz-Expires=10",
            "https://h/f?X-Amz-Date=20260932T100706Z&X-Amz-Expires=10",
        ] {
            assert_eq!(deadline(u), None, "{u}");
        }
        assert!(!expires_within(
            "https://example.com/f.bin",
            1_789_034_830,
            3600
        ));
    }
}
