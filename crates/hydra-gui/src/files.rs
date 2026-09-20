// Copyright (C) 2026 Javad Rajabzadeh
// SPDX-License-Identifier: GPL-3.0-or-later

//! What the download list does to the file on disk once it is there: show it
//! in the system file manager, hand it to another application, move it.
//!
//! Each of these is a different shell integration on every platform and none
//! of them is `open`: `open::that` launches a path with its default handler,
//! which for a *file* is the wrong verb for all three. They live together
//! because they share the same failure policy — the file manager, the
//! chooser and the move are all best-effort user actions, so a platform that
//! cannot do one falls back to the nearest thing that works rather than
//! reporting an error nobody can act on.
//!
//! The commands are built by their own functions and spawned by the callers,
//! which is what lets the argument shapes that actually go wrong (a
//! `/select,` with a space in it, an unencoded `file://` URI) be checked
//! without a file-manager window opening on a test runner.

use iced::window;
use iced::Task;
use std::io;
use std::path::Path;
use std::process::{Command, Stdio};

/// Run a helper without waiting for it and without inheriting our stdio.
///
/// Every command here is a shell integration that puts up its own window;
/// the caller is the UI thread, so none of them may be waited on. `true`
/// when the process started, which is all the caller can know at this point.
fn spawn(mut cmd: Command) -> bool {
    cmd.stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .is_ok()
}

/// Open the folder holding `path`, with `path` itself selected when
/// `select`.
///
/// Highlighting the file is what makes this useful in a folder with nine
/// hundred downloads in it, but selecting an item names one file manager —
/// Explorer, Finder — where opening a folder goes through the shell. So a
/// user running a replacement turns `select` off and gets their own window
/// without the highlight: Directory Opus and friends hook the folder call
/// and nothing `explorer /select,` takes. The folder is the fallback too,
/// for a path with nothing to select and a command that would not start.
pub fn reveal(path: &Path, select: bool) {
    if let Some(cmd) = reveal_command(path, select) {
        if spawn(cmd) {
            return;
        }
    }
    if let Some(dir) = path.parent() {
        let _ = open::that_detached(dir);
    }
}

/// The platform's "select this item in its folder", or `None` when the user
/// asked for the folder alone, there is no file to point at, or the
/// platform has no such thing to run.
fn reveal_command(path: &Path, select: bool) -> Option<Command> {
    if !select || !path.is_file() {
        return None;
    }
    #[cfg(target_os = "windows")]
    {
        // explorer.exe exits non-zero even when it did open the window, so
        // its status says nothing — spawning is the only signal there is.
        // The comma is part of the verb and the path must follow it with no
        // space, so this is ONE argument, not two.
        let mut cmd = Command::new("explorer");
        cmd.arg(format!("/select,{}", path.display()));
        Some(cmd)
    }
    #[cfg(target_os = "macos")]
    {
        let mut cmd = Command::new("open");
        cmd.arg("-R").arg(path);
        Some(cmd)
    }
    #[cfg(target_os = "linux")]
    {
        // The freedesktop interface every GTK and Qt file manager implements
        // (Nautilus, Dolphin, Nemo, Thunar, PCManFM). dbus-send ships with
        // dbus itself, so this needs no library and no session of our own;
        // a desktop without the interface answers with an error and the
        // caller falls back to opening the folder.
        let mut cmd = Command::new("dbus-send");
        cmd.args([
            "--session",
            "--dest=org.freedesktop.FileManager1",
            "--type=method_call",
            "/org/freedesktop/FileManager1",
            "org.freedesktop.FileManager1.ShowItems",
        ])
        .arg(format!("array:string:{}", file_uri(path)))
        .arg("string:");
        Some(cmd)
    }
    #[cfg(not(any(target_os = "windows", target_os = "macos", target_os = "linux")))]
    {
        None
    }
}

/// `file://` URI for an absolute path, percent-encoding everything outside
/// the unreserved set. Spaces and non-ASCII names are the common case in a
/// downloads folder, and an unencoded one is simply not a URI.
#[cfg(target_os = "linux")]
fn file_uri(path: &Path) -> String {
    use std::fmt::Write;
    let mut out = String::from("file://");
    for b in path.to_string_lossy().bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' | b'/' => {
                out.push(b as char)
            }
            _ => {
                let _ = write!(out, "%{b:02X}");
            }
        }
    }
    out
}

/// Ask the user which application should open `path`, and open it with that
/// one — without changing what the file type opens with by default.
///
/// Windows and macOS each ship a system chooser for exactly this, and it is
/// a process to start: there is nothing left to wait for, so the [`Task`] is
/// empty and `owner` goes unused.
#[cfg(not(target_os = "linux"))]
pub fn open_with<T: Send + 'static>(owner: Option<window::Id>, path: &Path) -> Task<T> {
    let _ = owner;
    #[cfg(any(target_os = "windows", target_os = "macos"))]
    {
        spawn(open_with_command(path));
    }
    #[cfg(not(any(target_os = "windows", target_os = "macos")))]
    {
        let _ = path;
    }
    Task::none()
}

/// Ask the user which application should open `path`, and open it with that
/// one — without changing what the file type opens with by default.
///
/// Linux ships no application chooser to call, so the application is chosen
/// with the same native panel "Browse..." uses, which is why this one has a
/// [`Task`] to hand back. Applications live in .desktop files, and the
/// desktops differ on whether they will launch one at all, so the panel
/// starts where they are kept and takes either shape.
#[cfg(target_os = "linux")]
pub fn open_with<T: Send + 'static>(owner: Option<window::Id>, path: &Path) -> Task<T> {
    let ask = crate::picker::Ask {
        title: Some(crate::i18n::tr("Open with...")),
        ..crate::picker::Ask::in_dir("/usr/share/applications")
    };
    let path = path.to_path_buf();
    crate::picker::file(owner, ask).and_then(move |app| {
        spawn(open_with_command(&path, &app));
        Task::none()
    })
}

/// The Windows "Open with" dialog. Shipped with the shell, and the only way
/// to reach it: the ShellExecute `openas` verb is the same dialog, and
/// rundll32 needs no FFI.
#[cfg(target_os = "windows")]
fn open_with_command(path: &Path) -> Command {
    let mut cmd = Command::new("rundll32.exe");
    cmd.arg("shell32.dll,OpenAs_RunDLL").arg(path);
    cmd
}

/// macOS puts its application chooser in AppleScript rather than in `open`.
/// `open ... using` is Finder's "Open With", so it leaves the file type's
/// default handler alone. The path travels as an argument rather than
/// spliced into the script, so a name with a quote in it cannot change what
/// runs.
#[cfg(target_os = "macos")]
fn open_with_command(path: &Path) -> Command {
    const SCRIPT: &str = r#"on run argv
    set theFile to POSIX file (item 1 of argv)
    set theApp to (choose application with title "Open With" with prompt "Choose an application to open this file:") as alias
    tell application "Finder" to open theFile using theApp
end run"#;
    let mut cmd = Command::new("osascript");
    cmd.arg("-e").arg(SCRIPT).arg(path);
    cmd
}

/// Open `path` with the application the Linux picker returned. A `.desktop`
/// file is launched through gio, which reads its `Exec` line and the field
/// codes in it; anything else is a program to run with the file as its
/// argument.
#[cfg(target_os = "linux")]
fn open_with_command(path: &Path, app: &Path) -> Command {
    if app.extension().is_some_and(|e| e == "desktop") {
        let mut cmd = Command::new("gio");
        cmd.arg("launch").arg(app).arg(path);
        return cmd;
    }
    let mut cmd = Command::new(app);
    cmd.arg(path);
    cmd
}

/// Move a file to `to`, creating its directory first.
///
/// `rename` cannot cross a filesystem, and a downloads folder and the place
/// a file is being filed away to are very often on different ones (an
/// external disk, a network share, `/home` against `/data`), so a failed
/// rename is retried as copy-then-remove. The copy is only reached when the
/// rename failed, so a permission error costs one extra failed syscall and
/// still reports the failure rather than leaving two copies.
pub fn move_file(from: &Path, to: &Path) -> io::Result<()> {
    if let Some(dir) = to.parent() {
        std::fs::create_dir_all(dir)?;
    }
    if std::fs::rename(from, to).is_ok() {
        return Ok(());
    }
    std::fs::copy(from, to)?;
    std::fs::remove_file(from)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Program and arguments of a built command, for the shape assertions
    /// below — what a file manager is handed is the whole contract here.
    fn spelling(cmd: &Command) -> (String, Vec<String>) {
        (
            cmd.get_program().to_string_lossy().into_owned(),
            cmd.get_args()
                .map(|a| a.to_string_lossy().into_owned())
                .collect(),
        )
    }

    /// A real downloaded file, since a reveal command is only built for a
    /// path that is one. Its own directory per test, so the cleanups do not
    /// race each other.
    fn a_downloaded_file(test: &str, name: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("hydra-reveal-{}-{test}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("test dir");
        let file = dir.join(name);
        std::fs::write(&file, b"payload").expect("downloaded file");
        file
    }

    #[test]
    fn a_move_lands_the_bytes_and_leaves_nothing_behind() {
        let dir = std::env::temp_dir().join(format!("hydra-files-{}", std::process::id()));
        let from = dir.join("a.bin");
        let to = dir.join("sub").join("b.bin");
        std::fs::create_dir_all(&dir).expect("test dir");
        std::fs::write(&from, b"payload").expect("source file");

        move_file(&from, &to).expect("the move succeeds");
        assert_eq!(std::fs::read(&to).expect("moved file"), b"payload");
        assert!(!from.exists(), "the source is gone");

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn a_move_of_a_file_that_is_not_there_reports_it() {
        let missing = std::env::temp_dir().join("hydra-files-nothing-here.bin");
        let to = std::env::temp_dir().join("hydra-files-nowhere.bin");
        assert!(move_file(&missing, &to).is_err(), "nothing to move");
        assert!(!to.exists(), "and nothing is created for it");
    }

    /// Explorer takes the item to select as part of the `/select,` verb.
    /// Split into two arguments — or given the comma with a space after it —
    /// it silently opens the user's Documents folder instead.
    #[cfg(target_os = "windows")]
    #[test]
    fn windows_selects_the_item_in_one_argument() {
        let file = a_downloaded_file("win-select", "My File.zip");
        let cmd = reveal_command(&file, true).expect("a command");
        let (program, args) = spelling(&cmd);
        assert_eq!(program, "explorer");
        let [arg] = args.as_slice() else {
            panic!("the item travels as ONE argument, not two: {args:?}");
        };
        assert!(
            arg.starts_with("/select,"),
            "no space after the comma: {arg}"
        );
        assert!(arg.ends_with("My File.zip"), "lost the item: {arg}");

        std::fs::remove_dir_all(file.parent().expect("dir")).ok();
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn macos_reveals_rather_than_opening() {
        let file = a_downloaded_file("mac-reveal", "My File.zip");
        let cmd = reveal_command(&file, true).expect("a command");
        let (program, args) = spelling(&cmd);
        assert_eq!(program, "open");
        // -R selects the file in Finder; without it `open` would RUN it.
        assert_eq!(args, ["-R", &file.to_string_lossy()]);

        std::fs::remove_dir_all(file.parent().expect("dir")).ok();
    }

    /// The chooser has to be handed the file as an argument, never spliced
    /// into the script: a downloaded name containing a quote would otherwise
    /// end the string literal and run whatever followed it.
    #[cfg(target_os = "macos")]
    #[test]
    fn macos_hands_the_chooser_the_path_as_an_argument() {
        let cmd = open_with_command(Path::new("/Users/a/Downloads/\" & do shell script \"x"));
        let (program, args) = spelling(&cmd);
        assert_eq!(program, "osascript");
        assert_eq!(args[0], "-e");
        assert!(args[1].contains("choose application"));
        assert!(
            !args[1].contains("do shell script"),
            "the name must not reach the script text"
        );
        assert_eq!(args[2], "/Users/a/Downloads/\" & do shell script \"x");
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn linux_asks_the_file_manager_for_a_uri_it_can_read() {
        let file = a_downloaded_file("linux-showitems", "naïve (1).zip");
        let cmd = reveal_command(&file, true).expect("cmd");
        let (program, args) = spelling(&cmd);
        assert_eq!(program, "dbus-send");
        assert!(args.contains(&"org.freedesktop.FileManager1.ShowItems".to_string()));
        assert!(args.contains(&format!("array:string:{}", file_uri(&file))));
        assert!(args.iter().any(|a| a.contains("na%C3%AFve%20%281%29.zip")));
        // ShowItems takes a startup id as its second argument; an empty one
        // is still an argument, and omitting it fails the call.
        assert_eq!(args.last().map(String::as_str), Some("string:"));

        std::fs::remove_dir_all(file.parent().expect("dir")).ok();
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn a_file_uri_leaves_the_unreserved_set_alone() {
        assert_eq!(file_uri(Path::new("/tmp/a-b_c.d~")), "file:///tmp/a-b_c.d~");
    }

    /// A desktop entry is not an executable: running it directly would try
    /// to exec a text file, so it goes through gio instead.
    #[cfg(target_os = "linux")]
    #[test]
    fn linux_launches_a_desktop_entry_through_gio() {
        let file = Path::new("/home/a/Downloads/x.pdf");
        let (program, args) = spelling(&open_with_command(
            file,
            Path::new("/usr/share/applications/okular.desktop"),
        ));
        assert_eq!(program, "gio");
        assert_eq!(
            args,
            [
                "launch",
                "/usr/share/applications/okular.desktop",
                "/home/a/Downloads/x.pdf"
            ]
        );

        let (program, args) = spelling(&open_with_command(file, Path::new("/usr/bin/xpdf")));
        assert_eq!(program, "/usr/bin/xpdf");
        assert_eq!(args, ["/home/a/Downloads/x.pdf"]);
    }

    /// The bug this setting exists for: with it off nothing is asked of a
    /// named file manager, so the folder goes to the shell — the call a
    /// replacement for Explorer has hooked.
    #[test]
    fn the_folder_alone_names_no_file_manager() {
        let file = a_downloaded_file("folder-only", "My File.zip");
        assert!(reveal_command(&file, false).is_none());

        std::fs::remove_dir_all(file.parent().expect("dir")).ok();
    }

    /// A file that is not there has nothing to highlight, whatever the
    /// setting says — a moved or deleted download still opens its folder.
    #[test]
    fn a_missing_download_has_nothing_to_select() {
        let gone = std::env::temp_dir().join("hydra-reveal-nothing-here.bin");
        assert!(reveal_command(&gone, true).is_none());
    }
}
