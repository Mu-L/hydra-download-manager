// Copyright (C) 2026 Javad Rajabzadeh
// SPDX-License-Identifier: GPL-3.0-or-later

//! Native-messaging host self-registration.
//!
//! A browser only talks to `hydra-host` if it finds a manifest naming that
//! binary, and every browser looks for it in its own per-user directory (or,
//! on Windows, under its own `HKCU` key). `scripts/install-native-host.sh`
//! writes those by hand, which is fine for a checkout and wrong for an
//! installed application: nobody should have to run a shell script to make
//! the extension work.
//!
//! So the app does it itself, at every start. All of these locations are
//! per-user, so nothing here needs administrator rights, and every write is
//! idempotent — an unchanged manifest is left alone, so this costs a few
//! `stat`s on a normal boot.
//!
//! Only browsers already present on the machine are touched: the profile
//! directory has to exist before a manifest is written into it, so this never
//! creates configuration for a browser the user does not have.
//!
//! Safari is absent on purpose. Its extension talks to the containing app
//! through `SFSafariWebExtensionHandler`, so there is no manifest to write.

use std::path::{Path, PathBuf};
use std::sync::Mutex;

const HOST_NAME: &str = "com.hydra.host";

/// Browsers the host is registered with, as of this session's registration
/// pass, for Options > Extensions to show.
static REGISTERED: Mutex<Vec<String>> = Mutex::new(Vec::new());

/// Names of the browsers whose manifest points at Hydra's host, in the
/// order they were registered. Empty until [`ensure_registered`] has run.
pub fn registered() -> Vec<String> {
    REGISTERED.lock().map(|g| g.clone()).unwrap_or_default()
}

fn set_registered(names: Vec<String>) {
    if let Ok(mut g) = REGISTERED.lock() {
        *g = names;
    }
}

/// A container the app may be running in, which changes what a manifest
/// may point at.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Sandbox {
    None,
    /// The binary lives on a `/tmp/.mount_*` FUSE mount that vanishes with
    /// the process; `scripts/package-appimage.sh` registers a stable shim.
    AppImage,
    /// `/app/bin` is only visible inside the sandbox; the browser outside
    /// needs a wrapper that goes back in through `flatpak run`.
    Flatpak,
}

fn sandbox() -> Sandbox {
    if std::env::var_os("APPIMAGE").is_some() && std::env::var_os("APPDIR").is_some() {
        Sandbox::AppImage
    } else if std::env::var_os("FLATPAK_ID").is_some() || Path::new("/.flatpak-info").exists() {
        Sandbox::Flatpak
    } else {
        Sandbox::None
    }
}

/// The Flatpak application id, as published on Flathub.
const FLATPAK_APP_ID: &str = "io.github.ja7ad.hydra";

/// What the Flatpak wrapper script has to say.
fn flatpak_wrapper_body() -> String {
    format!("#!/bin/sh\nexec flatpak run --command=hydra-host {FLATPAK_APP_ID} \"$@\"\n")
}

/// Write the wrapper the browser can execute from outside the sandbox at
/// `dir/hydra-host`, and hand its path back to register.
fn write_flatpak_wrapper(dir: &Path) -> Option<PathBuf> {
    let path = dir.join("hydra-host");
    let body = flatpak_wrapper_body();
    if std::fs::read_to_string(&path).is_ok_and(|cur| cur == body) {
        return Some(path);
    }
    if let Err(e) = std::fs::create_dir_all(dir) {
        crate::log::warn(&format!("nmhost: cannot create {}: {e}", dir.display()));
        return None;
    }
    if let Err(e) = std::fs::write(&path, body) {
        crate::log::warn(&format!("nmhost: cannot write {}: {e}", path.display()));
        return None;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755));
    }
    crate::log::info(&format!("nmhost: flatpak wrapper at {}", path.display()));
    Some(path)
}

/// The Chromium extension ids Hydra answers to: the dev build (derived from
/// the `key` pinned in `extensions/chrome/manifest.json`; see
/// `scripts/build-extensions.sh`) and the Chrome Web Store listing, which
/// signs with the store's own key and so gets an id of its own.
pub const CHROMIUM_EXT_IDS: [&str; 2] = [
    "jpnonmbbkjdpeebdhkjoliklfhkdcomj",
    "hcjpgdepggimagiehiampmgamlfkpbhh",
];

/// Firefox allow-lists by add-on id, not by an extension origin. Mirrors
/// `browser_specific_settings.gecko.id` in `extensions/firefox/manifest.json`.
const FIREFOX_EXT_ID: &str = "hydra@ja7ad.github.io";

/// Where `hydra-host` lives: next to the running executable. Packaging puts
/// both binaries in the same directory on every platform, and resolving it
/// relative to `current_exe` means a moved or relocated install still points
/// at its own host rather than at whatever was there at install time.
fn host_binary() -> Option<PathBuf> {
    let exe = std::env::current_exe().ok()?;
    let dir = exe.parent()?;
    let name = if cfg!(target_os = "windows") {
        "hydra-host.exe"
    } else {
        "hydra-host"
    };
    let path = dir.join(name);
    path.is_file().then_some(path)
}

/// The two manifest dialects. Chromium allow-lists an extension origin,
/// Firefox an add-on id; everything else about the file is identical.
fn manifest(host_path: &Path, gecko: bool) -> String {
    let allow = if gecko {
        format!("\"allowed_extensions\": [{}]", json_str(FIREFOX_EXT_ID))
    } else {
        let origins: Vec<String> = CHROMIUM_EXT_IDS
            .iter()
            .map(|id| json_str(&format!("chrome-extension://{id}/")))
            .collect();
        format!("\"allowed_origins\": [{}]", origins.join(", "))
    };
    format!(
        "{{\n  \"name\": {},\n  \"description\": \"Hydra Download Manager native host\",\n  \"path\": {},\n  \"type\": \"stdio\",\n  {allow}\n}}\n",
        json_str(HOST_NAME),
        json_str(&host_path.to_string_lossy()),
    )
}

/// Minimal JSON string escaping. The only characters that realistically turn
/// up here are the backslashes of a Windows path.
fn json_str(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

/// Write `body` to `dir/com.hydra.host.json`, but only when `root` exists —
/// that is the test for "this browser is installed for this user". `None`
/// when the browser is absent or the write failed; otherwise whether the
/// file changed (an identical manifest is left alone, mtime and all).
#[cfg(any(not(target_os = "windows"), test))]
fn write_manifest(root: &Path, dir: &Path, body: &str) -> Option<bool> {
    if !root.is_dir() {
        return None;
    }
    let file = dir.join(format!("{HOST_NAME}.json"));
    if std::fs::read_to_string(&file).is_ok_and(|cur| cur == body) {
        return Some(false);
    }
    if let Err(e) = std::fs::create_dir_all(dir) {
        crate::log::warn(&format!("nmhost: cannot create {}: {e}", dir.display()));
        return None;
    }
    match std::fs::write(&file, body) {
        Ok(()) => {
            crate::log::info(&format!("nmhost: registered {}", file.display()));
            Some(true)
        }
        Err(e) => {
            crate::log::warn(&format!("nmhost: cannot write {}: {e}", file.display()));
            None
        }
    }
}

/// One browser's registration point: its name for the Options page, the
/// profile root that decides whether it is installed, where its manifest
/// goes, and which dialect it reads.
#[cfg(not(target_os = "windows"))]
struct Target {
    name: &'static str,
    root: PathBuf,
    dir: PathBuf,
    gecko: bool,
}

/// Every browser this platform knows about.
#[cfg(not(target_os = "windows"))]
fn targets(home: &Path) -> Vec<Target> {
    // Chromium browsers keep NativeMessagingHosts inside the profile root;
    // Firefox uses one shared directory per Mozilla-family application.
    let chromium = |name: &'static str, root: PathBuf| Target {
        name,
        dir: root.join("NativeMessagingHosts"),
        root,
        gecko: false,
    };
    let gecko = |name: &'static str, root: PathBuf, dir: PathBuf| Target {
        name,
        root,
        dir,
        gecko: true,
    };

    #[cfg(target_os = "macos")]
    {
        let sup = home.join("Library/Application Support");
        vec![
            chromium("Chrome", sup.join("Google/Chrome")),
            chromium("Chrome Beta", sup.join("Google/Chrome Beta")),
            chromium("Chromium", sup.join("Chromium")),
            chromium("Edge", sup.join("Microsoft Edge")),
            chromium("Brave", sup.join("BraveSoftware/Brave-Browser")),
            chromium("Vivaldi", sup.join("Vivaldi")),
            chromium("Opera", sup.join("com.operasoftware.Opera")),
            chromium("Arc", sup.join("Arc/User Data")),
            gecko(
                "Firefox",
                home.join("Library/Application Support/Firefox"),
                sup.join("Mozilla/NativeMessagingHosts"),
            ),
            gecko(
                "LibreWolf",
                sup.join("LibreWolf"),
                sup.join("LibreWolf/NativeMessagingHosts"),
            ),
        ]
    }

    #[cfg(target_os = "linux")]
    {
        let cfg = home.join(".config");
        // Snap and Flatpak do not use ~/.config or ~/.mozilla at all: each
        // browser gets its own private tree. Ubuntu has shipped Firefox as a
        // SNAP by default since 22.04, so on a stock Ubuntu the classic
        // paths below match nothing and the extension is left reporting
        // "Hydra is not reachable" with no indication why.
        let snap = home.join("snap");
        let flat = home.join(".var/app");
        vec![
            chromium("Chrome", cfg.join("google-chrome")),
            chromium("Chrome Beta", cfg.join("google-chrome-beta")),
            chromium("Chromium", cfg.join("chromium")),
            chromium("Edge", cfg.join("microsoft-edge")),
            chromium("Brave", cfg.join("BraveSoftware/Brave-Browser")),
            chromium("Vivaldi", cfg.join("vivaldi")),
            chromium("Opera", cfg.join("opera")),
            // Snap Chromium keeps its profile under the snap's own tree.
            chromium("Chromium (snap)", snap.join("chromium/common/chromium")),
            // Flatpak browsers keep theirs under the app id.
            chromium(
                "Chrome (flatpak)",
                flat.join("com.google.Chrome/config/google-chrome"),
            ),
            chromium(
                "Chromium (flatpak)",
                flat.join("org.chromium.Chromium/config/chromium"),
            ),
            chromium(
                "Brave (flatpak)",
                flat.join("com.brave.Browser/config/BraveSoftware/Brave-Browser"),
            ),
            chromium(
                "Edge (flatpak)",
                flat.join("com.microsoft.Edge/config/microsoft-edge"),
            ),
            gecko(
                "Firefox",
                home.join(".mozilla"),
                home.join(".mozilla/native-messaging-hosts"),
            ),
            gecko(
                "LibreWolf",
                home.join(".librewolf"),
                home.join(".librewolf/native-messaging-hosts"),
            ),
            // The Ubuntu default.
            gecko(
                "Firefox (snap)",
                snap.join("firefox/common/.mozilla"),
                snap.join("firefox/common/.mozilla/native-messaging-hosts"),
            ),
            gecko(
                "Firefox (flatpak)",
                flat.join("org.mozilla.firefox/.mozilla"),
                flat.join("org.mozilla.firefox/.mozilla/native-messaging-hosts"),
            ),
            gecko(
                "LibreWolf (flatpak)",
                flat.join("io.gitlab.librewolf-community/.librewolf"),
                flat.join("io.gitlab.librewolf-community/.librewolf/native-messaging-hosts"),
            ),
        ]
    }
}

/// The `HKCU` keys each Windows browser reads. Chromium and Gecko share the
/// shape; only the vendor path differs.
#[cfg(target_os = "windows")]
const WIN_KEYS: &[(&str, &str, bool)] = &[
    (
        "Chrome",
        r"Software\Google\Chrome\NativeMessagingHosts",
        false,
    ),
    ("Chromium", r"Software\Chromium\NativeMessagingHosts", false),
    (
        "Edge",
        r"Software\Microsoft\Edge\NativeMessagingHosts",
        false,
    ),
    (
        "Brave",
        r"Software\BraveSoftware\Brave-Browser\NativeMessagingHosts",
        false,
    ),
    ("Vivaldi", r"Software\Vivaldi\NativeMessagingHosts", false),
    (
        "Opera",
        r"Software\Opera Software\NativeMessagingHosts",
        false,
    ),
    ("Firefox", r"Software\Mozilla\NativeMessagingHosts", true),
];

/// Point every browser's `HKCU` key at its manifest. Unlike the Unix side
/// there is nothing to probe for first: writing a key for a browser that is
/// not installed is inert, and creating it in advance means a browser
/// installed later works without Hydra being restarted.
#[cfg(target_os = "windows")]
fn register_windows(host: &Path) {
    use winreg::enums::HKEY_CURRENT_USER;
    use winreg::RegKey;

    // One manifest per dialect, in Hydra's own directory: on Windows the
    // browser is told where to look, so there is nothing to place inside a
    // browser-owned folder.
    let dir = crate::model::app_dir();
    if let Err(e) = std::fs::create_dir_all(&dir) {
        crate::log::warn(&format!("nmhost: cannot create {}: {e}", dir.display()));
        return;
    }
    let chromium = dir.join(format!("{HOST_NAME}.json"));
    let gecko = dir.join(format!("{HOST_NAME}.firefox.json"));
    for (path, body) in [
        (&chromium, manifest(host, false)),
        (&gecko, manifest(host, true)),
    ] {
        if std::fs::read_to_string(path).is_ok_and(|cur| cur == body) {
            continue;
        }
        if let Err(e) = std::fs::write(path, &body) {
            crate::log::warn(&format!("nmhost: cannot write {}: {e}", path.display()));
            return;
        }
    }

    let hkcu = RegKey::predef(HKEY_CURRENT_USER);
    let mut names = Vec::new();
    for (name, key, is_gecko) in WIN_KEYS {
        let want = if *is_gecko { &gecko } else { &chromium }
            .to_string_lossy()
            .to_string();
        let sub = format!(r"{key}\{HOST_NAME}");
        // create_subkey opens an existing key or makes a new one, which is
        // what "register, idempotently" means here.
        match hkcu.create_subkey(&sub) {
            Ok((k, _)) => {
                let cur: std::io::Result<String> = k.get_value("");
                if cur.ok().as_deref() != Some(want.as_str()) {
                    if let Err(e) = k.set_value("", &want) {
                        crate::log::warn(&format!("nmhost: {sub}: {e}"));
                        continue;
                    }
                }
                names.push(name.to_string());
            }
            Err(e) => crate::log::warn(&format!("nmhost: {sub}: {e}")),
        }
    }
    set_registered(names);
    crate::log::info(&format!("nmhost: registry keys point at {}", dir.display()));
}

/// Name of the pointer file beside `hydra-host` that tells it which
/// `--config DIR` to talk to. Must stay equal to `PROFILE_POINTER` in
/// hydra-host, which is the only reader.
const PROFILE_POINTER: &str = "hydra-profile";

/// Point the `hydra-host` next to us at `dir` — or, with `None`, take the
/// pointer away again so it goes back to the default profile.
///
/// A native-messaging manifest carries a path and no arguments, so this file
/// is the only channel there is for telling the host which profile the
/// browser's capture belongs to. It sits next to the binary rather than
/// inside the profile it names, so it travels with a portable copy and a
/// second copy elsewhere cannot claim the same one.
fn write_profile_pointer(host: &Path, dir: Option<&Path>) {
    let Some(pointer) = host.parent().map(|d| d.join(PROFILE_POINTER)) else {
        return;
    };
    let Some(dir) = dir else {
        if pointer.exists() {
            match std::fs::remove_file(&pointer) {
                Ok(()) => crate::log::info(&format!("nmhost: removed {}", pointer.display())),
                Err(e) => crate::log::warn(&format!("nmhost: {}: {e}", pointer.display())),
            }
        }
        return;
    };
    let body = format!(
        "{}
",
        dir.display()
    );
    if std::fs::read_to_string(&pointer).is_ok_and(|cur| cur == body) {
        return;
    }
    match std::fs::write(&pointer, &body) {
        Ok(()) => crate::log::info(&format!(
            "nmhost: {} points at {}",
            pointer.display(),
            dir.display()
        )),
        Err(e) => crate::log::warn(&format!("nmhost: cannot write {}: {e}", pointer.display())),
    }
}

/// Register the host with every browser on this machine. Idempotent, and
/// safe to call on every start — which is the point: an OS or browser
/// upgrade that wipes a profile directory repairs itself on the next launch.
///
/// `portable_capture` is the Options > Extensions switch, and only means
/// anything to a `--config DIR` instance. A manifest is machine-wide per
/// user and carries no arguments, so registering from a second profile
/// overwrites whatever an ordinary install registered — which is why a
/// portable copy stays out of the way by default and the WebSocket
/// transport (which needs no registration) is all it uses while it runs.
/// Switched on, it registers its own binary AND leaves the pointer file the
/// host reads, so a browser can start THIS profile when nothing is running.
///
/// Runs off the UI thread; failures are logged and otherwise ignored, since
/// the WebSocket transport still works whenever the app is already running.
pub fn ensure_registered(portable_capture: bool) {
    let profile = crate::model::app_dir_override().map(Path::to_path_buf);
    let register = profile.is_none() || portable_capture;
    std::thread::Builder::new()
        .name("nmhost-register".into())
        .spawn(move || {
            let host = match sandbox() {
                Sandbox::AppImage => {
                    crate::log::info(
                        "nmhost: AppImage — the manifests point at the shim AppRun installed, \
                         not at this transient mount",
                    );
                    return;
                }
                Sandbox::Flatpak => {
                    let Some(dir) = dirs::data_dir().map(|d| d.join("hydra")) else {
                        crate::log::warn("nmhost: no data directory for the flatpak wrapper");
                        return;
                    };
                    let Some(wrapper) = write_flatpak_wrapper(&dir) else {
                        return;
                    };
                    wrapper
                }
                Sandbox::None => match host_binary() {
                    Some(h) => h,
                    None => {
                        crate::log::warn(
                            "nmhost: hydra-host is not next to the app; browser capture cannot launch Hydra",
                        );
                        return;
                    }
                },
            };
            // Written before the manifests: a browser that spawns the host
            // the moment a key appears must already find the profile. `None`
            // takes the pointer away — the switch turned off, or an ordinary
            // install clearing one a portable copy left in its directory.
            write_profile_pointer(&host, profile.as_deref().filter(|_| register));
            if !register {
                if let Some(dir) = &profile {
                    crate::log::info(&format!(
                        "nmhost: --config {} — browser registration left to the default profile",
                        dir.display()
                    ));
                }
                return;
            }

            #[cfg(target_os = "windows")]
            {
                register_windows(&host);
            }

            #[cfg(not(target_os = "windows"))]
            {
                let Some(home) = dirs::home_dir() else {
                    crate::log::warn("nmhost: no home directory");
                    return;
                };
                let chromium = manifest(&host, false);
                let gecko = manifest(&host, true);
                let mut written = 0;
                let mut names = Vec::new();
                for t in targets(&home) {
                    let body = if t.gecko { &gecko } else { &chromium };
                    if let Some(changed) = write_manifest(&t.root, &t.dir, body) {
                        written += usize::from(changed);
                        names.push(t.name.to_string());
                    }
                }
                set_registered(names);
                crate::log::info(&format!(
                    "nmhost: {} manifest(s) written, host = {}",
                    written,
                    host.display()
                ));
            }
        })
        .ok();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn chromium_manifest_allow_lists_the_extension_origin() {
        let m = manifest(Path::new("/opt/hydra/hydra-host"), false);
        for id in CHROMIUM_EXT_IDS {
            assert!(m.contains(&format!("chrome-extension://{id}/")));
        }
        assert!(m.contains("\"allowed_origins\""));
        assert!(m.contains("\"path\": \"/opt/hydra/hydra-host\""));
        assert!(!m.contains("allowed_extensions"));
        // Must parse: a browser silently ignores a malformed manifest.
        serde_json::from_str::<serde_json::Value>(&m).unwrap();
    }

    #[test]
    fn firefox_manifest_allow_lists_the_addon_id() {
        let m = manifest(Path::new("/opt/hydra/hydra-host"), true);
        assert!(m.contains(FIREFOX_EXT_ID));
        assert!(m.contains("\"allowed_extensions\""));
        assert!(!m.contains("allowed_origins"));
        serde_json::from_str::<serde_json::Value>(&m).unwrap();
    }

    #[test]
    fn windows_paths_survive_json_escaping() {
        let m = manifest(Path::new(r"C:\Program Files\Hydra\hydra-host.exe"), false);
        let v: serde_json::Value = serde_json::from_str(&m).unwrap();
        assert_eq!(
            v["path"].as_str().unwrap(),
            r"C:\Program Files\Hydra\hydra-host.exe"
        );
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn linux_covers_snap_and_flatpak_browsers() {
        // Ubuntu ships Firefox as a snap, whose profile is nowhere near
        // ~/.mozilla. Missing it means a stock Ubuntu registers nothing and
        // the extension reports "Hydra is not reachable" with no clue why.
        let home = std::path::Path::new("/home/tester");
        let dirs: Vec<String> = targets(home)
            .into_iter()
            .map(|t| t.dir.to_string_lossy().into_owned())
            .collect();
        let has = |p: &str| dirs.iter().any(|d| d == p);

        assert!(
            has("/home/tester/snap/firefox/common/.mozilla/native-messaging-hosts"),
            "snap Firefox — the Ubuntu default — is not covered"
        );
        assert!(has(
            "/home/tester/.var/app/org.mozilla.firefox/.mozilla/native-messaging-hosts"
        ));
        assert!(has(
            "/home/tester/snap/chromium/common/chromium/NativeMessagingHosts"
        ));
        assert!(has(
            "/home/tester/.var/app/com.google.Chrome/config/google-chrome/NativeMessagingHosts"
        ));
        // ...without losing the classic ones.
        assert!(has("/home/tester/.mozilla/native-messaging-hosts"));
        assert!(has(
            "/home/tester/.config/google-chrome/NativeMessagingHosts"
        ));
    }

    /// The pointer file is the only thing that can tell `hydra-host` which
    /// `--config DIR` the browser's capture belongs to, and turning the
    /// switch back off has to leave the host pointing at the default profile
    /// again — a stale pointer would quietly aim every capture at a portable
    /// copy the user has stopped using.
    #[test]
    fn a_profile_pointer_is_written_beside_the_host_and_taken_away_again() {
        let tmp = std::env::temp_dir().join(format!("hydra-pointer-{}", std::process::id()));
        std::fs::create_dir_all(&tmp).unwrap();
        let host = tmp.join("hydra-host");
        let pointer = tmp.join(PROFILE_POINTER);
        let profile = Path::new("/media/stick/hydra/data");

        write_profile_pointer(&host, Some(profile));
        assert_eq!(
            std::fs::read_to_string(&pointer).unwrap().trim(),
            "/media/stick/hydra/data"
        );

        write_profile_pointer(&host, None);
        assert!(!pointer.exists(), "the pointer is gone, not emptied");
        // Removing one that was never there is not an error either.
        write_profile_pointer(&host, None);

        std::fs::remove_dir_all(&tmp).ok();
    }

    #[test]
    fn nothing_is_written_where_the_browser_is_absent() {
        let tmp = std::env::temp_dir().join(format!("hydra-nmhost-{}", std::process::id()));
        let root = tmp.join("not-installed");
        let dir = root.join("NativeMessagingHosts");
        assert!(write_manifest(&root, &dir, "{}").is_none());
        assert!(!dir.exists());
    }

    #[test]
    fn an_unchanged_manifest_is_not_rewritten() {
        let tmp = std::env::temp_dir().join(format!("hydra-nmhost-same-{}", std::process::id()));
        let dir = tmp.join("NativeMessagingHosts");
        std::fs::create_dir_all(&tmp).unwrap();
        let body = manifest(Path::new("/opt/hydra/hydra-host"), false);
        assert_eq!(write_manifest(&tmp, &dir, &body), Some(true));
        // Still registered, just not rewritten.
        assert_eq!(write_manifest(&tmp, &dir, &body), Some(false));
        // A changed host path does get written through.
        let moved = manifest(Path::new("/usr/local/bin/hydra-host"), false);
        assert_eq!(write_manifest(&tmp, &dir, &moved), Some(true));
        std::fs::remove_dir_all(&tmp).ok();
    }

    /// A browser outside the sandbox cannot run `/app/bin/hydra-host`; what
    /// it can run is a script that asks flatpak to. The manifest must point
    /// at that script, and the script must be executable.
    #[test]
    fn the_flatpak_wrapper_re_enters_the_sandbox() {
        let tmp = std::env::temp_dir().join(format!("hydra-flatpak-{}", std::process::id()));
        let wrapper = write_flatpak_wrapper(&tmp).expect("wrapper written");
        let body = std::fs::read_to_string(&wrapper).unwrap();
        assert!(body.starts_with("#!/bin/sh\n"));
        assert!(body.contains("exec flatpak run --command=hydra-host io.github.ja7ad.hydra \"$@\""));
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&wrapper).unwrap().permissions().mode();
            assert_eq!(mode & 0o111, 0o111, "not executable: {mode:o}");
        }
        // Idempotent: the second pass finds it and leaves it.
        assert_eq!(write_flatpak_wrapper(&tmp), Some(wrapper.clone()));
        let m = manifest(&wrapper, false);
        assert!(m.contains(&format!(
            "\"path\": {}",
            json_str(&wrapper.to_string_lossy())
        )));
        std::fs::remove_dir_all(&tmp).ok();
    }
}
