// Copyright (C) 2026 Javad Rajabzadeh
// SPDX-License-Identifier: GPL-3.0-or-later

//! Application self-update: the startup version check and the "Update Now"
//! pipeline (download → checksum → extract → launch the finisher → exit).
//!
//! The heavy lifting lives in the `hya-updater` crate; this module adapts it
//! to iced — the check is a one-shot `Task::perform` future, the update run
//! is a `Task::run` stream so the dialog can draw live progress. Endpoints
//! come from `hya_updater::api_base()`, so `HYDRA_UPDATE_API` pointed at the
//! mock server (`cargo run -p hya-updater --example mock_server`) exercises
//! this whole path without touching real releases.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use hya_updater::{UpdateMethod, Verification};

/// What the dialog needs to know about the newer release.
#[derive(Clone, Debug)]
pub struct UpdateInfo {
    pub version: String,
    /// Release notes, GitHub-flavoured markdown.
    pub notes: String,
    /// Release web page, for "open in browser".
    pub html_url: String,
    pub asset_name: String,
    pub asset_url: String,
    pub size: u64,
    /// `SHA256SUMS.txt` asset, when the release publishes one.
    pub sums_url: Option<String>,
    /// Whether Hydra can install this update itself. False for a packaged
    /// install (`/usr/bin` from a deb or rpm, a `.pkg` in `/Applications`):
    /// the dialog then offers the installer instead.
    pub in_place: bool,
    /// Whether finishing the update will ask for an administrator password
    /// — a root-owned install (a tarball unpacked into `/usr/local` with
    /// sudo) that Hydra may still replace, once the user authorises it.
    pub needs_auth: bool,
    /// The `.deb`/`.rpm` for this machine, when the install is packaged and
    /// the release ships one: (file name, download URL, size).
    pub package: Option<(String, String, u64)>,
    /// The release ships a bundle for this OS and architecture. Without one
    /// there is nothing to download: the dialog names the version and
    /// offers the release page, and that is the whole offer.
    pub has_bundle: bool,
    /// The command that updates a package-managed install (Homebrew), when
    /// the package manager is known.
    pub package_hint: Option<&'static str>,
}

/// Progress of a running update, streamed into the dialog.
#[derive(Clone, Debug)]
pub enum UpdateEvent {
    Progress(u64, Option<u64>),
    Verifying,
    Preparing,
    /// The finisher is running; the app must now exit so it can swap files.
    ReadyToRestart,
    Cancelled,
    Failed(String),
}

fn user_agent() -> String {
    format!("hydra-gui/{}", env!("CARGO_PKG_VERSION"))
}

/// Ask the release API whether a newer GUI bundle exists for this machine.
/// `beta` (Options > General > "Download Beta channel") also considers `-rc`
/// pre-releases when one is ahead of the stable release.
///
/// `Ok(None)` is "up to date". A newer release with no asset for this
/// OS/arch comes back with `has_bundle == false`: the version is still news,
/// even when the dialog can only point at the release page.
pub async fn check(beta: bool) -> Result<Option<UpdateInfo>, String> {
    let rel = hya_updater::check_channel(&user_agent(), beta)
        .await
        .map_err(|e| e.to_string())?;
    if !hya_updater::is_newer(rel.version(), env!("CARGO_PKG_VERSION")) {
        return Ok(None);
    }
    // Running from an AppImage, the update IS the new AppImage: the release
    // tarball would only be unpackable over a read-only mount that stops
    // existing when this process does.
    let appimage = hya_updater::appimage_path();
    let asset = match &appimage {
        Some(_) => rel.appimage_asset(),
        None => rel.gui_asset(),
    };
    let Some(asset) = asset else {
        crate::log::warn(&format!(
            "update {} available but has no {} bundle",
            rel.version(),
            match &appimage {
                Some(_) => format!("{}.AppImage", hya_updater::appimage_arch()),
                None => format!("{}-{}", hya_updater::os_tag(), hya_updater::arch_tag()),
            }
        ));
        return Ok(Some(UpdateInfo {
            version: rel.version().to_string(),
            notes: hya_updater::clean_notes(&rel.body),
            html_url: rel.html_url.clone(),
            asset_name: String::new(),
            asset_url: String::new(),
            size: 0,
            sums_url: None,
            in_place: false,
            needs_auth: false,
            package: None,
            has_bundle: false,
            package_hint: None,
        }));
    };
    // Decided before the download, not after: a package-managed install
    // cannot be rewritten by this process however many megabytes arrive.
    let install_dir = std::env::current_exe()
        .ok()
        .and_then(|exe| exe.parent().map(PathBuf::from));
    let method = install_dir
        .as_deref()
        .map(hya_updater::update_method)
        .unwrap_or(UpdateMethod::Package);
    let in_place = method.is_self_update();
    // For an AppImage the install is the image file, not the mount
    // `current_exe()` reports — say so in the log, that is the path the
    // finisher will rewrite.
    let where_ = appimage
        .as_deref()
        .or(install_dir.as_deref())
        .map(|d| d.display().to_string())
        .unwrap_or_else(|| "the install directory".into());
    if !in_place {
        crate::log::info(&format!(
            "update {} available but {where_} is owned by a package manager; \
             offering the installer instead",
            rel.version()
        ));
    } else {
        crate::log::info(&format!(
            "update {} available; {where_} updates {}",
            rel.version(),
            match method {
                UpdateMethod::Elevated => "in place, after authorisation",
                UpdateMethod::AppImage => "by replacing the image file",
                _ => "in place",
            }
        ));
    }
    let package = (!in_place)
        .then(|| rel.package_asset())
        .flatten()
        .map(|a| (a.name.clone(), a.browser_download_url.clone(), a.size));
    Ok(Some(UpdateInfo {
        version: rel.version().to_string(),
        notes: hya_updater::clean_notes(&rel.body),
        html_url: rel.html_url.clone(),
        asset_name: asset.name.clone(),
        asset_url: asset.browser_download_url.clone(),
        size: asset.size,
        sums_url: rel
            .asset("SHA256SUMS.txt")
            .map(|a| a.browser_download_url.clone()),
        in_place,
        needs_auth: match method {
            UpdateMethod::Elevated => true,
            // The image may sit in /opt or /usr/local/bin; the finisher
            // elevates on its own, but the dialog should warn first.
            UpdateMethod::AppImage => hya_updater::appimage_needs_auth(),
            _ => false,
        },
        package,
        has_bundle: true,
        package_hint: package_hint(method, install_dir.as_deref()),
    }))
}

/// The package manager's own update command, for an install only it may
/// rewrite.
fn package_hint(method: UpdateMethod, install_dir: Option<&Path>) -> Option<&'static str> {
    match method {
        UpdateMethod::Package => install_dir.and_then(hya_updater::package_manager_hint),
        _ => None,
    }
}

/// The archive against the release's `SHA256SUMS.txt`: a mismatch or a
/// missing entry is refused (and the archive removed), and a release that
/// publishes no sums at all is accepted on transport security alone, with
/// the log saying so.
fn verify_download(archive: &Path, asset_name: &str, sums: Option<&str>) -> std::io::Result<()> {
    match hya_updater::verify_archive(archive, asset_name, sums)? {
        Verification::Verified => {}
        Verification::Unpublished => crate::log::warn(&format!(
            "update {asset_name}: the release publishes no SHA256SUMS.txt; \
             accepted on transport security alone"
        )),
    }
    Ok(())
}

/// Run the full update as an event stream: download the archive into the OS
/// temp staging dir, verify it, extract it, put the finisher binary in a
/// path that will not be overwritten, and start it detached.
pub fn run(
    info: UpdateInfo,
    cancel: Arc<AtomicBool>,
) -> impl iced::futures::Stream<Item = UpdateEvent> {
    iced::stream::channel(64, async move |mut tx| {
        use iced::futures::SinkExt;
        match drive(info, cancel, &mut tx).await {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {
                let _ = tx.send(UpdateEvent::Cancelled).await;
            }
            Err(e) => {
                crate::log::error(&format!("update failed: {e}"));
                let _ = tx.send(UpdateEvent::Failed(e.to_string())).await;
            }
        }
    })
}

async fn drive(
    info: UpdateInfo,
    cancel: Arc<AtomicBool>,
    tx: &mut iced::futures::channel::mpsc::Sender<UpdateEvent>,
) -> std::io::Result<()> {
    use iced::futures::SinkExt;
    let ua = user_agent();
    let stage = hya_updater::staging_dir();
    let archive = stage.join(&info.asset_name);

    // Download. Progress goes through try_send: a full channel drops a
    // repaint, never blocks the transfer; the terminal events use send().
    {
        let mut progress_tx = tx.clone();
        let cancel = cancel.clone();
        hya_updater::http::download_to_file(&info.asset_url, &ua, &archive, move |got, total| {
            let _ = progress_tx.try_send(UpdateEvent::Progress(got, total));
            !cancel.load(Ordering::Relaxed)
        })
        .await?;
    }

    let _ = tx.send(UpdateEvent::Verifying).await;
    let sums = match &info.sums_url {
        Some(url) => {
            let body = hya_updater::http::get_bytes(url, &ua, 1024 * 1024).await?;
            Some(String::from_utf8_lossy(&body).into_owned())
        }
        None => None,
    };
    verify_download(&archive, &info.asset_name, sums.as_deref())?;

    let _ = tx.send(UpdateEvent::Preparing).await;
    // An AppImage download is the finished article: one executable file that
    // replaces the installed one. Everything else arrives as an archive of
    // binaries to unpack and copy over.
    let appimage = hya_updater::appimage_path();
    let bundle = match &appimage {
        Some(_) => None,
        None => {
            let unpack = stage.join("unpacked");
            let _ = std::fs::remove_dir_all(&unpack);
            Some(hya_updater::extract(&archive, &unpack)?)
        }
    };

    // The finisher: the new release's copy first (version-matched to what it
    // installs), else the one beside the running app; either way run from the
    // staging dir so the swap cannot overwrite it. An AppImage is a squashfs
    // image, not a directory, so it only has the second option.
    let updater_name = if cfg!(target_os = "windows") {
        "hydra-updater.exe"
    } else {
        "hydra-updater"
    };
    let exe = std::env::current_exe()?;
    let install_dir = exe
        .parent()
        .ok_or_else(|| std::io::Error::other("executable has no parent directory"))?
        .to_path_buf();
    let updater_src: PathBuf = bundle
        .iter()
        .map(|b| b.join(updater_name))
        .chain(std::iter::once(install_dir.join(updater_name)))
        .find(|p| p.is_file())
        .ok_or_else(|| {
            std::io::Error::other(format!(
                "no {updater_name} in the release bundle or next to the app; \
                 the downloaded update is at {}",
                archive.display()
            ))
        })?;
    let updater = stage.join(updater_name);
    std::fs::copy(&updater_src, &updater)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(&updater, std::fs::Permissions::from_mode(0o755));
    }

    let mut cmd = std::process::Command::new(&updater);
    match (&appimage, &bundle) {
        // Replace the image file the user launched, and relaunch that same
        // path — not `current_exe()`, which points into a mount that will
        // not exist a moment from now.
        (Some(img), _) => {
            cmd.arg("--src-file")
                .arg(&archive)
                .arg("--appimage")
                .arg(img)
                .arg("--relaunch")
                .arg(img);
        }
        (None, Some(bundle)) => {
            cmd.arg("--src-dir")
                .arg(bundle)
                .arg("--install-dir")
                .arg(&install_dir)
                // What the new files are: a macOS `.app` carries its version
                // in Info.plist, which nothing in the archive itself can tell
                // the finisher.
                .arg("--app-version")
                .arg(&info.version)
                .arg("--relaunch")
                .arg(&exe);
        }
        (None, None) => return Err(std::io::Error::other("nothing was extracted to install")),
    }
    // The finisher restarts us; a `--config DIR` instance has to come back
    // on the same profile rather than on the default one.
    if let Some(dir) = crate::model::app_dir_override() {
        cmd.arg("--relaunch-arg")
            .arg("--config")
            .arg("--relaunch-arg")
            .arg(dir);
    }
    #[cfg(target_os = "windows")]
    {
        use std::os::windows::process::CommandExt;
        // CREATE_NO_WINDOW | DETACHED_PROCESS: no console flash, and the
        // finisher outlives this process cleanly.
        cmd.creation_flags(0x0800_0000 | 0x0000_0008);
    }
    // Last look before the point of no return: a finisher, once started,
    // waits for this process to exit and then swaps the files whatever the
    // user clicked meanwhile.
    if cancel.load(Ordering::Relaxed) {
        return Err(std::io::Error::from(std::io::ErrorKind::Interrupted));
    }
    cmd.spawn()?;
    crate::log::info(&format!(
        "update {} downloaded; finisher started, exiting to let it swap files",
        info.version
    ));
    let _ = tx.send(UpdateEvent::ReadyToRestart).await;
    Ok(())
}

/// Startup housekeeping: remove `.old` files a previous update left behind
/// (Windows cannot delete the old exe while it is still tearing down).
pub fn sweep_leftovers() {
    // The AppImage case first: the previous image was renamed aside next to
    // itself, and `current_exe()` points at a mount that never holds one.
    if let Some(img) = hya_updater::appimage_path() {
        hya_updater::sweep_appimage_leftover(&img);
        return;
    }
    if let Ok(exe) = std::env::current_exe() {
        if let Some(dir) = exe.parent() {
            hya_updater::sweep_old_files(dir);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn archive_named(name: &str) -> PathBuf {
        let path =
            std::env::temp_dir().join(format!("hydra-gui-verify-{}-{name}", std::process::id()));
        std::fs::write(&path, b"release bytes").unwrap();
        path
    }

    #[test]
    fn a_matching_published_sum_passes_and_keeps_the_archive() {
        let archive = archive_named("ok.tar.gz");
        let sums = format!(
            "{} *ok.tar.gz\n",
            hya_updater::file_sha256(&archive).unwrap()
        );
        verify_download(&archive, "ok.tar.gz", Some(&sums)).unwrap();
        assert!(archive.is_file());
        let _ = std::fs::remove_file(&archive);
    }

    #[test]
    fn a_wrong_or_missing_sum_refuses_and_discards_the_archive() {
        let archive = archive_named("bad.tar.gz");
        let wrong = format!("{} bad.tar.gz\n", "0".repeat(64));
        assert!(verify_download(&archive, "bad.tar.gz", Some(&wrong)).is_err());
        assert!(!archive.exists(), "a mismatch leaves nothing to retry over");

        let archive = archive_named("unlisted.tar.gz");
        let other = format!("{} other.tar.gz\n", "0".repeat(64));
        assert!(verify_download(&archive, "unlisted.tar.gz", Some(&other)).is_err());
        assert!(!archive.exists());
    }

    #[test]
    fn a_release_without_sums_is_accepted_on_transport_alone() {
        let archive = archive_named("nosums.tar.gz");
        verify_download(&archive, "nosums.tar.gz", None).unwrap();
        assert!(archive.is_file());
        let _ = std::fs::remove_file(&archive);
    }

    #[test]
    fn the_package_hint_only_names_a_manager_for_a_packaged_install() {
        let dir = std::env::temp_dir();
        assert_eq!(package_hint(UpdateMethod::InPlace, Some(&dir)), None);
        assert_eq!(package_hint(UpdateMethod::Elevated, Some(&dir)), None);
        assert_eq!(package_hint(UpdateMethod::AppImage, Some(&dir)), None);
        // A packaged install nothing Homebrew owns has no command to offer.
        assert_eq!(package_hint(UpdateMethod::Package, Some(&dir)), None);
        assert_eq!(package_hint(UpdateMethod::Package, None), None);
    }
}
