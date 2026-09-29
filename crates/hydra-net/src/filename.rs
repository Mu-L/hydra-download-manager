// Copyright (C) 2026 Javad Rajabzadeh
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Turning a name from the network — a `Content-Disposition`, a URL path, a
//! page title, a browser capture — into a leaf every supported platform can
//! create.

/// `name` reduced to a leaf that is safe to write on Windows, macOS and Linux
/// alike, or `None` when nothing nameable is left.
///
/// Both path separators are cut, not just the platform's: a Windows path in a
/// header (`..\..\evil.exe`) reaches a Unix client as a string with no
/// separator in it, and a Windows client as a write outside the folder.
///
/// The Windows rules apply everywhere, not only on Windows. A name saved on
/// Linux is copied to an NTFS or exFAT drive later, and a list entry named
/// `a:b.mp4` that works on one machine is `os error 123` on the next.
pub fn portable(name: &str) -> Option<String> {
    let leaf = strip_invisibles(name.rsplit(['/', '\\']).next().unwrap_or(name));
    let replaced: String = leaf
        .chars()
        .map(|c| match c {
            '<' | '>' | ':' | '"' | '|' | '?' | '*' => '_',
            c => c,
        })
        .collect();
    // Windows drops trailing dots and spaces when it creates a file, so the
    // name on disk would no longer be the name the list opens.
    let trimmed = replaced.trim_start().trim_end_matches(['.', ' ']);
    if trimmed.is_empty() {
        return None;
    }
    let named = if is_reserved_device(trimmed) {
        format!("_{trimmed}")
    } else {
        trimmed.to_string()
    };
    Some(clamp_name(&named))
}

/// `CON`, `NUL`, `COM1` and the rest open the device instead of a file on
/// Windows, with or without an extension (`nul.txt` is the null device too).
fn is_reserved_device(name: &str) -> bool {
    let stem = name.split('.').next().unwrap_or(name).trim_end();
    let upper = stem.to_ascii_uppercase();
    match upper.as_str() {
        "CON" | "PRN" | "AUX" | "NUL" | "CONIN$" | "CONOUT$" => true,
        _ => upper
            .strip_prefix("COM")
            .or_else(|| upper.strip_prefix("LPT"))
            .is_some_and(|n| {
                matches!(n.as_bytes(), [d] if d.is_ascii_digit()) || matches!(n, "¹" | "²" | "³")
            }),
    }
}

/// The characters a filename is never made of, removed.
///
/// Controls (C0, DEL, C1) go because a newline or a NUL in a name is a
/// truncated write or a mangled log line, never a name.
///
/// The bidi formatting characters go for a sharper reason, and it is the one
/// that makes this matter for Persian, Arabic and Hebrew names specifically.
/// `U+202E` (RIGHT-TO-LEFT OVERRIDE) reverses the display of everything after
/// it, so a server can offer `عکس\u{202e}gpj.exe` and have every renderer —
/// the download list, the File Info dialog, the system file manager — draw it
/// as `عکس‏exe.jpg`, while what lands on disk and runs is an `.exe`. Nothing
/// in the header distinguishes that from a real RTL name, and no RTL name
/// needs it: the bidi algorithm takes direction from the letters themselves,
/// so Arabic script and Hebrew render right-to-left with no marks at all.
///
/// Two invisible characters are deliberately KEPT, because removing them
/// corrupts real names rather than protecting them: `U+200C` (ZERO WIDTH
/// NON-JOINER) is the نیم‌فاصله that Persian spelling depends on — `می‌روم`
/// is one word, `میروم` is a misspelling — and `U+200D` (ZERO WIDTH JOINER)
/// is what holds Indic conjuncts and multi-person emoji together. Neither
/// reorders anything, so neither can spoof an extension.
fn strip_invisibles(s: &str) -> String {
    s.chars()
        .filter(|&c| {
            !matches!(c,
                '\u{0}'..='\u{1f}'      // C0 controls
                | '\u{7f}'..='\u{9f}'   // DEL and the C1 controls
                | '\u{200b}'            // zero width space
                | '\u{200e}' | '\u{200f}' // LRM / RLM
                | '\u{202a}'..='\u{202e}' // embeddings and overrides
                | '\u{2060}'..='\u{2064}' // word joiner, invisible operators
                | '\u{2066}'..='\u{2069}' // isolates
                | '\u{feff}') // BOM, when a decode left one in front
        })
        .collect()
}

/// The longest name mainstream filesystems accept: 255 **bytes** on ext4,
/// APFS and exFAT; NTFS counts 255 UTF-16 units, which 255 UTF-8 bytes can
/// never exceed.
///
/// The unit is the whole point. A 200-character name is unremarkable in
/// English and unwritable in Persian, Korean or Chinese, where characters
/// cost two and three bytes each — so a limit counted in characters passes
/// every Latin test and fails with `ENAMETOOLONG` on exactly the names that
/// need this code to work.
const MAX_NAME_BYTES: usize = 255;

/// `name` shortened to fit [`MAX_NAME_BYTES`], cutting the stem and keeping
/// the extension.
///
/// The extension survives because it decides how the file opens and which
/// category it is filed under; a truncation that ate it would turn a long
/// Korean title into an extensionless blob. The cut lands on a character
/// boundary, because half a character is not one: slicing mid-way through a
/// three-byte `한` leaves bytes no filesystem stores and no UI draws.
fn clamp_name(name: &str) -> String {
    if name.len() <= MAX_NAME_BYTES {
        return name.to_string();
    }
    // A leading dot is not an extension separator, and a long tail is not an
    // extension — `report.نسخهٔ نهایی` keeps its whole name as the stem.
    let (stem, ext) = match name.rsplit_once('.') {
        Some((s, e)) if !s.is_empty() && e.len() <= 16 => (s, Some(e)),
        _ => (name, None),
    };
    let budget = MAX_NAME_BYTES - ext.map_or(0, |e| e.len() + 1);
    let mut cut = budget.min(stem.len());
    while cut > 0 && !stem.is_char_boundary(cut) {
        cut -= 1;
    }
    let stem = stem[..cut].trim_end();
    match ext {
        Some(e) => format!("{stem}.{e}"),
        None => stem.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::portable;

    #[test]
    fn a_video_title_with_a_colon_and_a_pipe_becomes_a_name_windows_accepts() {
        assert_eq!(
            portable("Q:ماینکرفت از هیچ (3) | شوکه شدیم!!.mp4").as_deref(),
            Some("Q_ماینکرفت از هیچ (3) _ شوکه شدیم!!.mp4")
        );
    }

    #[test]
    fn every_character_windows_refuses_is_replaced() {
        assert_eq!(
            portable(r#"a<b>c:d"e|f?g*h.zip"#).as_deref(),
            Some("a_b_c_d_e_f_g_h.zip")
        );
    }

    #[test]
    fn a_path_is_cut_to_its_leaf_whichever_separator_it_uses() {
        assert_eq!(portable("../../etc/passwd").as_deref(), Some("passwd"));
        assert_eq!(portable(r"..\..\evil.exe").as_deref(), Some("evil.exe"));
        assert_eq!(portable("C:evil.exe").as_deref(), Some("C_evil.exe"));
    }

    #[test]
    fn trailing_dots_and_spaces_go_because_windows_would_drop_them() {
        assert_eq!(portable("report. . ").as_deref(), Some("report"));
        assert_eq!(portable("  spaced.pdf").as_deref(), Some("spaced.pdf"));
    }

    #[test]
    fn a_name_that_is_only_dots_or_blanks_is_no_name() {
        for n in ["", " ", ".", "..", "...", "a/", "\u{202e}"] {
            assert_eq!(portable(n), None, "{n:?}");
        }
    }

    #[test]
    fn a_windows_device_name_is_prefixed_with_or_without_an_extension() {
        assert_eq!(portable("CON").as_deref(), Some("_CON"));
        assert_eq!(portable("nul.txt").as_deref(), Some("_nul.txt"));
        assert_eq!(portable("com1.tar.gz").as_deref(), Some("_com1.tar.gz"));
        assert_eq!(portable("LPT9").as_deref(), Some("_LPT9"));
        assert_eq!(portable("COM²").as_deref(), Some("_COM²"));
    }

    #[test]
    fn names_that_only_start_like_a_device_are_left_alone() {
        for n in [
            "console.log",
            "COM10",
            "LPT.txt",
            "auxiliary.pdf",
            "nullable",
            "COMپ.zip",
        ] {
            assert_eq!(portable(n).as_deref(), Some(n));
        }
    }

    #[test]
    fn names_in_every_script_are_kept_exactly_as_written() {
        for n in [
            "كتاب الرياضيات، الجزء الأول؟.pdf",
            "آموزش برنامه‌نویسی - قسمت ۳.mp4",
            "שיעור ראשון.docx",
            "无极 第二季 第01集.mkv",
            "進撃の巨人「最終章」.mp4",
            "설치프로그램 v2.0.exe",
            "गीत संग्रह.mp3",
            "Урок №5 — «Введение».pdf",
            "เพลงไทย.flac",
            "Ελληνικά.txt",
            "family 👨‍👩‍👧 trip.jpg",
            "ｆｕｌｌ：ｗｉｄｔｈ｜ｎａｍｅ.zip",
        ] {
            assert_eq!(portable(n).as_deref(), Some(n));
        }
    }

    #[test]
    fn only_the_forbidden_ascii_is_touched_inside_a_non_latin_name() {
        assert_eq!(
            portable("فصل ۲: مقدمه | نسخه نهایی?.pdf").as_deref(),
            Some("فصل ۲_ مقدمه _ نسخه نهایی_.pdf")
        );
        assert_eq!(
            portable("第1話：出会い*.mp4").as_deref(),
            Some("第1話：出会い_.mp4")
        );
    }

    #[test]
    fn persian_spelling_and_leading_dots_survive() {
        assert_eq!(portable("می‌روم.mp3").as_deref(), Some("می‌روم.mp3"));
        assert_eq!(portable(".bashrc").as_deref(), Some(".bashrc"));
    }

    #[test]
    fn a_long_name_is_cut_to_the_byte_limit_keeping_its_extension() {
        let name = format!("{}.mkv", "فیلم".repeat(100));
        let out = portable(&name).unwrap();
        assert!(out.len() <= 255, "{}", out.len());
        assert!(out.ends_with(".mkv"));
    }
}
