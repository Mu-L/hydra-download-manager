// Copyright (C) 2026 Javad Rajabzadeh
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Reading a session out of the browser that already has one.
//!
//! The reason a download manager exists is usually that a browser session
//! exists: the file is behind a university login, a private GitLab, a forum, a
//! paywalled dataset. Copying a `Cookie:` header out of devtools per download
//! and re-copying it when it rotates is the friction this removes.
//!
//! # Scope, and why it is narrow on purpose
//!
//! [`load`] takes the host being downloaded from and returns only the cookies
//! that host would receive. The whole profile is read — a b-tree scan has to
//! start somewhere — but everything else is dropped before [`load`] returns, so
//! no caller can persist or print a cookie for a site it is not fetching from.
//! Nothing here writes to a jar file; `--cookie-jar` is a separate decision the
//! user makes separately.
//!
//! # Consent
//!
//! [`Import::store`] names the exact file that was read, and the CLI and the
//! GUI both print it the first time a browser is used in a run. A download
//! manager reading a browser's keychain without saying so is indistinguishable
//! from malware, and the difference between the two is entirely whether the
//! user was told.
//!
//! # Reaching the store at all
//!
//! On macOS a browser profile is behind the system privacy control, so the
//! first call fails with [`Error::Denied`] until the program running this is
//! granted Full Disk Access. That is reported as its own error, naming the
//! path and the remedy, rather than as "no profile found" — the directory is
//! exactly where it should be, and sending the user to look for it is the
//! worst possible answer.
//!
//! # Locked stores
//!
//! Chromium holds an exclusive lock on `Cookies` while it runs, so the store is
//! copied to a temporary file and opened there rather than telling the user to
//! close their browser. The write-ahead log is copied alongside it, because on
//! a running browser that is where the newest cookies are.
//!
//! # At-rest encryption
//!
//! | Family | Store | Key |
//! |---|---|---|
//! | Firefox, LibreWolf, Zen | `cookies.sqlite` | values are plaintext |
//! | Chromium | `Cookies`, `encrypted_value` | macOS: Keychain → PBKDF2 → AES-128-CBC; Linux: libsecret → same, or the documented `peanuts` fallback; Windows: DPAPI-unwrapped AES-256-GCM |
//! | Safari | `Cookies.binarycookies` | values are plaintext |

use std::fmt;
use std::path::{Path, PathBuf};
use std::str::FromStr;

use super::{Cookie, CookieJar};
use crate::cookies::{chromium, safari, sqlite};

/// A browser whose cookie store this can read.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum Browser {
    Firefox,
    LibreWolf,
    Zen,
    Chrome,
    Chromium,
    Edge,
    Brave,
    Vivaldi,
    Opera,
    Safari,
}

impl Browser {
    /// Every name `--cookies-from-browser` accepts, for help text and
    /// completions.
    pub const ALL: &'static [Browser] = &[
        Browser::Firefox,
        Browser::LibreWolf,
        Browser::Zen,
        Browser::Chrome,
        Browser::Chromium,
        Browser::Edge,
        Browser::Brave,
        Browser::Vivaldi,
        Browser::Opera,
        Browser::Safari,
    ];

    /// The lowercase name used on the command line.
    pub fn name(self) -> &'static str {
        match self {
            Browser::Firefox => "firefox",
            Browser::LibreWolf => "librewolf",
            Browser::Zen => "zen",
            Browser::Chrome => "chrome",
            Browser::Chromium => "chromium",
            Browser::Edge => "edge",
            Browser::Brave => "brave",
            Browser::Vivaldi => "vivaldi",
            Browser::Opera => "opera",
            Browser::Safari => "safari",
        }
    }

    fn is_firefox_family(self) -> bool {
        matches!(self, Browser::Firefox | Browser::LibreWolf | Browser::Zen)
    }
}

impl fmt::Display for Browser {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.name())
    }
}

impl FromStr for Browser {
    type Err = Error;

    fn from_str(s: &str) -> Result<Self, Error> {
        let want = s.trim().to_ascii_lowercase();
        Browser::ALL
            .iter()
            .copied()
            .find(|b| b.name() == want)
            // `google-chrome` and `msedge` are what the binaries are called on
            // Linux, and typing the binary's name is the obvious guess.
            .or(match want.as_str() {
                "google-chrome" | "google chrome" => Some(Browser::Chrome),
                "msedge" | "microsoft-edge" => Some(Browser::Edge),
                "brave-browser" => Some(Browser::Brave),
                _ => None,
            })
            .ok_or_else(|| Error::UnknownBrowser(s.to_string()))
    }
}

/// Which browser, and which of its profiles.
///
/// Parsed from `BROWSER[:PROFILE]`, the spelling yt-dlp established and that
/// users already have in their fingers.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Source {
    pub browser: Browser,
    /// `None` takes the browser's default profile.
    pub profile: Option<String>,
}

impl fmt::Display for Source {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match &self.profile {
            Some(p) => write!(f, "{}:{p}", self.browser),
            None => write!(f, "{}", self.browser),
        }
    }
}

impl FromStr for Source {
    type Err = Error;

    fn from_str(s: &str) -> Result<Self, Error> {
        // Split on the FIRST colon only: a Windows profile path may contain
        // another, and the browser name never does.
        let (name, profile) = match s.split_once(':') {
            Some((b, p)) => (b, Some(p.trim().to_string()).filter(|p| !p.is_empty())),
            None => (s, None),
        };
        Ok(Source {
            browser: name.parse()?,
            profile,
        })
    }
}

#[derive(Debug)]
#[non_exhaustive]
pub enum Error {
    UnknownBrowser(String),
    /// No profile directory for this browser on this machine.
    NoProfile {
        browser: Browser,
        looked_in: String,
    },
    /// A profile was found but holds no cookie store.
    NoStore {
        browser: Browser,
        profile: PathBuf,
    },
    /// The path exists but this process may not read it.
    ///
    /// Separate from [`Error::NoProfile`] because the two send a user to
    /// completely different places. On macOS a browser profile is protected by
    /// the system's privacy control, so the very first use of this feature
    /// fails here — and reporting that as "no profile found" sends someone
    /// hunting for a directory that is sitting exactly where it should be.
    Denied {
        path: PathBuf,
    },
    /// The store could not be copied out from under a running browser.
    Locked {
        store: PathBuf,
        why: String,
    },
    Read {
        store: PathBuf,
        why: String,
    },
    /// The store was read but its values could not be decrypted.
    Decrypt {
        store: PathBuf,
        why: String,
    },
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::UnknownBrowser(s) => {
                let names: Vec<&str> = Browser::ALL.iter().map(|b| b.name()).collect();
                write!(f, "unknown browser {s:?} (known: {})", names.join(", "))
            }
            Error::NoProfile { browser, looked_in } => {
                write!(f, "no {browser} profile found; looked in {looked_in}")
            }
            Error::NoStore { browser, profile } => write!(
                f,
                "{browser} profile {} holds no cookie store",
                profile.display()
            ),
            Error::Denied { path } => write!(
                f,
                "{} cannot be read by this process{DENIED_REMEDY}",
                path.display()
            ),
            Error::Locked { store, why } => write!(
                f,
                "could not copy {} out from under the running browser: {why}",
                store.display()
            ),
            Error::Read { store, why } => write!(f, "{}: {why}", store.display()),
            Error::Decrypt { store, why } => {
                write!(f, "{}: cookies are encrypted and {why}", store.display())
            }
        }
    }
}

impl std::error::Error for Error {}

/// What to do about an [`Error::Denied`], where the platform has an answer.
#[cfg(target_os = "macos")]
const DENIED_REMEDY: &str =
    "; a browser profile is protected by macOS privacy control, so grant Full Disk Access \
     to the program running hydra in System Settings > Privacy & Security";
#[cfg(not(target_os = "macos"))]
const DENIED_REMEDY: &str = "; check the file's owner and mode";

/// An I/O failure against `path`, as the error it actually is.
///
/// A denial and a missing file are different problems with different answers,
/// and collapsing them is how "no firefox profile found" ends up printed about
/// a profile that is exactly where it should be.
fn io_error(path: &Path, e: &std::io::Error) -> Error {
    match e.kind() {
        std::io::ErrorKind::PermissionDenied => Error::Denied {
            path: path.to_path_buf(),
        },
        _ => Error::Read {
            store: path.to_path_buf(),
            why: e.to_string(),
        },
    }
}

/// What an import found, and where it found it.
#[derive(Clone, Debug)]
pub struct Import {
    /// Cookies for the requested host, and no others.
    pub jar: CookieJar,
    /// The exact file read, to be named in the consent line.
    pub store: PathBuf,
    pub browser: Browser,
    /// Cookies for this host that were held but could not be decrypted.
    ///
    /// Reported rather than silently dropped: on Windows, Chrome 127 and later
    /// wrap new cookies with a key only a SYSTEM-level process can unwrap, so
    /// an import can plausibly succeed, return nothing usable, and leave the
    /// user with an unexplained `403`.
    pub undecryptable: usize,
}

/// Read `host`'s cookies out of a browser's own store.
///
/// The returned jar holds only cookies that would be sent to `host`; everything
/// else the profile contains is dropped before this returns. Expired cookies
/// are dropped too, against `now`.
///
/// # Errors
///
/// [`Error`] names the profile path in every variant that has one, because
/// "could not read your cookies" without saying which file was tried is not
/// something a user can act on.
///
/// # Blocking
///
/// This reads files and, on macOS and Linux, runs the platform's secret-store
/// helper. Call it off an async executor.
pub fn load(src: &Source, host: &str, now: u64) -> Result<Import, Error> {
    let profile = profile_dir(src)?;
    let (mut jar, store, undecryptable) = read_profile(src.browser, &profile)?;
    // The scoping promise, and the only place it is kept: everything the
    // profile holds for other sites is dropped here, before this returns, so
    // no caller can print or persist a cookie for a site it is not fetching.
    jar.retain_for_host(host);
    jar.purge(now, false);
    Ok(Import {
        jar,
        store,
        browser: src.browser,
        undecryptable,
    })
}

/// Every cookie in one profile, the file they were read from, and how many
/// could not be decrypted.
///
/// Split from [`load`] because the two halves fail for unrelated reasons and
/// are worth reasoning about separately: finding a profile depends on how this
/// machine packages browsers, reading one depends on the browser's own schema
/// and crypto. `profile` is the profile DIRECTORY, except for Safari where it
/// is the cookie file itself — Safari has no profiles.
fn read_profile(browser: Browser, profile: &Path) -> Result<(CookieJar, PathBuf, usize), Error> {
    if browser == Browser::Safari {
        let raw = std::fs::read(profile).map_err(|e| io_error(profile, &e))?;
        return Ok((safari::parse(&raw), profile.to_path_buf(), 0));
    }
    let (db, store, _copy) = open_store(browser, profile)?;
    let (jar, undecryptable) = if browser.is_firefox_family() {
        (firefox_rows(&db, &store)?, 0)
    } else {
        chromium::read(&db, &store, browser)?
    };
    Ok((jar, store, undecryptable))
}

/// Open a profile's cookie database, and the temporary copy it was read from.
///
/// The copy is returned rather than dropped because it owns the file the `Db`
/// was read out of; letting it fall out of scope here would delete the store
/// out from under the caller on some future change to how `Db` reads.
fn open_store(browser: Browser, profile: &Path) -> Result<(sqlite::Db, PathBuf, TempCopy), Error> {
    let store = cookie_store(browser, profile)?;
    let copy = TempCopy::of(&store)?;
    let db = sqlite::Db::open(copy.path()).map_err(|e| Error::Read {
        store: store.clone(),
        why: e.to_string(),
    })?;
    Ok((db, store, copy))
}

/// Confirm this browser's cookie store can be reached, and say which file it is.
///
/// What a settings screen asks before the first download rather than after each
/// one: on macOS every browser profile is behind the system privacy control, so
/// a picker that accepted a browser and then failed on every address would be
/// reporting a configuration problem as a download problem.
///
/// Deliberately does NOT decrypt, so choosing a Chromium in a dropdown cannot
/// put a Keychain prompt on screen. That question is asked when a download
/// actually needs an answer.
///
/// # Errors
///
/// The same [`Error`] set as [`load`], minus [`Error::Decrypt`].
///
/// # Blocking
///
/// Reads files. Call it off an async executor.
pub fn check(src: &Source) -> Result<PathBuf, Error> {
    let profile = profile_dir(src)?;
    if src.browser == Browser::Safari {
        std::fs::read(&profile).map_err(|e| io_error(&profile, &e))?;
        return Ok(profile);
    }
    let (db, store, _copy) = open_store(src.browser, &profile)?;
    let table = if src.browser.is_firefox_family() {
        "moz_cookies"
    } else {
        "cookies"
    };
    db.rows(table, &["name"]).map_err(|e| Error::Read {
        store: store.clone(),
        why: e.to_string(),
    })?;
    Ok(store)
}

/// Firefox's `moz_cookies`: one row per cookie, values in the clear.
fn firefox_rows(db: &sqlite::Db, store: &Path) -> Result<CookieJar, Error> {
    let cols = [
        "name",
        "value",
        "host",
        "path",
        "expiry",
        "isSecure",
        "isHttpOnly",
    ];
    let rows = db.rows("moz_cookies", &cols).map_err(|e| Error::Read {
        store: store.to_path_buf(),
        why: e.to_string(),
    })?;
    Ok(rows
        .into_iter()
        .filter_map(|r| {
            let host_field = r[2].as_str();
            let name = r[0].as_str();
            if name.is_empty() || host_field.is_empty() {
                return None;
            }
            Some(Cookie {
                name: name.to_string(),
                value: r[1].as_str().to_string(),
                domain: super::canonical_host(host_field),
                // Firefox keeps the RFC's own distinction in the leading dot.
                host_only: !host_field.starts_with('.'),
                path: path_or_root(r[3].as_str()),
                secure: r[5].as_int() != 0,
                http_only: r[6].as_int() != 0,
                expires: (r[4].as_int() > 0).then(|| r[4].as_int() as u64),
            })
        })
        .collect())
}

pub(crate) fn path_or_root(p: &str) -> String {
    if p.starts_with('/') {
        p.to_string()
    } else {
        "/".to_string()
    }
}

/// The profile directory to read, or for Safari the cookie file itself.
fn profile_dir(src: &Source) -> Result<PathBuf, Error> {
    let roots = roots(src.browser);
    if src.browser == Browser::Safari {
        return roots
            .iter()
            .find(|p| p.is_file())
            .cloned()
            .ok_or_else(|| Error::NoProfile {
                browser: src.browser,
                looked_in: described(&roots),
            });
    }
    let root = roots
        .iter()
        .find(|p| p.is_dir())
        .ok_or_else(|| Error::NoProfile {
            browser: src.browser,
            looked_in: described(&roots),
        })?;
    profile_dir_in(src, root)
}

/// The profile to read inside a known root.
///
/// Split from [`profile_dir`] so the choice can be reasoned about without
/// depending on where this machine happens to keep its browsers — which is the
/// one part of the lookup that cannot be exercised any other way.
fn profile_dir_in(src: &Source, root: &Path) -> Result<PathBuf, Error> {
    // Asked before the scan rather than inferred from its empty result: the
    // scan walks directories with `read_dir(..).into_iter().flatten()`, which
    // reads a refusal and an empty directory as the same thing.
    if let Err(e) = std::fs::read_dir(root) {
        return Err(io_error(root, &e));
    }
    let chosen = if src.browser.is_firefox_family() {
        firefox_profile(root, src.profile.as_deref())
    } else {
        chromium_profile(root, src.profile.as_deref())
    };
    chosen.ok_or_else(|| Error::NoProfile {
        browser: src.browser,
        looked_in: root.display().to_string(),
    })
}

/// Firefox profiles live in randomly-named directories under `Profiles/`.
///
/// A named profile matches by substring, because the on-disk name is
/// `8f3k2j1x.my-profile` and nobody types the salt. With no name, the
/// `default-release` profile wins — Firefox's own default since 67 — then
/// `default`, then whichever profile's store was written most recently, which
/// is the one the user is actually browsing in.
fn firefox_profile(root: &Path, want: Option<&str>) -> Option<PathBuf> {
    let all: Vec<PathBuf> = ["Profiles", "."]
        .iter()
        .flat_map(|sub| std::fs::read_dir(root.join(sub)).into_iter().flatten())
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.is_dir())
        .collect();
    let with_store: Vec<PathBuf> = all
        .iter()
        .filter(|p| p.join("cookies.sqlite").is_file())
        .cloned()
        .collect();
    let named = |suffix: &str| {
        with_store
            .iter()
            .find(|p| file_name(p).ends_with(suffix))
            .cloned()
    };
    match want {
        // The label after the salt, exactly, before a substring anywhere:
        // `default` is inside `default-release`, and a user who typed the first
        // must not be handed the second. The exact match is taken with or
        // without a store, so a named profile that has none is reported as
        // such rather than quietly swapped for a sibling.
        Some(w) => {
            let label = |p: &Path| {
                file_name(p)
                    .split_once('.')
                    .map(|(_, l)| l.to_string())
                    .unwrap_or_default()
            };
            all.iter()
                .find(|p| label(p) == w)
                .or_else(|| with_store.iter().find(|p| file_name(p).contains(w)))
                .cloned()
                .or_else(|| root.join(w).is_dir().then(|| root.join(w)))
        }
        None => named(".default-release")
            .or_else(|| named(".default"))
            .or_else(|| newest_by(&with_store, "cookies.sqlite")),
    }
}

/// Chromium profiles are `Default`, `Profile 1`, `Profile 2`… under the user
/// data directory, and the name is exactly what the user sees in the browser's
/// profile menu only for `Default`.
fn chromium_profile(root: &Path, want: Option<&str>) -> Option<PathBuf> {
    match want {
        Some(w) => {
            let exact = root.join(w);
            exact.is_dir().then_some(exact)
        }
        None => {
            let default = root.join("Default");
            if default.is_dir() {
                return Some(default);
            }
            let dirs: Vec<PathBuf> = std::fs::read_dir(root)
                .into_iter()
                .flatten()
                .flatten()
                .map(|e| e.path())
                .filter(|p| p.is_dir() && file_name(p).starts_with("Profile "))
                .collect();
            dirs.first().cloned()
        }
    }
}

/// Newest `leaf` under any of `dirs`, by modification time.
fn newest_by(dirs: &[PathBuf], leaf: &str) -> Option<PathBuf> {
    dirs.iter()
        .filter_map(|d| {
            let m = std::fs::metadata(d.join(leaf)).ok()?.modified().ok()?;
            Some((m, d.clone()))
        })
        .max_by_key(|(m, _)| *m)
        .map(|(_, d)| d)
}

/// The cookie file inside a profile directory.
///
/// Chromium moved it under `Network/` in version 96 and kept reading the old
/// location, so both are live on machines that have been upgraded.
fn cookie_store(browser: Browser, profile: &Path) -> Result<PathBuf, Error> {
    let candidates: &[&str] = if browser.is_firefox_family() {
        &["cookies.sqlite"]
    } else {
        &["Network/Cookies", "Cookies"]
    };
    candidates
        .iter()
        .map(|c| profile.join(c))
        .find(|p| p.is_file())
        .ok_or_else(|| Error::NoStore {
            browser,
            profile: profile.to_path_buf(),
        })
}

fn file_name(p: &Path) -> String {
    p.file_name().unwrap_or_default().to_string_lossy().into()
}

fn described(paths: &[PathBuf]) -> String {
    paths
        .iter()
        .map(|p| p.display().to_string())
        .collect::<Vec<_>>()
        .join(", ")
}

#[cfg(not(target_os = "windows"))]
fn home() -> Option<PathBuf> {
    std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .map(PathBuf::from)
}

#[cfg(not(target_os = "macos"))]
fn env_dir(key: &str) -> Option<PathBuf> {
    std::env::var_os(key).map(PathBuf::from)
}

/// Where a browser keeps its profiles, per platform.
///
/// Several candidates per browser and platform because packaging moves them:
/// a Flatpak Firefox is under `~/.var/app`, and a Snap Chromium under
/// `~/snap`. Returning the list rather than one path is also what lets
/// [`Error::NoProfile`] say where it looked.
#[cfg(target_os = "macos")]
fn roots(b: Browser) -> Vec<PathBuf> {
    let Some(h) = home() else { return Vec::new() };
    let app = h.join("Library/Application Support");
    let one = |p: &str| vec![app.join(p)];
    match b {
        Browser::Firefox => one("Firefox"),
        Browser::LibreWolf => one("LibreWolf"),
        Browser::Zen => one("zen"),
        Browser::Chrome => one("Google/Chrome"),
        Browser::Chromium => one("Chromium"),
        Browser::Edge => one("Microsoft Edge"),
        Browser::Brave => one("BraveSoftware/Brave-Browser"),
        Browser::Vivaldi => one("Vivaldi"),
        Browser::Opera => one("com.operasoftware.Opera"),
        Browser::Safari => vec![
            h.join(
                "Library/Containers/com.apple.Safari/Data/Library/Cookies/Cookies.binarycookies",
            ),
            h.join("Library/Cookies/Cookies.binarycookies"),
        ],
    }
}

#[cfg(target_os = "windows")]
fn roots(b: Browser) -> Vec<PathBuf> {
    let (Some(roaming), Some(local)) = (env_dir("APPDATA"), env_dir("LOCALAPPDATA")) else {
        return Vec::new();
    };
    match b {
        Browser::Firefox => vec![roaming.join("Mozilla/Firefox")],
        Browser::LibreWolf => vec![roaming.join("librewolf")],
        Browser::Zen => vec![roaming.join("zen")],
        Browser::Chrome => vec![local.join("Google/Chrome/User Data")],
        Browser::Chromium => vec![local.join("Chromium/User Data")],
        Browser::Edge => vec![local.join("Microsoft/Edge/User Data")],
        Browser::Brave => vec![local.join("BraveSoftware/Brave-Browser/User Data")],
        Browser::Vivaldi => vec![local.join("Vivaldi/User Data")],
        Browser::Opera => vec![roaming.join("Opera Software/Opera Stable")],
        // No Safari on Windows since 2012.
        Browser::Safari => Vec::new(),
    }
}

#[cfg(not(any(target_os = "macos", target_os = "windows")))]
fn roots(b: Browser) -> Vec<PathBuf> {
    let Some(h) = home() else { return Vec::new() };
    let cfg = env_dir("XDG_CONFIG_HOME").unwrap_or_else(|| h.join(".config"));
    let flatpak = h.join(".var/app");
    match b {
        Browser::Firefox => vec![
            h.join(".mozilla/firefox"),
            flatpak.join("org.mozilla.firefox/.mozilla/firefox"),
            h.join("snap/firefox/common/.mozilla/firefox"),
        ],
        Browser::LibreWolf => vec![
            h.join(".librewolf"),
            flatpak.join("io.gitlab.librewolf-community/.librewolf"),
        ],
        Browser::Zen => vec![h.join(".zen"), flatpak.join("app.zen_browser.zen/.zen")],
        Browser::Chrome => vec![
            cfg.join("google-chrome"),
            flatpak.join("com.google.Chrome/config/google-chrome"),
        ],
        Browser::Chromium => vec![
            cfg.join("chromium"),
            flatpak.join("org.chromium.Chromium/config/chromium"),
            h.join("snap/chromium/common/chromium"),
        ],
        Browser::Edge => vec![cfg.join("microsoft-edge")],
        Browser::Brave => vec![
            cfg.join("BraveSoftware/Brave-Browser"),
            flatpak.join("com.brave.Browser/config/BraveSoftware/Brave-Browser"),
        ],
        Browser::Vivaldi => vec![cfg.join("vivaldi")],
        Browser::Opera => vec![cfg.join("opera")],
        Browser::Safari => Vec::new(),
    }
}

/// A private copy of a store, removed when it goes out of scope.
///
/// Chromium holds an exclusive lock on `Cookies` for as long as it runs, and
/// telling the user to close their browser is not an answer when the whole
/// point is to use the session it is holding open. Copying sidesteps the lock:
/// the file can be READ while locked, it is the connection SQLite would open
/// that is refused.
///
/// The write-ahead log comes too. Without it a running Firefox's newest
/// cookies — which is to say the session the user just logged in with — are
/// invisible, because they have not been checkpointed into the database yet.
#[derive(Debug)]
struct TempCopy {
    dir: PathBuf,
    file: PathBuf,
}

impl TempCopy {
    fn of(store: &Path) -> Result<Self, Error> {
        let stamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        let dir =
            std::env::temp_dir().join(format!("hydra-cookies-{}-{stamp}", std::process::id()));
        let lock = |why: String| Error::Locked {
            store: store.to_path_buf(),
            why,
        };
        // Owner-only, set at creation: the copy holds every site's cookies
        // until it is read and scoped, and on Linux the temp root is shared
        // with every other user of the machine.
        let mut builder = std::fs::DirBuilder::new();
        #[cfg(unix)]
        {
            use std::os::unix::fs::DirBuilderExt;
            builder.mode(0o700);
        }
        builder.create(&dir).map_err(|e| lock(e.to_string()))?;
        let file = dir.join("store");
        // A refusal here is the platform's, not the browser's lock: say which.
        std::fs::copy(store, &file).map_err(|e| match e.kind() {
            std::io::ErrorKind::PermissionDenied => io_error(store, &e),
            _ => lock(e.to_string()),
        })?;
        let mut wal = store.as_os_str().to_os_string();
        wal.push("-wal");
        // Absent on a database that was cleanly closed, which is not an error.
        let _ = std::fs::copy(PathBuf::from(wal), dir.join("store-wal"));
        Ok(TempCopy { dir, file })
    }

    fn path(&self) -> &Path {
        &self.file
    }
}

impl Drop for TempCopy {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture(name: &str) -> PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures")
            .join(name)
    }

    #[test]
    fn firefox_rows_map_the_leading_dot_and_the_flag_columns() {
        let db = sqlite::Db::open(&fixture("ff.sqlite")).unwrap();
        let jar = firefox_rows(&db, &fixture("ff.sqlite")).unwrap();
        let by = |n: &str| jar.iter().find(|c| c.name == n).cloned().unwrap();

        let sid = by("sid");
        assert_eq!(sid.domain, "example.org");
        assert!(!sid.host_only, "a leading dot is a Domain cookie");
        assert!(sid.secure && sid.http_only);
        assert_eq!(sid.expires, Some(2_000_000_000));

        let csrf = by("csrf");
        assert_eq!(csrf.domain, "www.example.org");
        assert!(csrf.host_only);
        assert_eq!(csrf.path, "/files");
        assert!(csrf.is_session(), "expiry 0 is a session cookie");
    }

    /// The scoping promise: a profile of 404 cookies yields the two the host
    /// being downloaded from would receive, and nothing else reaches the jar.
    #[test]
    fn a_profile_is_narrowed_to_the_host_and_the_expired_are_dropped() {
        let db = sqlite::Db::open(&fixture("ff.sqlite")).unwrap();
        let mut jar = firefox_rows(&db, &fixture("ff.sqlite")).unwrap();
        assert_eq!(jar.len(), 404);
        jar.retain_for_host("www.example.org");
        jar.purge(1_700_000_000, false);
        let names: Vec<&str> = jar.iter().map(|c| c.name.as_str()).collect();
        assert_eq!(
            names,
            ["sid", "csrf"],
            "`stale` expired, filler is another host"
        );
        assert_eq!(
            jar.header_value("www.example.org", "/files/x", true, 1_700_000_000)
                .as_deref(),
            Some("csrf=def456; sid=abc123")
        );
    }

    #[test]
    fn a_locked_store_is_read_through_a_copy_that_takes_the_log_with_it() {
        let copy = TempCopy::of(&fixture("wal.sqlite")).unwrap();
        let dir = copy.dir.clone();
        let db = sqlite::Db::open(copy.path()).unwrap();
        let jar = firefox_rows(&db, copy.path()).unwrap();
        assert_eq!(
            jar.iter().find(|c| c.name == "sid").unwrap().value,
            "from-the-wal",
            "the log was copied alongside the database"
        );
        drop(copy);
        assert!(
            !dir.exists(),
            "the copy is removed when it goes out of scope"
        );
    }

    /// The copy holds every site's cookies until it is scoped, and on Linux
    /// the temp root is shared with every user of the machine.
    #[cfg(unix)]
    #[test]
    fn the_copy_lives_in_a_directory_only_its_owner_can_enter() {
        use std::os::unix::fs::PermissionsExt;
        let copy = TempCopy::of(&fixture("wal.sqlite")).unwrap();
        let mode = std::fs::metadata(&copy.dir).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o700, "{mode:o}");
    }

    #[test]
    fn an_unreadable_store_names_the_path_it_tried() {
        let missing = fixture("no-such-store.sqlite");
        let e = TempCopy::of(&missing).unwrap_err().to_string();
        assert!(e.contains("no-such-store.sqlite"), "{e}");
    }

    /// A throwaway directory tree, removed when the test ends.
    struct Scratch(PathBuf);

    impl Scratch {
        fn new(name: &str) -> Self {
            let dir = std::env::temp_dir().join(format!(
                "hydra-browser-{name}-{}-{:?}",
                std::process::id(),
                std::thread::current().id()
            ));
            let _ = std::fs::remove_dir_all(&dir);
            std::fs::create_dir_all(&dir).unwrap();
            Scratch(dir)
        }

        /// An empty directory at `rel`, and its path.
        fn dir(&self, rel: &str) -> PathBuf {
            let p = self.0.join(rel);
            std::fs::create_dir_all(&p).unwrap();
            p
        }

        /// A committed store fixture copied to `rel`, standing in for the one
        /// a real browser would have written there.
        fn store(&self, rel: &str, from: &str) -> PathBuf {
            let p = self.0.join(rel);
            std::fs::create_dir_all(p.parent().unwrap()).unwrap();
            std::fs::copy(fixture(from), &p).unwrap();
            p
        }

        fn touch(&self, rel: &str) -> PathBuf {
            let p = self.0.join(rel);
            std::fs::create_dir_all(p.parent().unwrap()).unwrap();
            std::fs::write(&p, b"").unwrap();
            p
        }
    }

    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn a_firefox_profile_is_read_whole_and_says_where_from() {
        let s = Scratch::new("ff");
        let profile = s.dir("ab12cd34.default-release");
        s.store("ab12cd34.default-release/cookies.sqlite", "ff.sqlite");
        let (jar, store, undecryptable) = read_profile(Browser::Firefox, &profile).unwrap();
        assert_eq!(jar.len(), 404);
        assert_eq!(undecryptable, 0);
        assert!(store.ends_with("cookies.sqlite"));
    }

    #[test]
    fn a_chromium_profile_is_read_through_the_network_subdirectory() {
        let s = Scratch::new("ch");
        let profile = s.dir("Default");
        s.store("Default/Network/Cookies", "ch.sqlite");
        let (jar, store, _) = read_profile(Browser::Chrome, &profile).unwrap();
        assert_eq!(jar.len(), 3);
        assert!(store.ends_with("Network/Cookies"));
    }

    /// Chromium moved the store under `Network/` in version 96 and kept reading
    /// the old path, so an upgraded machine has both and the new one wins.
    #[test]
    fn the_newer_store_location_wins_over_the_one_beside_it() {
        let s = Scratch::new("ch2");
        let profile = s.dir("Default");
        s.store("Default/Cookies", "ch.sqlite");
        assert!(cookie_store(Browser::Chrome, &profile)
            .unwrap()
            .ends_with("Cookies"));
        s.store("Default/Network/Cookies", "ch.sqlite");
        assert!(cookie_store(Browser::Chrome, &profile)
            .unwrap()
            .ends_with("Network/Cookies"));
    }

    #[test]
    fn a_profile_with_no_store_names_itself() {
        let s = Scratch::new("empty");
        let profile = s.dir("Default");
        let e = read_profile(Browser::Chrome, &profile)
            .unwrap_err()
            .to_string();
        assert!(e.contains("chrome") && e.contains("Default"), "{e}");
    }

    #[test]
    fn a_store_that_is_not_a_database_reports_the_file_not_the_byte() {
        let s = Scratch::new("junk");
        let profile = s.dir("p");
        s.touch("p/cookies.sqlite");
        let e = read_profile(Browser::Firefox, &profile)
            .unwrap_err()
            .to_string();
        assert!(e.contains("cookies.sqlite"), "{e}");
        assert!(e.contains("SQLite"), "{e}");
    }

    #[test]
    fn safari_reads_the_cookie_file_itself_because_it_has_no_profiles() {
        let s = Scratch::new("safari");
        let file = s.0.join("Cookies.binarycookies");
        // "cook" magic with zero pages: a real, empty store.
        std::fs::write(&file, b"cook\x00\x00\x00\x00").unwrap();
        let (jar, store, _) = read_profile(Browser::Safari, &file).unwrap();
        assert!(jar.is_empty());
        assert_eq!(store, file);

        let missing = s.0.join("gone.binarycookies");
        let e = read_profile(Browser::Safari, &missing)
            .unwrap_err()
            .to_string();
        assert!(e.contains("gone.binarycookies"), "{e}");
    }

    /// Firefox profile directories are named `<salt>.<label>`, so nobody types
    /// the whole thing: `default-release` is the default since Firefox 67, a
    /// named profile matches by substring, and otherwise the one most recently
    /// written is the one being browsed in.
    #[test]
    fn the_firefox_profile_is_chosen_the_way_a_user_would_expect() {
        let s = Scratch::new("ffpick");
        let root = s.dir("Firefox");
        for p in ["aaa.default", "bbb.default-release", "ccc.work"] {
            s.store(&format!("Firefox/Profiles/{p}/cookies.sqlite"), "ff.sqlite");
        }
        let name = |p: Option<PathBuf>| file_name(&p.expect("a profile"));

        assert_eq!(name(firefox_profile(&root, None)), "bbb.default-release");
        assert_eq!(name(firefox_profile(&root, Some("work"))), "ccc.work");
        assert_eq!(name(firefox_profile(&root, Some("aaa"))), "aaa.default");
        assert_eq!(
            name(firefox_profile(&root, Some("default"))),
            "aaa.default",
            "`default` is a label of its own, not a prefix of default-release"
        );
        assert!(firefox_profile(&root, Some("nosuch")).is_none());
    }

    /// Found on a real machine: `firefox:default` read `default-release`,
    /// because the profile actually called `default` had no store and the
    /// substring match fell through to the one that did. The profile the user
    /// named is the answer, and its missing store is the error.
    #[test]
    fn a_named_profile_without_a_store_is_reported_not_swapped() {
        let s = Scratch::new("ffnamedbare");
        let root = s.dir("Firefox");
        s.dir("Firefox/Profiles/aaa.default");
        s.store(
            "Firefox/Profiles/bbb.default-release/cookies.sqlite",
            "ff.sqlite",
        );
        let src = Source {
            browser: Browser::Firefox,
            profile: Some("default".into()),
        };
        let chosen = profile_dir_in(&src, &root).expect("the named profile");
        assert_eq!(file_name(&chosen), "aaa.default");
        let e = cookie_store(Browser::Firefox, &chosen)
            .unwrap_err()
            .to_string();
        assert!(e.contains("aaa.default"), "{e}");
        assert_eq!(
            file_name(&firefox_profile(&root, None).expect("a profile")),
            "bbb.default-release",
            "with no name, only a profile with a store is a candidate"
        );
    }

    #[test]
    fn a_profile_directory_without_a_store_is_not_a_profile() {
        let s = Scratch::new("ffbare");
        let root = s.dir("Firefox");
        s.dir("Firefox/Profiles/aaa.default-release");
        assert!(firefox_profile(&root, None).is_none());
    }

    /// With no `default-release` and no `default`, the profile whose store was
    /// written last is the one the user is actually browsing in.
    #[test]
    fn the_most_recently_used_firefox_profile_is_the_fallback() {
        let s = Scratch::new("ffnewest");
        let root = s.dir("Firefox");
        s.store("Firefox/Profiles/aaa.one/cookies.sqlite", "ff.sqlite");
        let newer = s.store("Firefox/Profiles/bbb.two/cookies.sqlite", "ff.sqlite");
        // Written after, and said so: `copy` can land both in the same
        // filesystem timestamp tick.
        filetime_now_plus(&newer, 120);
        assert_eq!(
            file_name(&firefox_profile(&root, None).expect("a profile")),
            "bbb.two"
        );
    }

    #[test]
    fn the_chromium_profile_is_default_unless_one_is_named() {
        let s = Scratch::new("chpick");
        let root = s.dir("User Data");
        s.dir("User Data/Default");
        s.dir("User Data/Profile 2");

        assert_eq!(
            file_name(&chromium_profile(&root, None).unwrap()),
            "Default"
        );
        assert_eq!(
            file_name(&chromium_profile(&root, Some("Profile 2")).unwrap()),
            "Profile 2"
        );
        assert!(chromium_profile(&root, Some("Profile 9")).is_none());
    }

    /// A machine whose only profile is a numbered one still has a profile.
    #[test]
    fn a_chromium_install_with_no_default_falls_back_to_a_numbered_profile() {
        let s = Scratch::new("chnodefault");
        let root = s.dir("User Data");
        s.dir("User Data/Profile 1");
        s.dir("User Data/Crashpad");
        assert_eq!(
            file_name(&chromium_profile(&root, None).unwrap()),
            "Profile 1"
        );
    }

    /// Every error says which file or directory it is about, because "could not
    /// read your cookies" is not something a user can act on.
    #[test]
    fn every_error_names_what_it_was_looking_at() {
        let cases = [
            Error::NoProfile {
                browser: Browser::Firefox,
                looked_in: "/somewhere".into(),
            },
            Error::NoStore {
                browser: Browser::Chrome,
                profile: PathBuf::from("/somewhere"),
            },
            Error::Locked {
                store: PathBuf::from("/somewhere"),
                why: "busy".into(),
            },
            Error::Read {
                store: PathBuf::from("/somewhere"),
                why: "bad".into(),
            },
            Error::Decrypt {
                store: PathBuf::from("/somewhere"),
                why: "no key".into(),
            },
            Error::Denied {
                path: PathBuf::from("/somewhere"),
            },
        ];
        for e in cases {
            let text = e.to_string();
            assert!(text.contains("somewhere"), "{e:?} -> {text}");
        }
    }

    /// A directory that exists but cannot be READ is a permission problem, and
    /// saying "no profile found" about it sends the user looking for a profile
    /// that is exactly where it should be. This is the normal first run on
    /// macOS, where a browser profile is behind the system privacy control.
    #[cfg(unix)]
    #[test]
    fn an_unreadable_profile_directory_is_a_refusal_not_an_absence() {
        use std::os::unix::fs::PermissionsExt;

        let s = Scratch::new("denied");
        let root = s.dir("Firefox");
        s.store(
            "Firefox/Profiles/aaa.default-release/cookies.sqlite",
            "ff.sqlite",
        );
        std::fs::set_permissions(&root, std::fs::Permissions::from_mode(0o000)).unwrap();

        let src = Source {
            browser: Browser::Firefox,
            profile: None,
        };
        let e = profile_dir_in(&src, &root).unwrap_err();
        assert!(matches!(e, Error::Denied { .. }), "{e:?}");
        let text = e.to_string();
        assert!(text.contains("Firefox"), "{text}");
        assert!(!text.contains("no firefox profile found"), "{text}");

        std::fs::set_permissions(&root, std::fs::Permissions::from_mode(0o700)).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn an_unreadable_store_is_a_refusal_rather_than_a_lock() {
        use std::os::unix::fs::PermissionsExt;

        let s = Scratch::new("deniedstore");
        let profile = s.dir("p");
        let store = s.store("p/cookies.sqlite", "ff.sqlite");
        std::fs::set_permissions(&store, std::fs::Permissions::from_mode(0o000)).unwrap();

        let e = read_profile(Browser::Firefox, &profile).unwrap_err();
        assert!(matches!(e, Error::Denied { .. }), "{e:?}");

        std::fs::set_permissions(&store, std::fs::Permissions::from_mode(0o600)).unwrap();
    }

    /// The settings-screen question: can this be read at all, and from where.
    #[test]
    fn check_answers_with_the_store_it_would_read() {
        let s = Scratch::new("check");
        s.store(
            "Firefox/Profiles/aaa.default-release/cookies.sqlite",
            "ff.sqlite",
        );
        let src = Source {
            browser: Browser::Firefox,
            profile: None,
        };
        let store = profile_dir_in(&src, &s.0.join("Firefox"))
            .and_then(|p| open_store(Browser::Firefox, &p).map(|(_, store, _)| store))
            .unwrap();
        assert!(store.ends_with("cookies.sqlite"));
    }

    /// A store that is present but holds another browser's schema is a real
    /// failure mode — a Chromium `Cookies` file pointed at as a Firefox
    /// profile — and it must not read as success.
    #[test]
    fn check_refuses_a_store_without_the_table_it_needs() {
        let s = Scratch::new("checkschema");
        let profile = s.dir("p");
        s.store("p/cookies.sqlite", "ch.sqlite");
        let (db, store, _copy) = open_store(Browser::Firefox, &profile).unwrap();
        let e = db
            .rows("moz_cookies", &["name"])
            .map_err(|e| Error::Read {
                store,
                why: e.to_string(),
            })
            .unwrap_err()
            .to_string();
        assert!(e.contains("moz_cookies"), "{e}");
    }

    #[test]
    fn the_firefox_family_is_the_one_with_plaintext_values() {
        for b in [Browser::Firefox, Browser::LibreWolf, Browser::Zen] {
            assert!(b.is_firefox_family(), "{b}");
        }
        for b in [Browser::Chrome, Browser::Edge, Browser::Safari] {
            assert!(!b.is_firefox_family(), "{b}");
        }
    }

    /// Push a file's mtime `secs` into the future, so an ordering test does not
    /// depend on two copies landing in different filesystem ticks.
    fn filetime_now_plus(path: &Path, secs: u64) {
        let when = std::time::SystemTime::now() + std::time::Duration::from_secs(secs);
        std::fs::File::open(path)
            .unwrap()
            .set_times(std::fs::FileTimes::new().set_modified(when))
            .unwrap();
    }

    #[test]
    fn source_parses_browser_and_optional_profile() {
        assert_eq!(
            "firefox".parse::<Source>().unwrap(),
            Source {
                browser: Browser::Firefox,
                profile: None
            }
        );
        assert_eq!(
            "chrome:Profile 2".parse::<Source>().unwrap(),
            Source {
                browser: Browser::Chrome,
                profile: Some("Profile 2".into())
            }
        );
        // A trailing colon names no profile rather than an empty one.
        assert_eq!("edge:".parse::<Source>().unwrap().profile, None);
        assert_eq!("BRAVE".parse::<Source>().unwrap().browser, Browser::Brave);
    }

    #[test]
    fn an_unknown_browser_lists_the_known_ones() {
        let e = "netscape".parse::<Source>().unwrap_err().to_string();
        assert!(e.contains("netscape"), "{e}");
        assert!(e.contains("firefox") && e.contains("safari"), "{e}");
    }

    #[test]
    fn display_round_trips_a_source() {
        for s in ["firefox", "chrome:Profile 2", "safari"] {
            assert_eq!(s.parse::<Source>().unwrap().to_string(), s);
        }
    }

    #[test]
    fn binary_names_map_to_their_browser() {
        for (typed, want) in [
            ("google-chrome", Browser::Chrome),
            ("msedge", Browser::Edge),
            ("brave-browser", Browser::Brave),
        ] {
            assert_eq!(typed.parse::<Browser>().unwrap(), want);
        }
    }
}
