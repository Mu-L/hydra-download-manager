// Copyright (C) 2026 Javad Rajabzadeh
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Percent-decoding, shared by the transport's filename logic and the
//! front-ends' URL display. Malformed escapes are left as written.

/// Decode `%XX` escapes to bytes.
///
/// To bytes, not `char`s: one escape can spell a byte that is only part of a
/// multi-byte character, so the text can only be reassembled afterwards.
pub fn percent_decode_bytes(s: &str) -> Vec<u8> {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'%' && i + 2 < b.len() {
            if let (Some(h), Some(l)) = (hex_nibble(b[i + 1]), hex_nibble(b[i + 2])) {
                out.push((h << 4) | l);
                i += 3;
                continue;
            }
        }
        out.push(b[i]);
        i += 1;
    }
    out
}

/// Decode `%XX` escapes to text, replacing bytes that do not form UTF-8.
pub fn percent_decode(s: &str) -> String {
    match String::from_utf8(percent_decode_bytes(s)) {
        Ok(s) => s,
        Err(e) => String::from_utf8_lossy(e.as_bytes()).into_owned(),
    }
}

pub(crate) fn hex_nibble(c: u8) -> Option<u8> {
    match c {
        b'0'..=b'9' => Some(c - b'0'),
        b'a'..=b'f' => Some(c - b'a' + 10),
        b'A'..=b'F' => Some(c - b'A' + 10),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn escapes_decode_and_malformed_ones_are_left_alone() {
        assert_eq!(percent_decode("a%20b%2Fc"), "a b/c");
        assert_eq!(percent_decode("100%"), "100%");
        assert_eq!(percent_decode("%zz%4"), "%zz%4");
    }

    #[test]
    fn multi_byte_characters_are_reassembled_from_their_escapes() {
        assert_eq!(percent_decode("%C3%A9t%C3%A9"), "été");
        assert_eq!(percent_decode_bytes("%C3%A9"), [0xC3, 0xA9]);
        // A lone continuation byte is not a character; it is replaced, not dropped.
        assert_eq!(percent_decode("%A9x"), "\u{FFFD}x");
    }
}
