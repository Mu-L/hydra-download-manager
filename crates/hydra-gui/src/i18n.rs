// Copyright (C) 2026 Javad Rajabzadeh
// SPDX-License-Identifier: GPL-3.0-or-later

//! Translation, gettext-style: the msgid IS the English string.
//!
//! `tr("Add URL")` returns the active locale's translation, falling back to
//! the English text itself — a missing key can never render as an identifier.
//!
//! Catalogues are flat JSON maps. Built-in languages ship inside the binary
//! from `assets/locale/<tag>.json` (`en.json` is the identity template
//! translators copy); users can add or override with
//! `<app_dir>/locales/<tag>.json` on disk — disk entries win over built-ins
//! for the same tag.
//!
//! Persian deliberately keeps the LTR layout for now (per product decision);
//! full RTL mirroring (Arabic and friends) needs iced-level layout flipping
//! and is tracked as follow-up work.

use std::collections::HashMap;
use std::sync::RwLock;

/// Languages compiled into the binary: (tag, native display name, catalogue).
static BUILTIN: &[(&str, &str, &str)] = &[
    ("ar", "العربية", include_str!("../assets/locale/ar.json")),
    ("cs", "Čeština", include_str!("../assets/locale/cs.json")),
    ("da", "Dansk", include_str!("../assets/locale/da.json")),
    ("de", "German", include_str!("../assets/locale/de.json")),
    ("el", "Ελληνικά", include_str!("../assets/locale/el.json")),
    ("en", "English", include_str!("../assets/locale/en.json")),
    ("es", "Español", include_str!("../assets/locale/es.json")),
    ("fa", "فارسی", include_str!("../assets/locale/fa.json")),
    ("fi", "Suomi", include_str!("../assets/locale/fi.json")),
    ("fr", "Français", include_str!("../assets/locale/fr.json")),
    ("he", "עברית", include_str!("../assets/locale/he.json")),
    ("hi", "हिन्दी", include_str!("../assets/locale/hi.json")),
    ("hu", "Magyar", include_str!("../assets/locale/hu.json")),
    (
        "id",
        "Bahasa Indonesia",
        include_str!("../assets/locale/id.json"),
    ),
    ("it", "Italiano", include_str!("../assets/locale/it.json")),
    ("ja", "日本語", include_str!("../assets/locale/ja.json")),
    ("ko", "한국어", include_str!("../assets/locale/ko.json")),
    ("nl", "Nederlands", include_str!("../assets/locale/nl.json")),
    ("pl", "Polski", include_str!("../assets/locale/pl.json")),
    ("pt", "Português", include_str!("../assets/locale/pt.json")),
    (
        "pt-BR",
        "Português (Brasil)",
        include_str!("../assets/locale/pt-BR.json"),
    ),
    ("ro", "Română", include_str!("../assets/locale/ro.json")),
    ("ru", "Русский", include_str!("../assets/locale/ru.json")),
    ("sv", "Svenska", include_str!("../assets/locale/sv.json")),
    ("th", "ไทย", include_str!("../assets/locale/th.json")),
    ("tr", "Türkçe", include_str!("../assets/locale/tr.json")),
    ("uk", "Українська", include_str!("../assets/locale/uk.json")),
    ("vi", "Tiếng Việt", include_str!("../assets/locale/vi.json")),
    ("zh", "简体中文", include_str!("../assets/locale/zh.json")),
    (
        "zh-Hant",
        "繁體中文",
        include_str!("../assets/locale/zh-hant.json"),
    ),
];

static CATALOGUE: RwLock<Option<HashMap<String, String>>> = RwLock::new(None);

/// Translate one UI string. English in, active locale out.
pub fn tr(msgid: &str) -> String {
    if let Ok(guard) = CATALOGUE.read() {
        if let Some(map) = guard.as_ref() {
            if let Some(t) = map.get(msgid) {
                return t.clone();
            }
        }
    }
    msgid.to_string()
}

/// Locale tags available: built-ins plus any `<app_dir>/locales/*.json`.
pub fn available() -> Vec<String> {
    let mut tags: Vec<String> = BUILTIN.iter().map(|(t, _, _)| t.to_string()).collect();
    let dir = crate::model::app_dir().join("locales");
    if let Ok(rd) = std::fs::read_dir(dir) {
        for entry in rd.flatten() {
            let name = entry.file_name().to_string_lossy().into_owned();
            if let Some(tag) = name.strip_suffix(".json") {
                if !tags.iter().any(|t| t == tag) {
                    tags.push(tag.to_string());
                }
            }
        }
    }
    tags
}

/// Native display name for a tag ("fa" → "فارسی").
pub fn display_name(tag: &str) -> String {
    BUILTIN
        .iter()
        .find(|(t, _, _)| *t == tag)
        .map(|(_, n, _)| n.to_string())
        .unwrap_or_else(|| tag.to_string())
}

/// Switch locale. `"en"` (or legacy `"English"`) clears back to the built-in
/// English base; unknown/broken catalogues fall back to English too.
pub fn set_locale(tag: &str) {
    let map = if tag == "en" || tag == "English" {
        None
    } else {
        let mut merged: HashMap<String, String> = BUILTIN
            .iter()
            .find(|(t, _, _)| *t == tag)
            .and_then(|(_, _, json)| serde_json::from_str(json).ok())
            .unwrap_or_default();
        if let Ok(bytes) = std::fs::read(
            crate::model::app_dir()
                .join("locales")
                .join(format!("{tag}.json")),
        ) {
            if let Ok(disk) = serde_json::from_slice::<HashMap<String, String>>(&bytes) {
                merged.extend(disk);
            }
        }
        (!merged.is_empty()).then_some(merged)
    };
    crate::log::debug(&format!("locale -> {tag}"));
    if let Ok(mut guard) = CATALOGUE.write() {
        *guard = map;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_builtin_locales_valid_and_complete() {
        let en_raw = include_str!("../assets/locale/en.json");
        let en_map: HashMap<String, String> =
            serde_json::from_str(en_raw).expect("en.json should be valid JSON");

        for &(tag, name, raw) in BUILTIN {
            assert!(!tag.is_empty(), "tag should not be empty");
            assert!(!name.is_empty(), "display name should not be empty");
            let map: Result<HashMap<String, String>, _> = serde_json::from_str(raw);
            assert!(
                map.is_ok(),
                "locale '{tag}' failed to parse as JSON: {:?}",
                map.err()
            );
            let map = map.unwrap();
            for key in en_map.keys() {
                assert!(
                    map.contains_key(key),
                    "locale '{tag}' is missing translation for key: '{key}'"
                );
            }
            // A translation may reorder `{placeholders}`; it may not invent or
            // lose one. Without this a dropped `{tried_rate}` reaches the UI as
            // literal braces, and nothing at build time would say so.
            for (key, en_val) in &en_map {
                let want = placeholders(en_val);
                if want.is_empty() {
                    continue;
                }
                let got = placeholders(&map[key]);
                assert_eq!(
                    want, got,
                    "locale '{tag}' changed the placeholders in '{key}'"
                );
            }
        }
    }

    /// Every `tr("...")` in the source has to be a key of `en.json`, or the
    /// string can never be translated — the fallback hides that until a
    /// translator asks why one line stays English.
    #[test]
    fn every_tr_literal_in_the_source_is_in_the_english_catalogue() {
        let en: HashMap<String, String> =
            serde_json::from_str(include_str!("../assets/locale/en.json")).expect("en.json");
        let src = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
        let mut missing = std::collections::BTreeSet::new();
        let mut seen = 0;
        for file in rust_files(&src) {
            let text = std::fs::read_to_string(&file).expect("readable source");
            for literal in tr_literals(&text) {
                seen += 1;
                if !en.contains_key(&literal) {
                    missing.insert(format!("{}: {literal:?}", file.display()));
                }
            }
        }
        assert!(seen > 300, "the scanner found only {seen} tr() literals");
        assert!(
            missing.is_empty(),
            "not in en.json:\n{}",
            missing.into_iter().collect::<Vec<_>>().join("\n")
        );
    }

    fn rust_files(dir: &std::path::Path) -> Vec<std::path::PathBuf> {
        let mut out = Vec::new();
        for entry in std::fs::read_dir(dir).expect("source dir").flatten() {
            let path = entry.path();
            if path.is_dir() {
                out.extend(rust_files(&path));
            } else if path.extension().is_some_and(|e| e == "rs") {
                out.push(path);
            }
        }
        out
    }

    /// The string literals passed straight to `tr(`, unescaped the way rustc
    /// reads them: `\"`, `\\`, and a backslash before a newline that
    /// swallows the newline and the indentation after it.
    fn tr_literals(text: &str) -> Vec<String> {
        let mut out = Vec::new();
        let mut rest = text;
        while let Some(at) = rest.find("tr(") {
            let before = rest[..at].chars().next_back();
            rest = &rest[at + 3..];
            if before.is_some_and(|c| c.is_alphanumeric() || c == '_' || c == '"') {
                continue;
            }
            let body = rest.trim_start();
            let Some(body) = body.strip_prefix('"') else {
                continue;
            };
            let mut lit = String::new();
            let mut chars = body.chars();
            loop {
                match chars.next() {
                    None | Some('"') => break,
                    Some('\\') => match chars.next() {
                        Some('n') => lit.push('\n'),
                        Some('t') => lit.push('\t'),
                        Some('\n') => {
                            // A continuation: the rest of the indentation goes too.
                            let tail = chars.as_str();
                            let trimmed = tail.trim_start();
                            chars = trimmed.chars();
                        }
                        Some(c) => lit.push(c),
                        None => break,
                    },
                    Some(c) => lit.push(c),
                }
            }
            out.push(lit);
        }
        out
    }

    /// Every `{name}` token in a string, as a set.
    fn placeholders(s: &str) -> std::collections::BTreeSet<&str> {
        let mut out = std::collections::BTreeSet::new();
        let mut rest = s;
        while let Some(open) = rest.find('{') {
            let Some(close) = rest[open..].find('}') else {
                break;
            };
            out.insert(&rest[open..open + close + 1]);
            rest = &rest[open + close + 1..];
        }
        out
    }
}
