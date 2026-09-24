// Copyright (C) 2026 Javad Rajabzadeh
// SPDX-License-Identifier: GPL-3.0-or-later

//! Leveled session log in `<app_dir>/logs/gui.log`.
//!
//! `[2026-08-18 06:36:12] [INFO ] start #3 https://…` — level filter comes
//! from `log_level` in config.toml (`debug`/`info`/`warn`/`error`, default
//! `info`) or the `HYDRA_LOG` environment variable, which wins. Failure to
//! log must never fail the operation being logged.
//!
//! The file rolls daily: the first write of a day moves an older `gui.log`
//! aside as `gui-YYYY-MM-DD.log`, named for the day it was last written, and
//! only [`KEEP_DAYS`] days of log are kept on disk.

use std::io::Write;
use std::path::Path;
use std::sync::atomic::{AtomicU8, Ordering};
use std::sync::{Mutex, PoisonError};

use chrono::NaiveDate;

/// Days of log on disk, counting today's `gui.log`.
const KEEP_DAYS: usize = 3;
const FILE_NAME: &str = "gui.log";
const ARCHIVE_DATE: &str = "gui-%Y-%m-%d.log";

#[derive(Clone, Copy, PartialEq, Eq, PartialOrd)]
pub enum Level {
    Debug = 0,
    Info = 1,
    Warn = 2,
    Error = 3,
}

static FILTER: AtomicU8 = AtomicU8::new(Level::Info as u8);

pub fn set_level(name: &str) {
    let lvl = match name.to_ascii_lowercase().as_str() {
        "debug" | "trace" | "verbose" => Level::Debug,
        "warn" | "warning" => Level::Warn,
        "error" => Level::Error,
        _ => Level::Info,
    };
    FILTER.store(lvl as u8, Ordering::Relaxed);
}

/// Apply config level, then let HYDRA_LOG override it for one-off debugging.
pub fn init(config_level: Option<&str>) {
    if let Some(l) = config_level {
        set_level(l);
    }
    if let Ok(env) = std::env::var("HYDRA_LOG") {
        set_level(&env);
    }
}

/// Where the session log lives. Exposed so Help > Logs can open exactly the
/// file this module writes, rather than a path spelled twice.
pub fn path() -> std::path::PathBuf {
    crate::model::app_dir().join("logs").join(FILE_NAME)
}

fn write(level: Level, tag: &str, line: &str) {
    if (level as u8) < FILTER.load(Ordering::Relaxed) {
        return;
    }
    let now = chrono::Local::now();
    let ts = now.format("%Y-%m-%d %H:%M:%S%.3f");
    // One formatted string, one write_all: `writeln!` with arguments emits a
    // write per fragment, and the extbus socket threads log concurrently
    // with the UI thread — that interleaves mid-line.
    let record = format!("[{ts}] [{tag}] {line}\n");
    // The day the file was last rolled for. Held across the append too, so no
    // thread writes into a file another one is moving aside.
    static ROLLED: Mutex<Option<NaiveDate>> = Mutex::new(None);
    let mut rolled = ROLLED.lock().unwrap_or_else(PoisonError::into_inner);
    append(&path(), now.date_naive(), &mut rolled, &record);
}

fn append(file: &Path, today: NaiveDate, rolled: &mut Option<NaiveDate>, record: &str) {
    if *rolled != Some(today) {
        if let Some(dir) = file.parent() {
            let _ = std::fs::create_dir_all(dir);
            roll(dir, today);
        }
        *rolled = Some(today);
    }
    if let Ok(mut f) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(file)
    {
        let _ = f.write_all(record.as_bytes());
    }
}

/// Move a `gui.log` last written before `today` aside under that day, then
/// delete the oldest archives past [`KEEP_DAYS`]. Only files named exactly
/// like an archive are ever deleted.
fn roll(dir: &Path, today: NaiveDate) {
    let live = dir.join(FILE_NAME);
    if let Some(day) = modified_day(&live).filter(|day| *day < today) {
        let _ = std::fs::rename(&live, dir.join(day.format(ARCHIVE_DATE).to_string()));
    }
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    let mut archives: Vec<(NaiveDate, std::path::PathBuf)> = entries
        .flatten()
        .filter_map(|e| {
            let name = e.file_name();
            let day = NaiveDate::parse_from_str(name.to_str()?, ARCHIVE_DATE).ok()?;
            Some((day, e.path()))
        })
        .collect();
    archives.sort_unstable_by_key(|(day, _)| std::cmp::Reverse(*day));
    for (_, stale) in archives.iter().skip(KEEP_DAYS - 1) {
        let _ = std::fs::remove_file(stale);
    }
}

fn modified_day(file: &Path) -> Option<NaiveDate> {
    let modified = std::fs::metadata(file).ok()?.modified().ok()?;
    Some(chrono::DateTime::<chrono::Local>::from(modified).date_naive())
}

pub fn debug(line: &str) {
    write(Level::Debug, "DEBUG", line);
}

pub fn info(line: &str) {
    write(Level::Info, "INFO ", line);
}

pub fn warn(line: &str) {
    write(Level::Warn, "WARN ", line);
}

pub fn error(line: &str) {
    write(Level::Error, "ERROR", line);
}

/// Route panics into this log.
///
/// A release build is `windows_subsystem = "windows"` and is launched by the
/// browser's native-messaging host with its stdio on the null device, so a
/// panic message has nowhere to go: the app simply vanishes, and the bug
/// report reads "it crashed" with an empty log box. The default hook still
/// runs afterwards for the cases where a console does exist.
pub fn catch_panics() {
    let previous = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let where_ = info
            .location()
            .map(|l| format!("{}:{}", l.file(), l.line()))
            .unwrap_or_else(|| "unknown location".into());
        let payload = extract_panic_payload(info.payload());
        error(&format!(
            "panic at {where_}: {payload}\n{}",
            std::backtrace::Backtrace::force_capture()
        ));
        previous(info);
    }));
}

pub(crate) fn extract_panic_payload(payload: &(dyn std::any::Any + Send)) -> &str {
    if let Some(&s) = payload.downcast_ref::<&str>() {
        s
    } else if let Some(s) = payload.downcast_ref::<String>() {
        s.as_str()
    } else {
        "<non-string payload>"
    }
}

/// Back-compat alias for the original single-level call sites.
pub fn log(line: &str) {
    info(line);
}

/// The machine, once, at launch.
///
/// Every stream bug report starts with the same three round trips — which
/// build, which OS, was ffmpeg there — so the log answers them before they
/// are asked. It is deliberately ONE line plus the paths: a banner that
/// scrolls is a banner nobody reads.
///
/// Nothing here shells out. A launch path should not wait on a subprocess,
/// and every field is either a compile-time constant or a small file read.
pub fn banner() {
    let cpus = std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(0);
    info(&format!(
        "hydra-gui {} | {} {} | {} | {} core(s) | ffmpeg: {}",
        env!("CARGO_PKG_VERSION"),
        std::env::consts::OS,
        std::env::consts::ARCH,
        os_release().unwrap_or_else(|| "unknown release".into()),
        cpus,
        match hya_stream::hls::ffmpeg() {
            Some(p) => p.display().to_string(),
            // The single most common cause of "why is my HLS download a .ts
            // file" — worth stating at launch rather than per download.
            None => "not found".into(),
        },
    ));
    info(&format!("log: {}", path().display()));
    info(&format!("data: {}", crate::model::app_dir().display()));
}

/// Best-effort human-readable OS version.
#[cfg(target_os = "macos")]
fn os_release() -> Option<String> {
    let s = std::fs::read_to_string("/System/Library/CoreServices/SystemVersion.plist").ok()?;
    let tail = &s[s.find("<key>ProductVersion</key>")?..];
    let a = tail.find("<string>")? + "<string>".len();
    let b = tail[a..].find("</string>")?;
    Some(format!("macOS {}", &tail[a..a + b]))
}

/// `PRETTY_NAME` is the line every distribution agrees on, and the one that
/// distinguishes the snap-confined browsers case from the ordinary one.
#[cfg(target_os = "linux")]
fn os_release() -> Option<String> {
    let s = std::fs::read_to_string("/etc/os-release").ok()?;
    s.lines()
        .find_map(|l| l.strip_prefix("PRETTY_NAME="))
        .map(|v| v.trim_matches('"').to_string())
}

#[cfg(target_os = "windows")]
fn os_release() -> Option<String> {
    // No subprocess and no registry crate: the environment already
    // distinguishes the NT line, and the build number is not worth a
    // dependency here.
    std::env::var("OS").ok()
}

#[cfg(not(any(target_os = "macos", target_os = "linux", target_os = "windows")))]
fn os_release() -> Option<String> {
    None
}

/// A URL safe to write into a file the user is invited to hand to someone.
///
/// Signed CDN URLs carry BEARER CREDENTIALS in the query string —
/// CloudFront's `Signature` + `Key-Pair-Id`, Akamai's `hdnts`, the `token`
/// on a hundred smaller CDNs. They are exactly the URLs the browser
/// extension hands over, and exactly the URLs an HLS or DASH manifest is
/// full of, so this log would otherwise collect them by the hundred.
///
/// That is a leak because Help > Logs exists: this file is meant to be
/// opened and pasted into a bug report. Whoever reads that report gets
/// working credentials for the user's session, still live if the signature
/// has not expired.
///
/// The path is what makes a log useful for diagnosis; the query is what
/// makes it dangerous. So the query goes, and its length stays — enough to
/// tell "no query" from "query elided" when reading a trace back.
pub fn redact(url: &str) -> String {
    match url.split_once('?') {
        Some((head, q)) => format!("{head}?<{} chars elided>", q.len()),
        None => url.to_string(),
    }
}

#[cfg(test)]
mod tests {
    // `os_release` is only reached by the test below it, and that test only
    // runs where the release is discoverable.
    #[cfg(any(target_os = "macos", target_os = "linux"))]
    use super::os_release;
    use super::{append, redact, roll, ARCHIVE_DATE, FILE_NAME};
    use chrono::NaiveDate;
    use std::path::Path;

    /// The banner's whole value is being specific, so a platform that
    /// silently reports "unknown release" is worth catching here rather
    /// than in a bug report that needed it.
    #[test]
    #[cfg(any(target_os = "macos", target_os = "linux"))]
    fn the_os_version_is_actually_discoverable_here() {
        let r = os_release().expect("this platform should report a release");
        assert!(!r.trim().is_empty(), "empty release string");
    }

    #[test]
    fn a_signed_url_loses_its_credentials_but_stays_diagnosable() {
        let signed = "https://cdn.example/hls/seg1.ts\
                      ?Expires=1750000000&Signature=abc123DEF&Key-Pair-Id=APKAIOSFODNN7";
        let out = redact(signed);

        // The credential is gone, in every part.
        assert!(!out.contains("abc123DEF"), "signature survived: {out}");
        assert!(!out.contains("APKAIOSFODNN7"), "key id survived: {out}");
        assert!(!out.contains("Expires"), "query survived: {out}");

        // What is left still says which segment, on which host.
        assert!(out.starts_with("https://cdn.example/hls/seg1.ts?"));
        assert!(out.contains("elided"));
    }

    #[test]
    fn a_plain_url_is_left_alone() {
        let plain = "https://cdn.example/hls/seg1.ts";
        assert_eq!(redact(plain), plain);
    }

    fn scratch(name: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("hydra-log-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn day(s: &str) -> NaiveDate {
        NaiveDate::parse_from_str(s, "%Y-%m-%d").unwrap()
    }

    fn written_on(file: &Path, on: NaiveDate) {
        let noon = on.and_hms_opt(12, 0, 0).unwrap();
        let at = noon.and_local_timezone(chrono::Local).unwrap();
        let f = std::fs::File::options().write(true).open(file).unwrap();
        f.set_modified(at.into()).unwrap();
    }

    fn names(dir: &Path) -> Vec<String> {
        let mut v: Vec<String> = std::fs::read_dir(dir)
            .unwrap()
            .map(|e| e.unwrap().file_name().into_string().unwrap())
            .collect();
        v.sort();
        v
    }

    #[test]
    fn a_log_from_an_earlier_day_is_moved_aside_under_the_day_it_was_written() {
        let dir = scratch("earlier");
        let live = dir.join(FILE_NAME);
        std::fs::write(&live, "last week\n").unwrap();
        written_on(&live, day("2026-09-17"));

        roll(&dir, day("2026-09-24"));

        assert_eq!(names(&dir), ["gui-2026-09-17.log"]);
        let archived = std::fs::read_to_string(dir.join("gui-2026-09-17.log")).unwrap();
        assert_eq!(archived, "last week\n");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn todays_log_stays_where_help_logs_opens_it() {
        let dir = scratch("today");
        let live = dir.join(FILE_NAME);
        std::fs::write(&live, "this morning\n").unwrap();
        written_on(&live, day("2026-09-24"));

        roll(&dir, day("2026-09-24"));

        assert_eq!(names(&dir), [FILE_NAME]);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn only_the_last_three_days_are_kept_and_nothing_else_is_deleted() {
        let dir = scratch("prune");
        for d in ["2026-09-18", "2026-09-20", "2026-09-21", "2026-09-22"] {
            std::fs::write(dir.join(format!("gui-{d}.log")), d).unwrap();
        }
        let live = dir.join(FILE_NAME);
        std::fs::write(&live, "yesterday\n").unwrap();
        written_on(&live, day("2026-09-23"));
        // Not archives, whatever they look like.
        for other in ["gui-notes.log", "gui-2026-09-01.log.bak", "host.log"] {
            std::fs::write(dir.join(other), "keep").unwrap();
        }

        roll(&dir, day("2026-09-24"));

        assert_eq!(
            names(&dir),
            [
                "gui-2026-09-01.log.bak",
                "gui-2026-09-22.log",
                "gui-2026-09-23.log",
                "gui-notes.log",
                "host.log",
            ]
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_first_write_after_midnight_starts_a_new_file() {
        let dir = scratch("midnight");
        let live = dir.join(FILE_NAME);
        let today = chrono::Local::now().date_naive();
        let tomorrow = today.succ_opt().unwrap();
        let mut rolled = None;

        append(&live, today, &mut rolled, "one\n");
        append(&live, today, &mut rolled, "two\n");
        append(&live, tomorrow, &mut rolled, "three\n");

        let archive = dir.join(today.format(ARCHIVE_DATE).to_string());
        assert_eq!(std::fs::read_to_string(archive).unwrap(), "one\ntwo\n");
        assert_eq!(std::fs::read_to_string(&live).unwrap(), "three\n");
        assert_eq!(rolled, Some(tomorrow));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_missing_logs_folder_is_created_on_the_first_write() {
        let dir = scratch("fresh").join("logs");
        let live = dir.join(FILE_NAME);

        append(&live, day("2026-09-24"), &mut None, "first\n");

        assert_eq!(std::fs::read_to_string(&live).unwrap(), "first\n");
        let _ = std::fs::remove_dir_all(dir.parent().unwrap());
    }

    #[test]
    fn test_extract_panic_payload() {
        use super::extract_panic_payload;
        let str_payload: Box<dyn std::any::Any + Send> = Box::new("str error");
        assert_eq!(extract_panic_payload(&*str_payload), "str error");

        let string_payload: Box<dyn std::any::Any + Send> = Box::new("string error".to_string());
        assert_eq!(extract_panic_payload(&*string_payload), "string error");

        let other_payload: Box<dyn std::any::Any + Send> = Box::new(42i32);
        assert_eq!(
            extract_panic_payload(&*other_payload),
            "<non-string payload>"
        );
    }
}
