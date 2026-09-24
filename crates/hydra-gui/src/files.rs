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
//! which is what lets the argument shapes that actually go wrong (an
//! unencoded `file://` URI, a file name spliced into a script) be checked
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

/// Open the folder holding `path` with `path` itself selected.
///
/// "Open folder" on a download that landed in `~/Downloads` next to nine
/// hundred other files is only useful if the file is the one highlighted
/// when the window comes up. A path with nothing to select, or a platform
/// that cannot select it, gets its containing folder instead.
///
/// On a thread of its own: the file check and the Windows shell call can
/// each stall for seconds on a network share, and the caller is the UI.
pub fn reveal(path: &Path) {
    let path = path.to_path_buf();
    std::thread::spawn(move || {
        if path.is_file() && select_in_folder(&path) {
            return;
        }
        if let Some(dir) = path.parent() {
            let _ = open::that_detached(dir);
        }
    });
}

/// Windows selects through the shell API rather than `explorer /select,`:
/// a replacement file manager (Directory Opus, XYplorer) takes over that
/// call, but never a command line that runs explorer.exe by name.
#[cfg(target_os = "windows")]
fn select_in_folder(path: &Path) -> bool {
    shell::open_folder_and_select(path)
}

#[cfg(not(target_os = "windows"))]
fn select_in_folder(path: &Path) -> bool {
    reveal_command(path).is_some_and(spawn)
}

/// The platform's "select this item in its folder", or `None` where there is
/// no such thing to run.
#[cfg(not(target_os = "windows"))]
fn reveal_command(path: &Path) -> Option<Command> {
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
    #[cfg(not(any(target_os = "macos", target_os = "linux")))]
    {
        let _ = path;
        None
    }
}

/// `SHOpenFolderAndSelectItems` and the COM and item-ID-list lifetimes it
/// needs around it.
#[cfg(target_os = "windows")]
mod shell {
    use std::os::windows::ffi::OsStrExt;
    use std::path::Path;
    use std::ptr;

    use windows_sys::Win32::System::Com::{
        CoInitializeEx, CoUninitialize, COINIT_APARTMENTTHREADED, COINIT_DISABLE_OLE1DDE,
    };
    use windows_sys::Win32::UI::Shell::Common::ITEMIDLIST;
    use windows_sys::Win32::UI::Shell::{ILFree, SHOpenFolderAndSelectItems, SHParseDisplayName};

    /// `true` once the shell has shown `path` selected in its folder.
    pub(super) fn open_folder_and_select(path: &Path) -> bool {
        let Some(_com) = Apartment::enter() else {
            return false;
        };
        let Some(item) = ItemIdList::parse(path) else {
            return false;
        };
        // SAFETY: `item` is a live absolute ID list. With no children given,
        // the shell opens the item's parent and selects the item itself.
        unsafe { SHOpenFolderAndSelectItems(item.0, 0, ptr::null(), 0) >= 0 }
    }

    /// COM initialised on this thread for as long as the value lives, which
    /// the shell requires of anyone calling `SHOpenFolderAndSelectItems`.
    struct Apartment;

    impl Apartment {
        fn enter() -> Option<Self> {
            let mode = (COINIT_APARTMENTTHREADED | COINIT_DISABLE_OLE1DDE) as u32;
            // SAFETY: no reserved pointer; a thread that already has an
            // apartment of the same kind gets S_FALSE, which still needs the
            // CoUninitialize that Drop makes.
            (unsafe { CoInitializeEx(ptr::null(), mode) } >= 0).then_some(Apartment)
        }
    }

    impl Drop for Apartment {
        fn drop(&mut self) {
            // SAFETY: balances the successful CoInitializeEx in `enter`.
            unsafe { CoUninitialize() }
        }
    }

    /// An absolute shell item ID list, freed on drop.
    struct ItemIdList(*mut ITEMIDLIST);

    impl ItemIdList {
        /// `None` for a path the shell cannot resolve — one that no longer
        /// exists among them.
        fn parse(path: &Path) -> Option<Self> {
            let wide: Vec<u16> = path.as_os_str().encode_wide().chain([0]).collect();
            let mut item = ptr::null_mut();
            // SAFETY: `wide` is NUL-terminated and outlives the call; the
            // bind context and attribute query are optional and left null.
            let hr = unsafe {
                SHParseDisplayName(
                    wide.as_ptr(),
                    ptr::null_mut(),
                    &mut item,
                    0,
                    ptr::null_mut(),
                )
            };
            (hr >= 0 && !item.is_null()).then_some(ItemIdList(item))
        }
    }

    impl Drop for ItemIdList {
        fn drop(&mut self) {
            // SAFETY: allocated by SHParseDisplayName and freed exactly once.
            unsafe { ILFree(self.0) }
        }
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn a_download_with_a_space_and_non_ascii_name_resolves_to_a_shell_item() {
            let dir = std::env::temp_dir().join(format!("hydra-shell-{}", std::process::id()));
            std::fs::create_dir_all(&dir).expect("test dir");
            let file = dir.join("naïve (1) файл.zip");
            std::fs::write(&file, b"payload").expect("downloaded file");

            let _com = Apartment::enter().expect("an apartment on a fresh test thread");
            assert!(ItemIdList::parse(&file).is_some());

            std::fs::remove_dir_all(&dir).ok();
        }

        /// A deleted download has no item to select, which is what sends
        /// `reveal` to its open-the-folder fallback.
        #[test]
        fn a_deleted_download_resolves_to_nothing() {
            let _com = Apartment::enter().expect("an apartment on a fresh test thread");
            let gone = std::env::temp_dir().join("hydra-shell-nothing-here.bin");
            assert!(ItemIdList::parse(&gone).is_none());
        }
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
    #[cfg(any(target_os = "macos", target_os = "linux"))]
    fn spelling(cmd: &Command) -> (String, Vec<String>) {
        (
            cmd.get_program().to_string_lossy().into_owned(),
            cmd.get_args()
                .map(|a| a.to_string_lossy().into_owned())
                .collect(),
        )
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

    #[cfg(target_os = "macos")]
    #[test]
    fn macos_reveals_rather_than_opening() {
        let cmd = reveal_command(Path::new("/Users/a/Downloads/My File.zip")).expect("a command");
        let (program, args) = spelling(&cmd);
        assert_eq!(program, "open");
        // -R selects the file in Finder; without it `open` would RUN it.
        assert_eq!(args, ["-R", "/Users/a/Downloads/My File.zip"]);
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
        let cmd = reveal_command(Path::new("/home/a/My Downloads/naïve (1).zip")).expect("cmd");
        let (program, args) = spelling(&cmd);
        assert_eq!(program, "dbus-send");
        assert!(args.contains(&"org.freedesktop.FileManager1.ShowItems".to_string()));
        assert!(args.contains(
            &"array:string:file:///home/a/My%20Downloads/na%C3%AFve%20%281%29.zip".to_string()
        ));
        // ShowItems takes a startup id as its second argument; an empty one
        // is still an argument, and omitting it fails the call.
        assert_eq!(args.last().map(String::as_str), Some("string:"));
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
}
