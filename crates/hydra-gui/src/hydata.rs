// Copyright (C) 2026 Javad Rajabzadeh
// SPDX-License-Identifier: GPL-3.0-or-later

//! `.hydata`: the settings as one file that moves between machines and
//! platforms (File > Export settings / Import settings).
//!
//! The file is [`MAGIC`], a format byte, then the configuration as TOML,
//! zlib-compressed. Paths under the home directory travel as `~/...` and land
//! under the importing user's home. Passwords, window geometry and trusted
//! extension origins belong to the machine that has them, so an export
//! leaves them out and an import keeps the local ones.

use std::io::{Read, Write};
use std::path::{Component, Path};

use flate2::read::ZlibDecoder;
use flate2::write::ZlibEncoder;
use flate2::Compression;

use crate::model::{self, ConfigFile};

pub const EXTENSION: &str = "hydata";

const MAGIC: &[u8; 6] = b"HYDATA";
const FORMAT: u8 = 1;
/// Far above any real configuration, and low enough that a crafted file
/// cannot inflate into an out-of-memory.
const MAX_CONFIG: u64 = 4 << 20;

pub fn encode(cfg: &ConfigFile, home: Option<&Path>) -> Result<Vec<u8>, String> {
    let mut out = cfg.clone();
    strip_local(&mut out);
    for_each_path(&mut out, |p| *p = to_portable(p, home));
    let text = toml::to_string_pretty(&out).map_err(|e| e.to_string())?;
    let mut header = MAGIC.to_vec();
    header.push(FORMAT);
    let mut z = ZlibEncoder::new(header, Compression::best());
    z.write_all(text.as_bytes()).map_err(|e| e.to_string())?;
    z.finish().map_err(|e| e.to_string())
}

/// The configuration in `bytes`, migrated and with its paths moved onto this
/// machine. Passwords are still missing; see [`keep_local`].
pub fn decode(bytes: &[u8], home: Option<&Path>) -> Result<ConfigFile, String> {
    let body = bytes
        .strip_prefix(MAGIC.as_slice())
        .ok_or("this is not a Hydra settings file")?;
    let (&format, packed) = body.split_first().ok_or("the file is truncated")?;
    if format > FORMAT {
        return Err("the file was written by a newer version of Hydra".into());
    }
    if format != FORMAT {
        return Err(format!("unknown settings format {format}"));
    }
    let mut text = String::new();
    ZlibDecoder::new(packed)
        .take(MAX_CONFIG + 1)
        .read_to_string(&mut text)
        .map_err(|e| format!("the file is damaged ({e})"))?;
    if text.len() as u64 > MAX_CONFIG {
        return Err("the file is too large to be Hydra settings".into());
    }
    let mut cfg: ConfigFile =
        toml::from_str(&text).map_err(|e| format!("the settings inside are unreadable ({e})"))?;
    for_each_path(&mut cfg, |p| *p = from_portable(p, home));
    let mut cfg = model::normalize_config(cfg);
    // What is left relative was absolute on another platform (`D:\Video` on
    // Linux): no folder here to point at, so fall back to the local default.
    for c in &mut cfg.categories {
        if !Path::new(&c.dir).is_absolute() {
            c.dir = model::default_category_dir(&c.name);
        }
    }
    for s in &mut cfg.settings.sounds {
        if !Path::new(&s.file).is_absolute() {
            s.file.clear();
        }
    }
    Ok(cfg)
}

/// Give `imported` back what [`encode`] left out, from `local`. A password is
/// kept only for the same site (or proxy) and user it was saved for.
pub fn keep_local(imported: &mut ConfigFile, local: &ConfigFile) {
    let (s, l) = (&mut imported.settings, &local.settings);
    s.window_size = l.window_size;
    s.window_pos = l.window_pos;
    s.trusted_extensions = l.trusted_extensions.clone();
    if s.proxy_pass.is_empty() && s.proxy_host == l.proxy_host && s.proxy_user == l.proxy_user {
        s.proxy_pass = l.proxy_pass.clone();
    }
    for login in s.logins.iter_mut().filter(|x| x.pass.is_empty()) {
        if let Some(known) = l
            .logins
            .iter()
            .find(|k| k.site == login.site && k.user == login.user)
        {
            login.pass = known.pass.clone();
        }
    }
}

fn strip_local(cfg: &mut ConfigFile) {
    let s = &mut cfg.settings;
    s.proxy_pass.clear();
    for login in &mut s.logins {
        login.pass.clear();
    }
    s.window_size = None;
    s.window_pos = None;
    s.trusted_extensions.clear();
}

fn for_each_path(cfg: &mut ConfigFile, mut f: impl FnMut(&mut String)) {
    for c in &mut cfg.categories {
        f(&mut c.dir);
    }
    for s in &mut cfg.settings.sounds {
        f(&mut s.file);
    }
    f(&mut cfg.settings.virus_scanner);
}

fn to_portable(path: &str, home: Option<&Path>) -> String {
    let Some(rest) = home.and_then(|h| Path::new(path).strip_prefix(h).ok()) else {
        return path.to_owned();
    };
    let mut out = String::from("~");
    for part in rest.components() {
        if let Component::Normal(p) = part {
            out.push('/');
            out.push_str(&p.to_string_lossy());
        }
    }
    out
}

fn from_portable(path: &str, home: Option<&Path>) -> String {
    let (Some(rest), Some(home)) = (path.strip_prefix('~'), home) else {
        return path.to_owned();
    };
    if !rest.is_empty() && !rest.starts_with('/') {
        return path.to_owned();
    }
    let mut out = home.to_path_buf();
    out.extend(
        rest.split('/')
            .filter(|p| !p.is_empty() && *p != "." && *p != ".."),
    );
    out.to_string_lossy().into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{SiteLogin, SoundRow};
    use std::path::PathBuf;

    fn home(user: &str) -> PathBuf {
        std::env::temp_dir().join("homes").join(user)
    }

    fn under(home: &Path, parts: &[&str]) -> String {
        let mut p = home.to_path_buf();
        p.extend(parts);
        p.to_string_lossy().into_owned()
    }

    fn configured() -> ConfigFile {
        let mut cfg = model::normalize_config(ConfigFile::default());
        cfg.language = Some("de".into());
        cfg.settings.default_conns = 12;
        cfg.settings.user_agent = "custom-agent".into();
        cfg
    }

    fn through_file(cfg: &ConfigFile, from: &Path, to: &Path) -> ConfigFile {
        decode(&encode(cfg, Some(from)).unwrap(), Some(to)).unwrap()
    }

    fn inflated(bytes: &[u8]) -> String {
        let mut text = String::new();
        ZlibDecoder::new(&bytes[MAGIC.len() + 1..])
            .read_to_string(&mut text)
            .unwrap();
        text
    }

    fn packed(format: u8, text: &[u8]) -> Vec<u8> {
        let mut header = MAGIC.to_vec();
        header.push(format);
        let mut z = ZlibEncoder::new(header, Compression::fast());
        z.write_all(text).unwrap();
        z.finish().unwrap()
    }

    #[test]
    fn settings_survive_the_round_trip() {
        let cfg = configured();
        let back = through_file(&cfg, &home("a"), &home("a"));
        assert_eq!(back.language.as_deref(), Some("de"));
        assert_eq!(back.settings.default_conns, 12);
        assert_eq!(back.settings.user_agent, "custom-agent");
        assert_eq!(
            back.categories.iter().map(|c| &c.name).collect::<Vec<_>>(),
            cfg.categories.iter().map(|c| &c.name).collect::<Vec<_>>()
        );
        assert_eq!(back.queues.len(), cfg.queues.len());
        assert_eq!(back.shortcuts, cfg.shortcuts);
    }

    #[test]
    fn the_file_is_compressed_behind_its_own_signature() {
        let cfg = configured();
        let bytes = encode(&cfg, None).unwrap();
        assert!(bytes.starts_with(b"HYDATA\x01"));
        assert!(bytes.len() < toml::to_string_pretty(&cfg).unwrap().len() / 2);
    }

    #[test]
    fn folders_under_home_move_to_the_importing_users_home() {
        let (a, b) = (home("alice"), home("bob"));
        let mut cfg = configured();
        cfg.categories[1].dir = under(&a, &["Downloads", "Models"]);
        cfg.settings.virus_scanner = under(&a, &["bin", "scan"]);
        cfg.settings.sounds = vec![SoundRow {
            event: "done".into(),
            enabled: true,
            file: under(&a, &["ding.wav"]),
        }];

        let text = inflated(&encode(&cfg, Some(&a)).unwrap());
        assert!(text.contains("\"~/Downloads/Models\""), "{text}");
        assert!(
            !text.contains("alice"),
            "the exporter's home leaked: {text}"
        );

        let back = through_file(&cfg, &a, &b);
        assert_eq!(back.categories[1].dir, under(&b, &["Downloads", "Models"]));
        assert_eq!(back.settings.virus_scanner, under(&b, &["bin", "scan"]));
        assert_eq!(back.settings.sounds[0].file, under(&b, &["ding.wav"]));
    }

    #[test]
    fn a_folder_from_another_platform_falls_back_to_the_local_default() {
        let foreign = if cfg!(windows) {
            "/mnt/data/Models"
        } else {
            r"D:\Data\Models"
        };
        let mut cfg = configured();
        let name = cfg.categories[1].name.clone();
        cfg.categories[1].dir = foreign.into();
        cfg.settings.sounds = vec![SoundRow {
            event: "done".into(),
            enabled: true,
            file: foreign.into(),
        }];
        let back = through_file(&cfg, &home("a"), &home("b"));
        assert_eq!(back.categories[1].dir, model::default_category_dir(&name));
        assert_eq!(back.settings.sounds[0].file, "", "the chime plays instead");
    }

    #[test]
    fn a_folder_outside_home_is_kept_as_it_is() {
        let outside = std::env::temp_dir()
            .join("shared")
            .to_string_lossy()
            .into_owned();
        let mut cfg = configured();
        cfg.categories[1].dir = outside.clone();
        assert_eq!(
            through_file(&cfg, &home("a"), &home("b")).categories[1].dir,
            outside
        );
    }

    #[test]
    fn passwords_and_window_geometry_stay_on_the_machine() {
        let mut cfg = configured();
        cfg.settings.proxy_host = "proxy.lan".into();
        cfg.settings.proxy_user = "me".into();
        cfg.settings.proxy_pass = "proxy-secret".into();
        cfg.settings.logins = vec![SiteLogin {
            site: "files.example".into(),
            user: "me".into(),
            pass: "site-secret".into(),
        }];
        cfg.settings.window_size = Some((1200.0, 700.0));

        let bytes = encode(&cfg, None).unwrap();
        let text = inflated(&bytes);
        assert!(!text.contains("secret"), "{text}");
        assert!(!text.contains("window_size"), "{text}");

        let mut local = configured();
        local.settings.window_size = Some((800.0, 500.0));
        local.settings.proxy_host = "proxy.lan".into();
        local.settings.proxy_user = "me".into();
        local.settings.proxy_pass = "local-proxy".into();
        local.settings.logins = vec![
            SiteLogin {
                site: "files.example".into(),
                user: "me".into(),
                pass: "local-site".into(),
            },
            SiteLogin {
                site: "other.example".into(),
                user: "me".into(),
                pass: "unrelated".into(),
            },
        ];
        let mut imported = decode(&bytes, None).unwrap();
        keep_local(&mut imported, &local);
        let s = &imported.settings;
        assert_eq!(s.window_size, Some((800.0, 500.0)));
        assert_eq!(s.proxy_pass, "local-proxy");
        assert_eq!(s.logins.len(), 1);
        assert_eq!(s.logins[0].pass, "local-site");
    }

    /// A trusted origin admits an extension to Hydra without the token, so
    /// no file from elsewhere may add one — only the local answer counts.
    #[test]
    fn trusted_extensions_neither_leave_nor_arrive_by_file() {
        let mut cfg = configured();
        cfg.settings.trusted_extensions = vec!["moz-extension://exported".into()];
        let text = inflated(&encode(&cfg, None).unwrap());
        assert!(!text.contains("moz-extension"), "{text}");

        let crafted = packed(
            FORMAT,
            b"[settings]\ntrusted_extensions = [\"chrome-extension://planted\"]\n",
        );
        let mut local = configured();
        local.settings.trusted_extensions = vec!["moz-extension://local".into()];
        let mut imported = decode(&crafted, None).unwrap();
        keep_local(&mut imported, &local);
        assert_eq!(
            imported.settings.trusted_extensions,
            ["moz-extension://local"]
        );
    }

    #[test]
    fn a_password_is_not_handed_to_a_different_user_or_proxy() {
        let mut cfg = configured();
        cfg.settings.proxy_host = "proxy.lan".into();
        cfg.settings.proxy_user = "new".into();
        cfg.settings.logins = vec![SiteLogin {
            site: "files.example".into(),
            user: "new".into(),
            pass: String::new(),
        }];
        let mut local = configured();
        local.settings.proxy_host = "proxy.lan".into();
        local.settings.proxy_user = "old".into();
        local.settings.proxy_pass = "old-proxy".into();
        local.settings.logins = vec![SiteLogin {
            site: "files.example".into(),
            user: "old".into(),
            pass: "old-site".into(),
        }];
        let mut imported = decode(&encode(&cfg, None).unwrap(), None).unwrap();
        keep_local(&mut imported, &local);
        assert_eq!(imported.settings.proxy_pass, "");
        assert_eq!(imported.settings.logins[0].pass, "");
    }

    #[test]
    fn an_older_config_is_migrated_on_import() {
        let bytes = packed(FORMAT, b"[settings]\ndark_mode = true\nfont_size = 13\n");
        let cfg = decode(&bytes, None).unwrap();
        assert_eq!(cfg.settings.theme_mode, Some(model::ThemeMode::Dark));
        assert_eq!(cfg.settings.ui_scale_pct, 100);
        assert!(!cfg.categories.is_empty());
        assert!(!cfg.queues.is_empty());
    }

    #[test]
    fn files_that_are_not_hydra_settings_are_refused() {
        let good = encode(&configured(), None).unwrap();
        let refused = [
            (
                b"url list\nhttps://a.b/x".to_vec(),
                "not a Hydra settings file",
            ),
            (b"HYDATA".to_vec(), "truncated"),
            (packed(FORMAT + 1, b""), "newer version"),
            (packed(0, b""), "unknown settings format"),
            (good[..good.len() / 2].to_vec(), "damaged"),
            (packed(FORMAT, b"settings = 5"), "unreadable"),
        ];
        for (bytes, why) in refused {
            let err = decode(&bytes, None).unwrap_err();
            assert!(err.contains(why), "expected {why:?}, got {err:?}");
        }
    }

    #[test]
    fn a_file_inflating_past_the_limit_is_refused() {
        let bomb = packed(FORMAT, &vec![b' '; MAX_CONFIG as usize + 1]);
        assert!(bomb.len() < 64 << 10);
        assert!(decode(&bomb, None).unwrap_err().contains("too large"));
    }

    #[test]
    fn home_itself_travels_as_a_bare_tilde() {
        let (a, b) = (home("a"), home("b"));
        let here = a.to_string_lossy();
        assert_eq!(to_portable(&here, Some(&a)), "~");
        assert_eq!(from_portable("~", Some(&b)), b.to_string_lossy());
        assert_eq!(from_portable("~user/x", Some(&b)), "~user/x");
        assert_eq!(from_portable("~/../../etc", Some(&b)), under(&b, &["etc"]));
    }
}
