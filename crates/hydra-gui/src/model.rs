// Copyright (C) 2026 Javad Rajabzadeh
// SPDX-License-Identifier: GPL-3.0-or-later

//! Persistent data model: downloads, categories, queues, settings.
//!
//! Everything here serializes to one JSON file in the platform config dir, so
//! the download list and options survive restarts. Live per-connection
//! state is `#[serde(skip)]` — a restart resumes from
//! the received spans (`held`), not from socket state.

use serde::{Deserialize, Serialize};
use std::path::PathBuf;

pub type DlId = u64;

#[derive(Clone, Copy, PartialEq, Eq, Debug, Serialize, Deserialize)]
pub enum DlState {
    /// In a queue, waiting for a worker slot.
    Queued,
    /// Probing / sending the first requests.
    Connecting,
    /// Bytes are arriving.
    Receiving,
    Paused,
    Complete,
    Error,
}

impl DlState {
    pub fn is_active(self) -> bool {
        matches!(self, DlState::Connecting | DlState::Receiving)
    }
}

/// One row of the per-connection table in the progress dialog.
#[derive(Clone, Debug, Default)]
pub struct ConnRow {
    pub downloaded: u64,
    pub info: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct DownloadItem {
    pub id: DlId,
    pub url: String,
    pub file_name: String,
    /// Directory the finished file is moved to.
    pub save_dir: String,
    pub category: Option<String>,
    pub description: String,
    pub size: Option<u64>,
    pub downloaded: u64,
    pub state: DlState,
    pub error: Option<String>,
    /// Server honours byte ranges (the "Resume capability" line).
    pub resume: Option<bool>,
    pub added: i64,
    pub last_try: Option<i64>,
    /// Queue name when scheduled, `None` for direct downloads.
    pub queue: Option<String>,
    pub q_order: u32,
    /// HTTP basic credentials from "Use authorization".
    pub auth: Option<(String, String)>,
    /// Cookie header value for sites that gate downloads behind a session
    /// (filled manually or, later, by the browser extension).
    #[serde(default)]
    pub cookies: Option<String>,
    /// `Referer:` header value for a hotlink-protected origin: the page the
    /// browser was on when the extension captured this file. Sites that gate
    /// their CDN on it answer `403` to a request without it, however good the
    /// cookies are, so it travels with the item and is replayed on every
    /// start — the same way `StreamInfo::referer` already works for a
    /// manifest's segments.
    #[serde(default)]
    pub referer: Option<String>,
    /// Per-download cap, bytes/sec, when the Speed Limiter tab enables one.
    pub speed_limit: Option<u64>,
    /// Parked by the Connection tab's download limit, not by the user. The
    /// quota tick resumes exactly these when the window rolls over or the
    /// cap is raised; a hand-paused item carries `false` and stays put.
    #[serde(default)]
    pub limit_paused: bool,
    /// Byte spans confirmed on disk; drives resume and the chunk strip.
    pub held: Vec<(u64, u64)>,
    /// The `.part` staging file, pinned when the transfer first starts so a
    /// later rename in File Info cannot orphan the bytes being written.
    #[serde(default)]
    pub part_path: Option<String>,
    #[serde(skip)]
    pub rate: f64,
    /// Scheduler retries consumed in this session.
    #[serde(skip)]
    pub retries: u32,
    /// Animated progress fraction the widgets draw; glides toward
    /// `progress()` on the animation tick so the bar moves continuously
    /// between engine updates.
    #[serde(skip)]
    pub disp_progress: f32,
    #[serde(skip)]
    pub eta_secs: Option<u64>,
    /// Seconds of MEDIA captured, for a live recording. A live stream has no
    /// size, so this is what the progress dialog shows in place of one.
    #[serde(skip)]
    pub recorded_secs: Option<f64>,
    #[serde(skip)]
    pub conns: Vec<ConnRow>,
    #[serde(skip)]
    pub status_line: String,
    /// Shut down, log off or sleep the computer once this download (and any
    /// virus scan) finishes — the per-download analogue of a queue's
    /// [`Schedule::shutdown_when_done`]. The action itself only runs after
    /// the cancellable countdown in `WinKind::Power`.
    #[serde(default)]
    pub shutdown_after: bool,
    #[serde(default)]
    pub shutdown_action: PowerAction,
    /// Set when `url` names an adaptive-stream MANIFEST rather than a file.
    /// Its presence is what routes the item to the stream-aware engine path
    /// instead of the range scheduler.
    #[serde(default)]
    pub stream: Option<StreamInfo>,
    /// Set when this item came from a Metalink document. Its presence is what
    /// gives the transfer a mirror list, a size it can trust, a digest, and —
    /// where the document published `<pieces>` — per-chunk verification with
    /// targeted refetch instead of starting over.
    #[serde(default)]
    pub metalink: Option<MetalinkInfo>,
    /// The user named this file themselves — in the File Info / Properties
    /// dialog, or by taking the renamed copy the duplicate dialog offered.
    ///
    /// A probe answers AFTER the transfer it belongs to has started, so
    /// without this the `Content-Disposition` name arrived second and won:
    /// the edited name was written to the item, the download started, and
    /// the first `Probed` event put the server's name back and re-aimed the
    /// engine's final path at it. Every rename made from the dialog was lost
    /// that way, and the duplicate dialog's "Download as new file" copy was
    /// renamed back over the file it had just warned about.
    #[serde(default)]
    pub name_locked: bool,
    /// Which proxy this download takes: the app default, none, or its own.
    /// Editable in File Info and, while the transfer is stopped, in the
    /// progress dialog.
    #[serde(default)]
    pub proxy: ProxyChoice,
}

/// What a stream item needs beyond a URL: which rendition was chosen, and
/// what container the user asked the finished file to be. Persisted so a
/// restarted stream picks the same rendition instead of silently re-choosing.
#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
pub struct StreamInfo {
    /// "hls" or "dash".
    pub protocol: String,
    /// The variant playlist chosen in the browser, when one was.
    #[serde(default)]
    pub variant_url: Option<String>,
    #[serde(default)]
    pub height: Option<u32>,
    #[serde(default)]
    pub bandwidth: Option<u64>,
    /// "MP4" or "TS".
    #[serde(default)]
    pub container: String,
    /// Page the stream played on; sent as `Referer` with every segment.
    #[serde(default)]
    pub referer: Option<String>,
    /// The BROWSER's User-Agent, not Hydra's. Origins that gate on it hand
    /// a different playlist — or none — to anything else.
    #[serde(default)]
    pub user_agent: Option<String>,
    #[serde(default)]
    pub live: bool,
    /// Stop a live recording after this many seconds and finish the file.
    #[serde(default)]
    pub max_seconds: Option<u64>,
}

/// One mirror from a Metalink document, as the engine will use it.
#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
pub struct MirrorRef {
    pub url: String,
    /// Rank after the document's ordering has been normalised: 1 is best,
    /// always in the RFC 5854 direction whichever dialect it came from.
    ///
    /// Metalink 3.0 states preference on a scale that runs the OTHER way
    /// (0-100, higher better). Storing the document's own number would mean
    /// every consumer had to remember which dialect it came from, and the one
    /// that forgot would give most of the work to the mirror the publisher
    /// ranked last — a transfer that still succeeds, just slower, and
    /// indistinguishably from bad luck.
    #[serde(default)]
    pub priority: u32,
    /// A ceiling the MIRROR stated for itself. Narrows the user's per-host
    /// setting; never widens it.
    #[serde(default)]
    pub max_connections: Option<usize>,
}

/// What a Metalink document said about one file.
///
/// Persisted with the item for the same reason `held` is: a download resumed
/// after a restart that had lost its mirror list would fall back to one source
/// and lose its reserve bench — a difference invisible until the mirror that
/// failed before fails again.
#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
pub struct MetalinkInfo {
    /// Every mirror this build can fetch from, best first.
    #[serde(default)]
    pub mirrors: Vec<MirrorRef>,
    /// The size the document stated.
    ///
    /// This is what admits a second mirror at all. Agreement between mirrors is
    /// otherwise established on a strong validator, and independent mirror
    /// operators running independent web servers cannot share an `ETag` — so
    /// without a stated size a nineteen-mirror list downloads from one host.
    #[serde(default)]
    pub size: Option<u64>,
    /// The strongest digest the document published, as `algorithm:hex`.
    #[serde(default)]
    pub digest: Option<String>,
    /// The document's `<pieces>` as a chunk manifest, in its on-disk JSON form.
    ///
    /// Stored as text rather than a parsed structure so the GUI's state file
    /// stays independent of `hya-net`'s manifest type: this file is written by
    /// one version of Hydra and read by the next.
    #[serde(default)]
    pub pieces: Option<String>,
    /// Where the document came from, for the file-info dialog.
    #[serde(default)]
    pub origin: String,
    /// The document carried an OpenPGP `<signature>` over this file.
    ///
    /// Recorded and NOT verified. Saying nothing would let a user assume there
    /// was nothing to check; reporting it as verified would be worse.
    #[serde(default)]
    pub signed: bool,
}

/// How far a live recording has got, as a fraction.
///
/// Only a recording with a time limit has one: seconds captured against
/// seconds asked for. Without a limit there is no end to be a fraction of,
/// and bytes cannot stand in — a live stream has no size.
pub fn live_progress(max_seconds: Option<u64>, recorded: Option<f64>) -> f32 {
    match (max_seconds, recorded) {
        (Some(limit), Some(done)) if limit > 0 => (done / limit as f64).clamp(0.0, 1.0) as f32,
        _ => 0.0,
    }
}

impl DownloadItem {
    pub fn full_path(&self) -> PathBuf {
        PathBuf::from(&self.save_dir).join(&self.file_name)
    }

    /// The `.part` staging file: next to the destination, so completion is a
    /// same-filesystem rename. hydra writes bytes at their final offsets — no
    /// assembly pass exists, so a separate temp directory would only add a
    /// cross-drive copy at the end.
    pub fn part_file(&self) -> PathBuf {
        match &self.part_path {
            Some(p) => PathBuf::from(p),
            None => PathBuf::from(&self.save_dir).join(format!("{}.part", self.file_name)),
        }
    }

    pub fn progress(&self) -> f32 {
        // A live recording has no size, so bytes can say nothing about how
        // far along it is — but a recording with a time limit does have a
        // real fraction, and it is the one the person set. Seconds captured
        // against seconds asked for is the honest bar for that case.
        if let Some(si) = self.stream.as_ref().filter(|s| s.live) {
            return live_progress(si.max_seconds, self.recorded_secs);
        }
        match self.size {
            // Clamped: a size that is an estimate can lag the bytes it is
            // meant to bound, and a fraction over 1.0 draws a bar past its
            // own track.
            Some(s) if s > 0 => (self.downloaded as f64 / s as f64).min(1.0) as f32,
            _ => 0.0,
        }
    }

    /// The Status column text of the main list, in the active locale.
    pub fn status_text(&self) -> String {
        use crate::i18n::tr;
        match self.state {
            DlState::Complete => tr("Complete"),
            DlState::Paused => tr("Paused"),
            DlState::Queued => tr("Queued"),
            DlState::Error => tr("Error"),
            DlState::Connecting => tr("Connecting..."),
            DlState::Receiving => match self.size {
                Some(s) => crate::fmt::pct(self.downloaded, s),
                None => tr("Receiving..."),
            },
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct CategoryDef {
    pub name: String,
    /// Extensions (lowercase, no dot) auto-filed into this category.
    pub exts: Vec<String>,
    /// Default download directory for the category.
    pub dir: String,
}

fn downloads_dir() -> String {
    dirs::download_dir()
        .unwrap_or_else(|| PathBuf::from("Downloads"))
        .to_string_lossy()
        .into_owned()
}

fn sub(cat: &str) -> String {
    PathBuf::from(downloads_dir())
        .join(cat)
        .to_string_lossy()
        .into_owned()
}

/// The category model weights and datasets are filed under.
///
/// A constant rather than a literal because three places have to agree on it:
/// the default list, the tree's icon lookup, and the one-time migration that
/// adds it to an install that predates it.
pub const AI_CATEGORY: &str = "AI & Datasets";

/// Model weights, checkpoints and dataset containers.
///
/// Deliberately WITHOUT csv, tsv and json. They are dataset formats, but they
/// are also what a large part of the ordinary web serves, and a capture list
/// that swallows every one of them turns the browser into a nuisance — the
/// user can add them, but they must ask.
pub const AI_TYPES: &[&str] = &[
    "SAFETENSORS",
    "GGUF",
    "GGML",
    "ONNX",
    "PT",
    "PTH",
    "CKPT",
    "PB",
    "TFLITE",
    "H5",
    "HDF5",
    "KERAS",
    "NPY",
    "NPZ",
    "PKL",
    "MLMODEL",
    "MLPACKAGE",
    "CAFFEMODEL",
    "PARQUET",
    "PQ",
    "ARROW",
    "FEATHER",
    "AVRO",
    "ORC",
    "TFRECORD",
    "JSONL",
    "NDJSON",
];

/// The stock capture list: the classic media/archive set plus [`AI_TYPES`].
///
/// Composed rather than spelled out so the model formats have one definition
/// shared with the category and the migration.
pub fn default_auto_types() -> String {
    const BASE: &str = "3GP 7Z AAC ACE AIF APK ARJ ASF AVI BIN BZ2 DMG EXE GZ GZIP IMG ISO \
LZH M4A M4V MKV MOV MP3 MP4 MPA MPE MPEG MPG MSI MSU OGG OGV PDF PKG PPS PPT QT RA RAR RM \
RMVB SEA SIT SITX TAR TIF TIFF WAV WMA WMV Z ZIP";
    format!("{BASE} {}", AI_TYPES.join(" "))
}

pub fn default_categories() -> Vec<CategoryDef> {
    let e = |s: &str| s.split_whitespace().map(str::to_string).collect::<Vec<_>>();
    vec![
        CategoryDef {
            name: "General".into(),
            exts: vec![],
            dir: downloads_dir(),
        },
        CategoryDef {
            name: AI_CATEGORY.into(),
            exts: AI_TYPES.iter().map(|e| e.to_ascii_lowercase()).collect(),
            dir: sub(AI_CATEGORY),
        },
        CategoryDef {
            name: "Compressed".into(),
            exts: e("zip rar 7z gz gzip bz2 tar arj lzh sit sitx sea ace z xz zst"),
            dir: sub("Compressed"),
        },
        CategoryDef {
            name: "Documents".into(),
            exts: e("doc docx pdf ppt pptx pps txt rtf odt xls xlsx csv epub chm djvu"),
            dir: sub("Documents"),
        },
        CategoryDef {
            name: "Music".into(),
            exts: e("mp3 aac m4a wav wma ogg flac aif mpa ra"),
            dir: sub("Music"),
        },
        CategoryDef {
            name: "Programs".into(),
            exts: e("exe msi msu dmg pkg deb rpm appimage apk bin img iso"),
            dir: sub("Programs"),
        },
        CategoryDef {
            name: "Video".into(),
            exts: e("avi mp4 mkv mov mpg mpeg wmv flv m4v webm rm rmvb ogv 3gp asf qt ts"),
            dir: sub("Video"),
        },
    ]
}

/// Category for a file name, by extension, against the configured lists.
///
/// A name with no extension at all files under Programs: bare names like
/// `meilisearch-linux-aarch64` are almost always executables, and General
/// told the user nothing.
pub fn categorize(file: &str, cats: &[CategoryDef]) -> Option<String> {
    match file.rsplit_once('.') {
        None => cats
            .iter()
            .find(|c| c.name == "Programs")
            .map(|c| c.name.clone()),
        Some((_, ext)) => {
            let ext = ext.to_ascii_lowercase();
            cats.iter()
                .find(|c| c.exts.contains(&ext))
                .map(|c| c.name.clone())
        }
    }
}

/// Folder a download filed under `cat` should be saved in.
///
/// With `flat` on (Options > Save to > "Do not create category folders")
/// every category resolves to the first — General — folder, so no
/// per-category subdirectory is ever created. `cat` of `None` means "not
/// categorized", which also lands in General.
///
/// `None` comes back only when `cats` is empty or when `cat` names a
/// category that no longer exists; callers decide whether to fall back or to
/// leave the item's folder as it is.
pub fn category_dir(cats: &[CategoryDef], cat: Option<&str>, flat: bool) -> Option<String> {
    match cat.filter(|_| !flat) {
        Some(c) => cats.iter().find(|k| k.name == c).map(|k| k.dir.clone()),
        None => cats.first().map(|k| k.dir.clone()),
    }
}

/// Which palette the interface paints with (View > Theme).
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default, Serialize, Deserialize)]
pub enum ThemeMode {
    /// Follow the OS appearance, and keep following it: a desktop that
    /// switches to dark at sunset takes the interface with it, with no visit
    /// to the menu.
    #[default]
    System,
    Light,
    Dark,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug, Serialize, Deserialize)]
pub enum ProxyMode {
    None,
    System,
    Script,
    Manual,
}

/// What the manually configured proxy speaks.
///
/// The distinction is not cosmetic: a SOCKS proxy carries a TCP stream and is
/// dialled by the connector, while an HTTP proxy parses the request line and
/// is addressed by the target. `crate::proxy` turns this into one or the
/// other; picking the wrong one produces a connection that fails with a
/// message about the wrong protocol.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default, Serialize, Deserialize)]
pub enum ProxyType {
    /// Forward proxy: absolute-form requests, `CONNECT` for TLS. The default
    /// because it is what a bare `host:port` means everywhere else.
    #[default]
    Http,
    Socks4,
    Socks4a,
    Socks5,
}

impl ProxyType {
    /// The URL scheme this type is spelled with, which is also what
    /// `hya_net::Proxy::parse` reads.
    pub fn scheme(self) -> &'static str {
        match self {
            ProxyType::Http => "http",
            ProxyType::Socks4 => "socks4",
            ProxyType::Socks4a => "socks4a",
            ProxyType::Socks5 => "socks5",
        }
    }

    /// Every type, in the order the Options picker lists them.
    pub const ALL: [ProxyType; 4] = [
        ProxyType::Http,
        ProxyType::Socks4,
        ProxyType::Socks4a,
        ProxyType::Socks5,
    ];
}

impl std::fmt::Display for ProxyType {
    /// The picker's label. Not translated: these are protocol names.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            ProxyType::Http => "HTTP",
            ProxyType::Socks4 => "SOCKS4",
            ProxyType::Socks4a => "SOCKS4a",
            ProxyType::Socks5 => "SOCKS5",
        })
    }
}

/// Which proxy ONE download uses.
///
/// A download manager is often the reason a proxy exists on the machine at
/// all: one large file has to go through the tunnel while everything else
/// stays on the fast direct path, or the other way round. The choice travels
/// with the item and is persisted, so a download resumed tomorrow still takes
/// the route it was started on.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum ProxyChoice {
    /// Whatever Options > Proxy/Socks says, including later changes to it.
    #[default]
    Default,
    /// Straight to the origin, whatever Options says.
    Direct,
    /// This download's own proxy, as a full specification —
    /// `socks5://user:pass@host:port`.
    Custom(String),
}

impl ProxyChoice {
    /// Which of the three this is, for the dialogs' picker.
    pub fn pick(&self) -> ProxyPick {
        match self {
            ProxyChoice::Default => ProxyPick::Default,
            ProxyChoice::Direct => ProxyPick::Direct,
            ProxyChoice::Custom(_) => ProxyPick::Custom,
        }
    }

    /// The specification the user typed, empty for the other two.
    pub fn spec(&self) -> &str {
        match self {
            ProxyChoice::Custom(s) => s,
            _ => "",
        }
    }

    /// Rebuild from what a dialog holds: a picker value and the address box
    /// beside it, which keeps its text while the picker is on something else
    /// so switching back does not retype it.
    pub fn from_parts(pick: ProxyPick, spec: &str) -> Self {
        match pick {
            ProxyPick::Default => ProxyChoice::Default,
            ProxyPick::Direct => ProxyChoice::Direct,
            ProxyPick::Custom => ProxyChoice::Custom(spec.trim().to_string()),
        }
    }
}

/// The three options a per-download proxy picker offers.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default, Serialize, Deserialize)]
pub enum ProxyPick {
    #[default]
    Default,
    Direct,
    Custom,
}

impl ProxyPick {
    pub const ALL: [ProxyPick; 3] = [ProxyPick::Default, ProxyPick::Direct, ProxyPick::Custom];
}

impl std::fmt::Display for ProxyPick {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&match self {
            ProxyPick::Default => crate::i18n::tr("Default (Options)"),
            ProxyPick::Direct => crate::i18n::tr("No proxy"),
            ProxyPick::Custom => crate::i18n::tr("This download only"),
        })
    }
}

/// What "when done" should do to the machine — a queue's
/// [`Schedule::shutdown_when_done`] or a download's
/// [`DownloadItem::shutdown_after`].
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default, Serialize, Deserialize)]
pub enum PowerAction {
    #[default]
    Shutdown,
    LogOff,
    Sleep,
}

impl PowerAction {
    /// Whether the action takes the login session with it. Shutting down and
    /// logging off end the process either way, so Hydra exits with them;
    /// sleeping only suspends the machine, and Hydra keeps running and is
    /// still there when it wakes.
    pub fn ends_session(self) -> bool {
        matches!(self, PowerAction::Shutdown | PowerAction::LogOff)
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SiteLogin {
    pub site: String,
    pub user: String,
    pub pass: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SoundRow {
    pub event: String,
    pub enabled: bool,
    pub file: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default)]
pub struct Settings {
    // General tab
    pub launch_on_startup: bool,
    /// Ask the release API for a newer version when the app starts; a hit
    /// opens the update dialog. Only the check is automatic — downloading
    /// and installing always wait for the user's "Update Now".
    pub check_updates_on_startup: bool,
    /// Update checks also consider `-rc` pre-releases: a release candidate
    /// ahead of the stable release is offered, otherwise stable is.
    pub beta_channel: bool,
    /// Autostart launches come up in the tray without opening the window.
    pub start_in_tray: bool,
    /// Closing the main window leaves Hydra running in the system tray
    /// (default) instead of quitting, so queues and transfers carry on.
    /// Off: the close button ends the session the way File > Exit does.
    /// Ignored when no tray icon could be installed — without one there
    /// would be no way back into the app, so closing always quits then.
    pub close_to_tray: bool,
    /// Fewer wakeups everywhere: slower UI refresh, no glide animation,
    /// coarser engine ticks. Transfer speed is unaffected.
    pub power_save: bool,
    /// Hide the app from the Dock (macOS, activation policy Accessory) or
    /// the taskbar (Windows, skip_taskbar on each window). The tray icon
    /// remains the way back in. Not offered on Linux: neither Wayland nor
    /// iced/winit's X11 path exposes a skip-taskbar control.
    pub hide_from_taskbar: bool,
    /// GPU (wgpu) rendering instead of the default software raster; smoother
    /// on very large windows at the cost of a much larger memory baseline.
    pub gpu_render: bool,
    pub monitor_clipboard: bool,
    pub capture_browsers: Vec<(String, bool)>,
    // File types tab
    pub auto_types: String,
    /// Whether the model/dataset formats have already been offered to this
    /// install. See [`seed_ai_formats`] — it is a one-time additive migration,
    /// and this is what stops it from undoing a user's later edits.
    ///
    /// The field-level `default` is load-bearing and NOT redundant with the
    /// one on the struct: the struct's fills a missing field from
    /// `Settings::default()`, which says `true` because a fresh config already
    /// lists these formats. A config written before the flag existed would
    /// then read as "already seeded" and the migration would never run — the
    /// exact installs it exists for. Field-level wins, and gives `false`.
    #[serde(default)]
    pub ai_formats_seeded: bool,
    pub dont_start_sites: String,
    pub addr_exceptions: Vec<String>,
    pub show_exception_dialog: bool,
    // Save to tab
    pub remember_last_dir: bool,
    pub server_file_date: bool,
    /// Off (default): a download lands in its category's folder, so a fresh
    /// install fills `~/Downloads` with `Video/`, `Documents/`, ... On: the
    /// per-category folders are ignored and everything is saved straight
    /// into the General category's folder (`~/Downloads` out of the box).
    /// Only new downloads are affected; items already on the list keep the
    /// folder they were added with.
    pub no_category_dirs: bool,
    // Downloads tab
    pub show_file_info_dialog: bool,
    /// Start pulling bytes while the Download File Info dialog is open.
    pub bg_download: bool,
    pub start_minimized: bool,
    pub show_speed_tab: bool,
    pub show_completion_tab: bool,
    pub show_hide_buttons: bool,
    pub show_complete_dialog: bool,
    /// Take a download off the list once it has finished — after the
    /// complete dialog is closed, when that dialog is enabled. Only the row
    /// goes; the downloaded file stays where it was saved.
    pub remove_completed: bool,
    pub user_agent: String,
    pub virus_scanner: String,
    pub virus_args: String,
    // Connection tab
    pub default_conns: usize,
    /// Measure-and-adapt connection count: the transfer starts at ONE
    /// connection and the in-band ramp (`hya_core::ramp`) admits more only
    /// while the aggregate rate says they pay for themselves, with
    /// `default_conns` as the ceiling. The project's own measurements found
    /// a fixed multi-connection setting slower than a single stream on four
    /// of five live objects (saturated links divide, they do not add), so
    /// this defaults ON.
    pub adaptive_conns: bool,
    pub conn_exceptions: Vec<(String, usize)>,
    pub dl_limit_enabled: bool,
    pub dl_limit_mb: u64,
    pub dl_limit_hours: u64,
    pub warn_before_stop: bool,
    // Proxy tab
    pub proxy_mode: ProxyMode,
    pub proxy_script: String,
    /// Address of the manual proxy: a host, or a full `socks5://host:port`
    /// spec pasted from whatever published it.
    pub proxy_host: String,
    pub proxy_port: String,
    pub proxy_user: String,
    pub proxy_pass: String,
    /// What that address speaks. Configs written before the setting existed
    /// deserialize as `Http`, which is what they were treated as.
    pub proxy_type: ProxyType,
    pub ftp_pasv: bool,
    // Sites logins
    pub logins: Vec<SiteLogin>,
    // Sounds
    pub sounds: Vec<SoundRow>,
    // View state
    /// Light, Dark, or System (follow the OS) — View > Theme. `None` marks a
    /// config written before the setting existed; [`load_config`] resolves it
    /// from the legacy `dark_mode` flag, so it is `Some` while running.
    pub theme_mode: Option<ThemeMode>,
    /// The old View > Dark Mode support checkbox. Read once at load, folded
    /// into `theme_mode`, and dropped from the file on the next save — an
    /// upgrade must not silently move a user off the palette they picked.
    pub dark_mode: Option<bool>,
    pub show_categories: bool,
    pub font_size: u16,
    /// Global cap from Downloads > Speed Limiter, bytes/sec.
    pub global_speed_limit: Option<u64>,
    pub speed_limiter_on: bool,
    /// Download-table column widths (drag the header dividers).
    pub column_widths: Vec<f32>,
    /// Last main-window size; restored on start when still sensible.
    pub window_size: Option<(f32, f32)>,
}

impl Default for Settings {
    fn default() -> Self {
        Settings {
            launch_on_startup: true,
            check_updates_on_startup: true,
            beta_channel: false,
            start_in_tray: true,
            close_to_tray: true,
            power_save: false,
            hide_from_taskbar: false,
            gpu_render: false,
            monitor_clipboard: false,
            capture_browsers: [
                "Apple Safari",
                "Google Chrome",
                "Microsoft Edge",
                "Mozilla Firefox",
                "Opera",
            ]
            .iter()
            .map(|b| (b.to_string(), true))
            .collect(),
            auto_types: default_auto_types(),
            // A fresh config already has them, so there is nothing to add.
            ai_formats_seeded: true,
            dont_start_sites: "*.update.microsoft.com download.windowsupdate.com".into(),
            addr_exceptions: vec![],
            show_exception_dialog: true,
            remember_last_dir: true,
            server_file_date: false,
            no_category_dirs: false,
            show_file_info_dialog: true,
            bg_download: true,
            start_minimized: false,
            show_speed_tab: true,
            show_completion_tab: true,
            show_hide_buttons: true,
            show_complete_dialog: true,
            remove_completed: false,
            user_agent: format!("hydra-gui/{}", env!("CARGO_PKG_VERSION")),
            virus_scanner: String::new(),
            virus_args: String::new(),
            // Default: 8 connections; the scheduler settles well at this
            // order on live origins.
            default_conns: 8,
            adaptive_conns: true,
            conn_exceptions: vec![],
            dl_limit_enabled: false,
            dl_limit_mb: 200,
            dl_limit_hours: 5,
            warn_before_stop: true,
            proxy_mode: ProxyMode::None,
            proxy_script: String::new(),
            proxy_host: String::new(),
            proxy_port: String::new(),
            proxy_user: String::new(),
            proxy_pass: String::new(),
            proxy_type: ProxyType::default(),
            ftp_pasv: false,
            logins: vec![],
            sounds: [
                "Download complete",
                "Download failed",
                "Queue processing started",
                "Queue processing stopped/finished",
            ]
            .iter()
            .map(|e| SoundRow {
                event: e.to_string(),
                enabled: false,
                file: String::new(),
            })
            .collect(),
            theme_mode: None,
            dark_mode: None,
            show_categories: true,
            font_size: 13,
            global_speed_limit: None,
            speed_limiter_on: false,
            column_widths: vec![],
            window_size: None,
        }
    }
}

impl Settings {
    /// The palette View > Theme is set to. A config that predates the
    /// setting reads as [`ThemeMode::System`] here only if it also carried no
    /// `dark_mode` flag — [`load_config`] folds that one in first.
    pub fn theme(&self) -> ThemeMode {
        self.theme_mode.unwrap_or_default()
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default)]
pub struct Schedule {
    /// One-time downloading vs periodic synchronization.
    pub periodic: bool,
    pub start_on_startup: bool,
    pub start_enabled: bool,
    /// `23:00` wall-clock.
    pub start_at: String,
    pub once: bool,
    pub days: [bool; 7],
    pub stop_enabled: bool,
    pub stop_at: String,
    pub retries_enabled: bool,
    pub retries: u32,
    pub open_file_enabled: bool,
    pub open_file: String,
    pub exit_when_done: bool,
    /// Shut down, log off or sleep the computer once every file in the queue
    /// has downloaded — checked in the same "when done" pass as
    /// `exit_when_done`. The action itself only runs after the cancellable
    /// countdown in `WinKind::Power`.
    #[serde(default)]
    pub shutdown_when_done: bool,
    #[serde(default)]
    pub shutdown_action: PowerAction,
}

impl Default for Schedule {
    fn default() -> Self {
        Schedule {
            periodic: false,
            start_on_startup: false,
            start_enabled: false,
            start_at: "23:00".into(),
            once: false,
            days: [true; 7],
            stop_enabled: false,
            stop_at: "07:30".into(),
            retries_enabled: false,
            retries: 10,
            open_file_enabled: false,
            open_file: String::new(),
            exit_when_done: false,
            shutdown_when_done: false,
            shutdown_action: PowerAction::default(),
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct QueueDef {
    pub name: String,
    pub files_at_once: u32,
    pub schedule: Schedule,
    /// The two stock queues cannot be renamed or deleted.
    #[serde(default)]
    pub builtin: bool,
    /// Folder-icon colour, packed `0xRRGGBB`, chosen when the queue is
    /// created so each user queue is told apart at a glance. `None` is the
    /// stock yellow folder of the two built-in queues (and of queues saved
    /// before the field existed).
    #[serde(default)]
    pub color: Option<u32>,
    #[serde(skip)]
    pub running: bool,
    /// Whether this run of the queue actually started at least one member.
    /// Gates the "queue finished" actions (sound, open file, exit): a queue
    /// started with nothing to do must not fire `exit_when_done`.
    #[serde(skip)]
    pub did_work: bool,
}

pub fn default_queues() -> Vec<QueueDef> {
    ["Main download queue", "Synchronization queue"]
        .iter()
        .map(|n| QueueDef {
            name: n.to_string(),
            files_at_once: 4,
            schedule: Schedule::default(),
            builtin: true,
            color: None,
            running: false,
            did_work: false,
        })
        .collect()
}

/// Folder colours handed to new queues: distinct hues that read on both the
/// light and the dark window, none of them the stock yellow.
pub const QUEUE_COLORS: [u32; 8] = [
    0x4C8DFF, // blue
    0x3DB56A, // green
    0xE05D5D, // red
    0xA66CFF, // violet
    0xFF8A3D, // orange
    0x2BB5C9, // teal
    0xE64FA0, // pink
    0x8C7A5B, // brown
];

/// A colour for a queue about to be created: random among the palette
/// entries no existing queue uses, so two new queues never come out alike
/// until the palette is exhausted; then random over the whole palette.
pub fn pick_queue_color(existing: &[QueueDef]) -> u32 {
    let free: Vec<u32> = QUEUE_COLORS
        .iter()
        .copied()
        .filter(|c| !existing.iter().any(|q| q.color == Some(*c)))
        .collect();
    let pool: &[u32] = if free.is_empty() {
        &QUEUE_COLORS
    } else {
        &free
    };
    // Enough randomness for a colour: the nanosecond clock, not a crypto
    // source, and no dependency for it.
    let seed = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.subsec_nanos() as usize)
        .unwrap_or(0);
    pool[seed % pool.len()]
}

/// The `--config DIR` the app was started with, absolute, when one was
/// given. Written once in `main` before anything reads [`app_dir`].
static APP_DIR_OVERRIDE: std::sync::OnceLock<PathBuf> = std::sync::OnceLock::new();

/// Run out of `dir` instead of the platform application directory. Called
/// from `main` for `--config DIR`, before the first [`app_dir`] read; a
/// second call is ignored, since half the process would already be pointing
/// at the first answer.
pub fn set_app_dir(dir: PathBuf) {
    let _ = APP_DIR_OVERRIDE.set(dir);
}

/// The `--config DIR` in force, if any.
///
/// Callers that hand this instance's identity to some OTHER process — the
/// login item that relaunches it, the native-messaging host the browser
/// spawns — have to know the directory is not the one that process would
/// otherwise assume.
pub fn app_dir_override() -> Option<&'static std::path::Path> {
    APP_DIR_OVERRIDE.get().map(|p| p.as_path())
}

/// The application directory holding `config.toml`, `state.redb`,
/// `locales/` and `logs/`.
///
/// `--config DIR` moves all of it, so a portable install (a USB stick, a
/// second profile) keeps its settings and download list beside itself.
/// Without the flag: deliberately NOT `dirs::config_dir()` everywhere — on
/// macOS that resolves to `~/Library/Application Support`, and hydra's
/// convention (shared with the CLI) is `~/.config/hydra` on both Linux and
/// macOS. Windows uses `%APPDATA%\hydra`
/// (`Users\{user}\AppData\Roaming\hydra`).
pub fn app_dir() -> PathBuf {
    if let Some(dir) = APP_DIR_OVERRIDE.get() {
        return dir.clone();
    }
    #[cfg(target_os = "windows")]
    {
        dirs::config_dir()
            .unwrap_or_else(std::env::temp_dir)
            .join("hydra")
    }
    #[cfg(not(target_os = "windows"))]
    {
        dirs::home_dir()
            .unwrap_or_else(std::env::temp_dir)
            .join(".config")
            .join("hydra")
    }
}

/// User configuration: everything the Options/Scheduler dialogs edit.
/// Stored as `config.toml`; a missing or unparsable file yields defaults.
/// (action id, default combo, English label) — the shortcut table.
pub const SHORTCUT_ACTIONS: [(&str, &str, &str); 12] = [
    ("add_url", "cmd+n", "Add new download"),
    (
        "clipboard_add",
        "cmd+shift+v",
        "Add URL from clipboard and start",
    ),
    ("options", "cmd+,", "Options"),
    ("scheduler", "cmd+e", "Scheduler"),
    ("resume_last", "cmd+r", "Resume last unfinished download"),
    ("stop_last", "cmd+s", "Stop last active download"),
    ("start_main_queue", "cmd+shift+r", "Start main queue"),
    ("stop_main_queue", "cmd+shift+s", "Stop main queue"),
    ("select_all", "cmd+a", "Select all downloads"),
    (
        "remove_selected",
        "cmd+alt+r",
        "Remove selected downloads from the list",
    ),
    ("close_window", "cmd+w", "Close window"),
    ("quit", "cmd+q", "Exit Hydra"),
];

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct ConfigFile {
    pub language: Option<String>,
    /// action id -> combo ("cmd+shift+v"); `cmd` is ⌘ on macOS, Ctrl
    /// elsewhere. Editable in Help > Keyboard Shortcuts.
    pub shortcuts: std::collections::BTreeMap<String, String>,
    /// Log verbosity: debug | info | warn | error (default info).
    pub log_level: Option<String>,
    /// "software" (default; ~10x lighter in memory) or "gpu".
    pub renderer: Option<String>,
    pub settings: Settings,
    pub categories: Vec<CategoryDef>,
    pub queues: Vec<QueueDef>,
}

/// Rolling accounting for the Connection tab's "Download no more than N
/// MBytes every H hours".
///
/// Lives in the state file rather than in [`Settings`], deliberately: the
/// Options dialog edits a *clone* of the settings and writes the whole clone
/// back on OK, so a counter kept there would be rewound to whatever it read
/// when the dialog opened — silently refunding every byte transferred while
/// the dialog was on screen.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct DlQuota {
    /// Bytes transferred inside the current window.
    pub used: u64,
    /// Unix seconds the window opened; 0 = no window running yet. The window
    /// starts at the first accounted byte, not at app start, so an idle
    /// Hydra does not burn through periods it never downloaded in.
    pub window_start: i64,
}

/// Runtime state: the download list. Kept separate from configuration so a
/// hand-edited (or broken) config never touches the user's download history.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default)]
pub struct StateFile {
    pub next_id: DlId,
    pub downloads: Vec<DownloadItem>,
    pub dl_quota: DlQuota,
}

impl Default for StateFile {
    fn default() -> Self {
        StateFile {
            next_id: 1,
            downloads: vec![],
            dl_quota: DlQuota::default(),
        }
    }
}

pub fn load_config() -> ConfigFile {
    let path = app_dir().join("config.toml");
    let mut cfg: ConfigFile = match std::fs::read_to_string(&path) {
        Ok(text) => toml::from_str(&text).unwrap_or_else(|e| {
            crate::log::warn(&format!("config.toml unparsable ({e}); using defaults"));
            ConfigFile::default()
        }),
        Err(_) => ConfigFile::default(),
    };
    if cfg.categories.is_empty() {
        cfg.categories = default_categories();
    }
    if cfg.queues.is_empty() {
        cfg.queues = default_queues();
    }
    for (id, combo, _) in SHORTCUT_ACTIONS {
        cfg.shortcuts
            .entry(id.to_string())
            .or_insert_with(|| combo.to_string());
    }
    // Configs written before the flag existed: stock names are stock queues.
    for q in &mut cfg.queues {
        if q.name == "Main download queue" || q.name == "Synchronization queue" {
            q.builtin = true;
        }
    }
    if cfg.settings.font_size == 0 {
        cfg.settings = Settings::default();
    }
    migrate_theme_mode(&mut cfg.settings);
    seed_ai_formats(&mut cfg);
    cfg
}

/// Add the model and dataset formats to an install that predates them.
///
/// Changing the DEFAULTS only ever reaches a fresh install: `auto_types` and
/// the category list are both written to `config.toml` in full, so an existing
/// user would never see the new formats no matter what the defaults said.
///
/// Additive and one-time. It appends the types the list does not already have
/// and adds the category if it is absent, then records that it has run — so a
/// user who afterwards trims the list or deletes the category does not find
/// either restored on the next launch.
fn seed_ai_formats(cfg: &mut ConfigFile) {
    if cfg.settings.ai_formats_seeded {
        return;
    }
    cfg.settings.ai_formats_seeded = true;

    let listed: std::collections::HashSet<String> = cfg
        .settings
        .auto_types
        .split_whitespace()
        .map(|t| t.to_ascii_uppercase())
        .collect();
    let missing: Vec<&str> = AI_TYPES
        .iter()
        .copied()
        .filter(|t| !listed.contains(*t))
        .collect();
    if !missing.is_empty() {
        // Appended rather than merged and re-sorted: the field is a set, and
        // rewriting the order a user typed is not this migration's business.
        let sep = if cfg.settings.auto_types.trim().is_empty() {
            ""
        } else {
            " "
        };
        cfg.settings.auto_types = format!(
            "{}{sep}{}",
            cfg.settings.auto_types.trim_end(),
            missing.join(" ")
        );
    }
    if !cfg.categories.iter().any(|c| c.name == AI_CATEGORY) {
        cfg.categories.push(CategoryDef {
            name: AI_CATEGORY.into(),
            exts: AI_TYPES.iter().map(|e| e.to_ascii_lowercase()).collect(),
            dir: sub(AI_CATEGORY),
        });
    }
    crate::log::info(&format!(
        "config: seeded {} model/dataset formats",
        missing.len()
    ));
}

/// View > Theme replaced the Dark Mode checkbox: a config written before it
/// carries only `dark_mode`, and that pick stands. Only a config with neither
/// key — a fresh install — starts out following the OS. The legacy key is
/// dropped either way, so the next save writes just `theme_mode`.
fn migrate_theme_mode(s: &mut Settings) {
    if s.theme_mode.is_none() {
        s.theme_mode = Some(match s.dark_mode {
            Some(true) => ThemeMode::Dark,
            Some(false) => ThemeMode::Light,
            None => ThemeMode::System,
        });
    }
    s.dark_mode = None;
}

pub fn save_config(cfg: &ConfigFile) {
    let dir = app_dir();
    let _ = std::fs::create_dir_all(&dir);
    if let Ok(s) = toml::to_string_pretty(cfg) {
        let _ = std::fs::write(dir.join("config.toml"), s);
    }
}

// ---------------------------------------------------------------- state db
//
// The download list lives in an embedded redb database (`state.redb`):
// crash-safe transactions and room to grow to many thousands of entries.
// A `gui-state.json` from earlier builds is migrated in on first load.

const DOWNLOADS: redb::TableDefinition<u64, &[u8]> = redb::TableDefinition::new("downloads");
/// Small singleton records that are not download rows. Only the download
/// limit's window counter lives here today; it is kept out of `config.toml`
/// on purpose (it moves while transfers run, and that file is hand-editable)
/// and out of the downloads table so saving it does not rewrite every row.
const META: redb::TableDefinition<&str, &[u8]> = redb::TableDefinition::new("meta");
const QUOTA_KEY: &str = "dl_quota";

fn state_db() -> Option<&'static redb::Database> {
    use std::sync::OnceLock;
    static DB: OnceLock<Option<redb::Database>> = OnceLock::new();
    DB.get_or_init(|| {
        let dir = app_dir();
        let _ = std::fs::create_dir_all(&dir);
        let path = dir.join("state.redb");
        match redb::Database::create(&path) {
            Ok(db) => Some(db),
            Err(first) => {
                // A database written by an older redb (file format 2) cannot
                // be opened by redb 4: set it aside and start fresh — the
                // JSON snapshot from the original migration re-seeds below.
                crate::log::warn(&format!("state db needs recovery: {first}"));
                let _ = std::fs::rename(&path, dir.join("state.redb.old"));
                match redb::Database::create(&path) {
                    Ok(db) => Some(db),
                    Err(e) => {
                        crate::log::error(&format!("state db unavailable: {e}"));
                        None
                    }
                }
            }
        }
    })
    .as_ref()
}

pub fn load_state() -> StateFile {
    let mut st = StateFile::default();
    let mut loaded = false;
    if let Some(db) = state_db() {
        use redb::ReadableDatabase;
        if let Ok(txn) = db.begin_read() {
            if let Ok(table) = txn.open_table(DOWNLOADS) {
                use redb::ReadableTable;
                if let Ok(iter) = table.iter() {
                    for row in iter.flatten() {
                        if let Ok(d) = serde_json::from_slice::<DownloadItem>(row.1.value()) {
                            st.next_id = st.next_id.max(d.id + 1);
                            st.downloads.push(d);
                        }
                    }
                    loaded = true;
                }
            }
        }
    }
    // One-time migration from the JSON era — the live file first, then the
    // snapshot kept from a previous migration (used again after db recovery).
    if !loaded || st.downloads.is_empty() {
        for name in ["gui-state.json", "gui-state.json.migrated"] {
            let json = app_dir().join(name);
            if !json.exists() {
                continue;
            }
            if let Some(old) = std::fs::read(&json)
                .ok()
                .and_then(|b| serde_json::from_slice::<StateFile>(&b).ok())
            {
                st = old;
                save_state(&st);
                if name == "gui-state.json" {
                    let _ = std::fs::rename(&json, app_dir().join("gui-state.json.migrated"));
                }
                crate::log::info(&format!("migrated {name} into state.redb"));
                break;
            }
        }
    }
    if let Some(db) = state_db() {
        use redb::ReadableDatabase;
        if let Ok(txn) = db.begin_read() {
            if let Ok(table) = txn.open_table(META) {
                if let Ok(Some(row)) = table.get(QUOTA_KEY) {
                    if let Ok(q) = serde_json::from_slice::<DlQuota>(row.value()) {
                        st.dl_quota = q;
                    }
                }
            }
        }
    }
    // A transfer that was live when the process died is not live now.
    for d in &mut st.downloads {
        if d.state.is_active() {
            d.state = DlState::Paused;
        }
        d.disp_progress = d.progress();
    }
    st.downloads.sort_by_key(|d| d.id);
    st
}

pub fn save_state(st: &StateFile) {
    let Some(db) = state_db() else { return };
    let Ok(txn) = db.begin_write() else { return };
    {
        let _ = txn.delete_table(DOWNLOADS);
        if let Ok(mut table) = txn.open_table(DOWNLOADS) {
            for d in &st.downloads {
                if let Ok(bytes) = serde_json::to_vec(d) {
                    let _ = table.insert(d.id, bytes.as_slice());
                }
            }
        }
    }
    if let Err(e) = txn.commit() {
        crate::log::error(&format!("state save failed: {e}"));
    }
    save_quota(&st.dl_quota);
}

/// Persist the download-limit window on its own. The counter moves while
/// transfers run, and [`save_state`] rewrites the whole downloads table —
/// this is the one-row write a per-second update can afford.
pub fn save_quota(q: &DlQuota) {
    let Some(db) = state_db() else { return };
    let Ok(txn) = db.begin_write() else { return };
    {
        if let (Ok(mut table), Ok(bytes)) = (txn.open_table(META), serde_json::to_vec(q)) {
            let _ = table.insert(QUOTA_KEY, bytes.as_slice());
        }
    }
    if let Err(e) = txn.commit() {
        crate::log::error(&format!("download-limit counter save failed: {e}"));
    }
}

#[cfg(test)]
mod tests {
    /// The load-bearing half of the migration flag.
    ///
    /// `Settings` carries `#[serde(default)]`, which fills a missing field
    /// from `Settings::default()` — and that says `true`, because a fresh
    /// config already lists the model formats. Without the field's OWN
    /// `#[serde(default)]` an upgraded install would read as "already seeded"
    /// and never receive them, which is the only case the migration exists for.
    #[test]
    fn a_config_written_before_the_flag_reads_as_unseeded() {
        let old: Settings = toml::from_str("font_size = 13").expect("parse");
        assert!(!old.ai_formats_seeded, "an old config must still be seeded");
        assert!(
            Settings::default().ai_formats_seeded,
            "a fresh one must not"
        );
    }

    fn legacy_config() -> ConfigFile {
        let mut cfg = ConfigFile {
            settings: Settings {
                auto_types: "ZIP EXE MP4".into(),
                ai_formats_seeded: false,
                ..Settings::default()
            },
            categories: default_categories(),
            ..ConfigFile::default()
        };
        cfg.categories.retain(|c| c.name != AI_CATEGORY);
        cfg
    }

    #[test]
    fn the_model_formats_reach_an_install_that_predates_them() {
        let mut cfg = legacy_config();
        seed_ai_formats(&mut cfg);

        let listed: Vec<&str> = cfg.settings.auto_types.split_whitespace().collect();
        for want in ["SAFETENSORS", "GGUF", "H5", "PQ", "PARQUET", "ONNX"] {
            assert!(listed.contains(&want), "{want} missing from {listed:?}");
        }
        // What was already there survives.
        for kept in ["ZIP", "EXE", "MP4"] {
            assert!(listed.contains(&kept), "{kept} was dropped");
        }
        assert!(cfg.categories.iter().any(|c| c.name == AI_CATEGORY));
        assert!(cfg.settings.ai_formats_seeded);
    }

    #[test]
    fn seeding_twice_changes_nothing_the_second_time() {
        let mut cfg = legacy_config();
        seed_ai_formats(&mut cfg);
        let once = cfg.clone();
        seed_ai_formats(&mut cfg);
        assert_eq!(cfg.settings.auto_types, once.settings.auto_types);
        assert_eq!(cfg.categories.len(), once.categories.len());
    }

    /// The flag's real job: a user who trims the list afterwards keeps it
    /// trimmed. Restoring it on every launch would make the setting unusable.
    #[test]
    fn a_later_edit_is_not_undone_on_the_next_launch() {
        let mut cfg = legacy_config();
        seed_ai_formats(&mut cfg);
        cfg.settings.auto_types = "ZIP".into();
        cfg.categories.retain(|c| c.name != AI_CATEGORY);
        seed_ai_formats(&mut cfg);
        assert_eq!(cfg.settings.auto_types, "ZIP");
        assert!(!cfg.categories.iter().any(|c| c.name == AI_CATEGORY));
    }

    #[test]
    fn a_type_already_listed_is_not_added_twice() {
        let mut cfg = legacy_config();
        // Lower case, because the list is matched case-insensitively.
        cfg.settings.auto_types = "zip safetensors".into();
        seed_ai_formats(&mut cfg);
        let n = cfg
            .settings
            .auto_types
            .split_whitespace()
            .filter(|t| t.eq_ignore_ascii_case("safetensors"))
            .count();
        assert_eq!(n, 1, "in {:?}", cfg.settings.auto_types);
    }

    #[test]
    fn model_and_dataset_files_land_in_the_ai_category() {
        let cats = default_categories();
        for f in [
            "model.safetensors",
            "llama-3-8b.Q4_K_M.gguf",
            "TEP_Mode1.h5",
            "train.parquet",
            "shard.pq",
            "net.onnx",
            "weights.ckpt",
        ] {
            assert_eq!(
                categorize(f, &cats).as_deref(),
                Some(AI_CATEGORY),
                "{f} was filed wrong"
            );
        }
        // The formats deliberately left out stay where they were.
        assert_eq!(
            categorize("export.csv", &cats).as_deref(),
            Some("Documents")
        );
    }

    use super::*;

    /// The Options picker lists `ProxyType::ALL` and routing reads
    /// `scheme()`: a type missing from either is a protocol the user can
    /// never choose, or one that resolves as something else.
    #[test]
    fn every_proxy_type_is_offered_and_names_its_own_scheme() {
        assert_eq!(ProxyType::ALL.len(), 4);
        for t in ProxyType::ALL {
            let spec = format!("{}://127.0.0.1:1080", t.scheme());
            let px = hya_net::Proxy::parse(&spec).expect("a scheme the transport knows");
            assert_eq!(px.kind.as_str(), t.scheme());
            assert!(
                t.to_string().eq_ignore_ascii_case(t.scheme()),
                "the label and the scheme must name the same protocol: {t}"
            );
        }
    }

    /// The dialogs hold a picker and an address box; the item holds one
    /// value. Round-tripping between them must not lose the address when the
    /// picker moves off "this download only" and back.
    #[test]
    fn a_per_download_proxy_round_trips_between_the_dialog_and_the_item() {
        let own = ProxyChoice::Custom("socks5://127.0.0.1:10808".into());
        assert_eq!(own.pick(), ProxyPick::Custom);
        assert_eq!(own.spec(), "socks5://127.0.0.1:10808");
        assert_eq!(ProxyChoice::from_parts(own.pick(), own.spec()), own);

        // The other two carry no address, whatever is still in the box.
        assert_eq!(
            ProxyChoice::from_parts(ProxyPick::Default, "socks5://h:1"),
            ProxyChoice::Default
        );
        assert_eq!(
            ProxyChoice::from_parts(ProxyPick::Direct, "socks5://h:1"),
            ProxyChoice::Direct
        );
        assert_eq!(ProxyChoice::default().spec(), "");
        assert_eq!(ProxyChoice::Direct.pick(), ProxyPick::Direct);
    }

    /// A config written before the proxy settings worked must still load, and
    /// must not silently acquire a proxy: the keys it carries are gone, and
    /// the ones that replaced them default to "no proxy".
    #[test]
    fn a_config_from_before_the_proxy_settings_worked_still_loads() {
        let old = r#"
            proxy_mode = "Manual"
            proxy_host = "127.0.0.1"
            proxy_port = "10808"
            proxy_http = true
            proxy_https = false
            proxy_ftp = false
            font_size = 13
        "#;
        let s: Settings = toml::from_str(old).expect("an old config still parses");
        assert_eq!(s.proxy_mode, ProxyMode::Manual);
        assert_eq!(s.proxy_host, "127.0.0.1");
        assert_eq!(s.proxy_type, ProxyType::Http);
    }

    #[test]
    fn a_new_queue_gets_a_colour_no_other_queue_has() {
        let mut queues = default_queues();
        assert!(
            queues.iter().all(|q| q.color.is_none()),
            "stock queues stay yellow"
        );
        for _ in 0..QUEUE_COLORS.len() {
            let c = pick_queue_color(&queues);
            assert!(QUEUE_COLORS.contains(&c));
            assert!(
                !queues.iter().any(|q| q.color == Some(c)),
                "{c:06X} handed out twice"
            );
            let mut q = queues[0].clone();
            q.color = Some(c);
            queues.push(q);
        }
        // Palette exhausted: still a palette colour, never a crash.
        assert!(QUEUE_COLORS.contains(&pick_queue_color(&queues)));
    }

    fn cats() -> Vec<CategoryDef> {
        vec![
            CategoryDef {
                name: "General".into(),
                exts: vec![],
                dir: "/dl".into(),
            },
            CategoryDef {
                name: "Video".into(),
                exts: vec!["mp4".into()],
                dir: "/dl/Video".into(),
            },
        ]
    }

    #[test]
    fn theme_mode_takes_over_from_the_dark_mode_flag() {
        let mut old: Settings = toml::from_str("dark_mode = true").unwrap();
        migrate_theme_mode(&mut old);
        assert_eq!(old.theme(), ThemeMode::Dark);
        // The flag is gone from the file after the next save.
        assert_eq!(old.dark_mode, None);
        assert!(!toml::to_string(&old).unwrap().contains("dark_mode"));

        let mut old: Settings = toml::from_str("dark_mode = false").unwrap();
        migrate_theme_mode(&mut old);
        assert_eq!(old.theme(), ThemeMode::Light);

        // Neither key: a fresh install follows the OS.
        let mut fresh: Settings = toml::from_str("").unwrap();
        migrate_theme_mode(&mut fresh);
        assert_eq!(fresh.theme(), ThemeMode::System);

        // A hand-edited config carrying both: the explicit setting wins.
        let mut both: Settings =
            toml::from_str("theme_mode = \"System\"\ndark_mode = true").unwrap();
        migrate_theme_mode(&mut both);
        assert_eq!(both.theme(), ThemeMode::System);
    }

    #[test]
    fn category_dir_uses_subfolder_unless_flat() {
        let c = cats();
        assert_eq!(
            category_dir(&c, Some("Video"), false),
            Some("/dl/Video".into())
        );
        // Flat: the category still classifies the file, but the folder is
        // always General's.
        assert_eq!(category_dir(&c, Some("Video"), true), Some("/dl".into()));
        // Uncategorized, and a category that no longer exists.
        assert_eq!(category_dir(&c, None, false), Some("/dl".into()));
        assert_eq!(category_dir(&c, Some("Gone"), false), None);
        assert_eq!(category_dir(&c, Some("Gone"), true), Some("/dl".into()));
        assert_eq!(category_dir(&[], Some("Video"), false), None);
    }

    #[test]
    fn only_sleeping_leaves_the_session_up() {
        // Shutdown and log off take the process with them, so Hydra exits
        // alongside them; sleep suspends the machine and Hydra is still
        // there on wake.
        assert!(PowerAction::Shutdown.ends_session());
        assert!(PowerAction::LogOff.ends_session());
        assert!(!PowerAction::Sleep.ends_session());
    }

    #[test]
    fn a_config_written_before_sleep_existed_still_loads() {
        // The stored variant name is the wire format: adding Sleep must not
        // move Shutdown or LogOff.
        let sc: Schedule = toml::from_str(
            "periodic = false\nstart_on_startup = false\nstart_enabled = false\n\
             start_at = \"23:00\"\nonce = false\ndays = [true, true, true, true, true, true, true]\n\
             stop_enabled = false\nstop_at = \"07:30\"\nretries_enabled = false\nretries = 10\n\
             open_file_enabled = false\nopen_file = \"\"\nexit_when_done = false\n\
             shutdown_when_done = true\nshutdown_action = \"LogOff\"\n",
        )
        .unwrap();
        assert_eq!(sc.shutdown_action, PowerAction::LogOff);
        // A schedule saved before the field existed at all defaults to
        // shutting down, as it always did.
        let bare: Schedule = toml::from_str("shutdown_when_done = true\n").unwrap();
        assert_eq!(bare.shutdown_action, PowerAction::Shutdown);
        assert!(toml::to_string(&Schedule {
            shutdown_action: PowerAction::Sleep,
            ..Schedule::default()
        })
        .unwrap()
        .contains("shutdown_action = \"Sleep\""));
    }
}
