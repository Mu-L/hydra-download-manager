// Copyright (C) 2026 Javad Rajabzadeh
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Safari's `Cookies.binarycookies`.
//!
//! An undocumented but stable container: a `cook` magic, a big-endian table of
//! page sizes, then pages of little-endian cookie records that address their
//! own NUL-terminated strings by offset. Values are in the clear — Safari
//! protects the file with the filesystem (it lives inside the app's sandbox
//! container) rather than by encrypting it, so nothing has to be unwrapped.
//!
//! Endianness is mixed and that is not a mistake in the reader: the page table
//! is big-endian and everything inside a page is little-endian.
//!
//! Parsing is total — a truncated or foreign file yields the cookies that could
//! be read and no error. The file is not this program's to validate, and the
//! caller has a better failure to report than "byte 4718 was short": the
//! download that needed a session it did not get.

use super::{canonical_host, Cookie, CookieJar};

/// Seconds between the Mac absolute time reference (2001-01-01) and the Unix
/// epoch. Safari stores both timestamps in the former.
const MAC_EPOCH_OFFSET: f64 = 978_307_200.0;

/// Read every cookie in a `Cookies.binarycookies` body.
pub(crate) fn parse(raw: &[u8]) -> CookieJar {
    let mut jar = CookieJar::new();
    if !raw.starts_with(b"cook") || raw.len() < 8 {
        return jar;
    }
    let pages = be32(raw, 4) as usize;
    let table = 8;
    // A page count from a corrupt header must not make this allocate or spin:
    // the table itself has to fit in the file.
    if table + pages * 4 > raw.len() {
        return jar;
    }
    let mut at = table + pages * 4;
    for i in 0..pages {
        let size = be32(raw, table + i * 4) as usize;
        let Some(page) = raw.get(at..at + size) else {
            break;
        };
        read_page(page, &mut jar);
        at += size;
    }
    jar
}

fn read_page(page: &[u8], jar: &mut CookieJar) {
    if le32(page, 0) != 0x0001_0000 {
        return;
    }
    let count = le32(page, 4) as usize;
    if 8 + count * 4 > page.len() {
        return;
    }
    for i in 0..count {
        let at = le32(page, 8 + i * 4) as usize;
        if let Some(c) = read_cookie(page, at) {
            jar.insert(c);
        }
    }
}

fn read_cookie(page: &[u8], at: usize) -> Option<Cookie> {
    let size = le32(page, at) as usize;
    let rec = page.get(at..at + size)?;
    if rec.len() < 56 {
        return None;
    }
    let flags = le32(rec, 8);
    let domain = canonical_host(&cstr(rec, le32(rec, 16) as usize)?);
    let name = cstr(rec, le32(rec, 20) as usize)?;
    if domain.is_empty() || name.is_empty() {
        return None;
    }
    let expiry = le64f(rec, 40) + MAC_EPOCH_OFFSET;
    Some(Cookie {
        name,
        value: cstr(rec, le32(rec, 28) as usize).unwrap_or_default(),
        domain,
        // Safari keeps the same leading-dot convention as the interchange
        // format: a dot means the cookie carried a `Domain` attribute.
        host_only: !cstr(rec, le32(rec, 16) as usize)?.starts_with('.'),
        path: super::browser::path_or_root(&cstr(rec, le32(rec, 24) as usize).unwrap_or_default()),
        secure: flags & 0x1 != 0,
        http_only: flags & 0x4 != 0,
        expires: (expiry > 0.0 && expiry.is_finite()).then_some(expiry as u64),
    })
}

/// A NUL-terminated string at `at` within a record.
fn cstr(rec: &[u8], at: usize) -> Option<String> {
    let tail = rec.get(at..)?;
    let end = memchr::memchr(0, tail).unwrap_or(tail.len());
    Some(String::from_utf8_lossy(&tail[..end]).into_owned())
}

fn be32(b: &[u8], at: usize) -> u32 {
    match b.get(at..at + 4) {
        Some(s) => u32::from_be_bytes([s[0], s[1], s[2], s[3]]),
        None => 0,
    }
}

fn le32(b: &[u8], at: usize) -> u32 {
    match b.get(at..at + 4) {
        Some(s) => u32::from_le_bytes([s[0], s[1], s[2], s[3]]),
        None => 0,
    }
}

fn le64f(b: &[u8], at: usize) -> f64 {
    match b.get(at..at + 8) {
        Some(s) => f64::from_le_bytes(s.try_into().unwrap_or([0; 8])),
        None => 0.0,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// One cookie record, laid out the way Safari writes it.
    fn record(
        domain: &str,
        name: &str,
        path: &str,
        value: &str,
        flags: u32,
        expiry: f64,
    ) -> Vec<u8> {
        let strings = [domain, name, path, value];
        let mut offsets = [0u32; 4];
        let mut at = 56u32;
        for (i, s) in strings.iter().enumerate() {
            offsets[i] = at;
            at += s.len() as u32 + 1;
        }
        let mut r = Vec::new();
        r.extend_from_slice(&at.to_le_bytes()); // size
        r.extend_from_slice(&0u32.to_le_bytes());
        r.extend_from_slice(&flags.to_le_bytes());
        r.extend_from_slice(&0u32.to_le_bytes());
        for o in offsets {
            r.extend_from_slice(&o.to_le_bytes());
        }
        r.extend_from_slice(&0u64.to_le_bytes());
        r.extend_from_slice(&(expiry - MAC_EPOCH_OFFSET).to_le_bytes());
        r.extend_from_slice(&0f64.to_le_bytes());
        for s in strings {
            r.extend_from_slice(s.as_bytes());
            r.push(0);
        }
        r
    }

    fn file(records: Vec<Vec<u8>>) -> Vec<u8> {
        let mut page = Vec::new();
        page.extend_from_slice(&0x0001_0000u32.to_le_bytes());
        page.extend_from_slice(&(records.len() as u32).to_le_bytes());
        let mut at = 8 + records.len() * 4 + 4;
        for r in &records {
            page.extend_from_slice(&(at as u32).to_le_bytes());
            at += r.len();
        }
        page.extend_from_slice(&0u32.to_le_bytes());
        for r in &records {
            page.extend_from_slice(r);
        }

        let mut out = b"cook".to_vec();
        out.extend_from_slice(&1u32.to_be_bytes());
        out.extend_from_slice(&(page.len() as u32).to_be_bytes());
        out.extend_from_slice(&page);
        out
    }

    #[test]
    fn reads_domain_path_flags_and_expiry() {
        let raw = file(vec![
            record(
                ".example.org",
                "sid",
                "/",
                "abc",
                0x1 | 0x4,
                2_000_000_000.0,
            ),
            record("www.example.org", "csrf", "/files", "def", 0, 0.0),
        ]);
        let jar = parse(&raw);
        let c: Vec<&Cookie> = jar.iter().collect();
        assert_eq!(c.len(), 2);

        assert_eq!(c[0].domain, "example.org");
        assert!(!c[0].host_only);
        assert!(c[0].secure && c[0].http_only);
        assert_eq!(c[0].expires, Some(2_000_000_000));
        assert_eq!(c[0].value, "abc");

        assert_eq!(c[1].domain, "www.example.org");
        assert!(c[1].host_only);
        assert_eq!(c[1].path, "/files");
        assert!(c[1].is_session());
    }

    #[test]
    fn a_file_that_is_not_a_cookie_store_yields_nothing() {
        assert!(parse(b"not a cookie file at all").is_empty());
        assert!(parse(b"cook").is_empty());
    }

    #[test]
    fn a_truncated_file_keeps_the_pages_it_could_read() {
        let raw = file(vec![record(".example.org", "sid", "/", "abc", 0, 0.0)]);
        // Cut the last page short: the header still promises it.
        assert!(parse(&raw[..raw.len() - 10]).is_empty());
        // A page count larger than the file cannot make this read past the end.
        let mut lying = raw.clone();
        lying[4..8].copy_from_slice(&0xffff_ffffu32.to_be_bytes());
        assert!(parse(&lying).is_empty());
    }
}
