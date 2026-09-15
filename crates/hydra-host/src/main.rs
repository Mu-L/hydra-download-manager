// Copyright (C) 2026 Javad Rajabzadeh
// SPDX-License-Identifier: GPL-3.0-or-later

//! Chrome/Firefox native-messaging host for Hydra.
//!
//! The browser spawns this binary and speaks the native-messaging framing on
//! stdio: 4-byte little-endian length, then a JSON document, each way. Every
//! request is forwarded to the running hydra-gui over the loopback socket it
//! publishes in `<app_dir>/ipc.json` (adding the secret token from that
//! file — the browser never sees it), and the GUI's reply is framed back.
//!
//! When the GUI is not running the host launches it minimized and waits for
//! the socket to appear, so clicking a download in the browser "just works"
//! exactly like monitor.

use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpStream;
use std::path::PathBuf;
use std::time::{Duration, Instant};

/// Pointer file a portable (`--config DIR`) instance writes next to this
/// binary when it takes browser capture over; see `nmhost::ensure_registered`
/// in hydra-gui. One line: the absolute application directory.
const PROFILE_POINTER: &str = "hydra-profile";

/// The `--config DIR` this host should talk to, if any.
///
/// A native-messaging manifest carries a path and no arguments, so the
/// browser can say nothing about which profile it wants: the answer has to
/// be found beside us. `HYDRA_CONFIG` is the explicit override — a script, a
/// second profile, a checkout — and the pointer file is what a portable copy
/// leaves next to its own `hydra-host`, so the answer travels with the copy
/// rather than living in the profile it names.
///
/// A directory that is not there reads as no override: an unplugged USB
/// stick must leave the browser reaching an ordinary install rather than
/// creating an empty profile somewhere the user is not looking.
fn portable_dir() -> Option<PathBuf> {
    let exe_dir = std::env::current_exe().ok()?.parent()?.to_path_buf();
    resolve_profile(
        std::env::var_os("HYDRA_CONFIG"),
        &exe_dir.join(PROFILE_POINTER),
    )
}

/// The profile `HYDRA_CONFIG` or `pointer` names, or None when neither names
/// a directory that is there. The variable wins: it is the explicit answer
/// for this one process, while the file is whatever was left beside us.
fn resolve_profile(env: Option<std::ffi::OsString>, pointer: &std::path::Path) -> Option<PathBuf> {
    let dir = match env {
        Some(dir) => PathBuf::from(dir),
        None => PathBuf::from(std::fs::read_to_string(pointer).ok()?.trim()),
    };
    (dir.is_absolute() && dir.is_dir()).then_some(dir)
}

fn app_dir() -> PathBuf {
    if let Some(dir) = portable_dir() {
        return dir;
    }
    #[cfg(target_os = "windows")]
    {
        std::env::var_os("APPDATA")
            .map(PathBuf::from)
            .unwrap_or_else(std::env::temp_dir)
            .join("hydra")
    }
    #[cfg(not(target_os = "windows"))]
    {
        PathBuf::from(std::env::var_os("HOME").unwrap_or_default())
            .join(".config")
            .join("hydra")
    }
}

/// (port, token) out of an ipc.json body. A file from a crashed run, a
/// half-written one, or one from a build that spelled the fields
/// differently all read as "no instance" rather than as a bad address.
fn parse_ipc(text: &str) -> Option<(u16, String)> {
    let v: serde_json::Value = serde_json::from_str(text).ok()?;
    let port = u16::try_from(v.get("port")?.as_u64()?).ok()?;
    let token = v.get("token")?.as_str()?.to_string();
    Some((port, token))
}

/// (port, token) from ipc.json, if the file exists and parses.
fn read_ipc() -> Option<(u16, String)> {
    parse_ipc(&std::fs::read_to_string(app_dir().join("ipc.json")).ok()?)
}

/// Try to connect to the GUI right now. The file may be stale from a
/// previous run, so a parse success still has to survive the connect.
fn connect_once() -> Option<(TcpStream, String)> {
    let (port, token) = read_ipc()?;
    let stream = TcpStream::connect_timeout(
        &std::net::SocketAddr::from(([127, 0, 0, 1], port)),
        Duration::from_millis(600),
    )
    .ok()?;
    stream.set_nodelay(true).ok();
    Some((stream, token))
}

/// A minimized GUI launch, stdio detached from ours.
///
/// `profile` is the `--config DIR` this host resolved: the launched app has
/// to come up on the SAME profile, or the browser would start an ordinary
/// instance and then fail to find the socket it is waiting for.
fn gui_command(
    program: &std::ffi::OsStr,
    profile: Option<&std::path::Path>,
) -> std::process::Command {
    let mut cmd = std::process::Command::new(program);
    cmd.arg("--minimized");
    if let Some(dir) = profile {
        cmd.arg("--config").arg(dir);
    }
    cmd.stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null());
    cmd
}

/// Detached spawn of a GUI binary; true when the process started.
///
/// Windows: the browser runs a native-messaging host inside a JOB OBJECT and
/// kills the job when the host exits — which, for the one-shot
/// `sendNativeMessage` the extension calls us through, is the moment we
/// answer. Every process we started goes with us, so the GUI launched for
/// this very capture died seconds after starting, before any window of it
/// appeared. `CREATE_BREAKAWAY_FROM_JOB` is what Mozilla and Chrome
/// prescribe for children that must outlive the host; `DETACHED_PROCESS`
/// keeps the GUI off the console the browser gave us.
fn spawn_direct(program: std::ffi::OsString, profile: Option<&std::path::Path>) -> bool {
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_BREAKAWAY_FROM_JOB: u32 = 0x0100_0000;
        const DETACHED_PROCESS: u32 = 0x0000_0008;
        if gui_command(&program, profile)
            .creation_flags(CREATE_BREAKAWAY_FROM_JOB | DETACHED_PROCESS)
            .spawn()
            .is_ok()
        {
            return true;
        }
        // A job created without JOB_OBJECT_LIMIT_BREAKAWAY_OK refuses the
        // flag outright (ERROR_ACCESS_DENIED) rather than ignoring it. Fall
        // back to an ordinary spawn — the pre-existing behaviour, and still
        // the right answer on any browser that runs us outside a job.
    }
    gui_command(&program, profile).spawn().is_ok()
}

/// `hydra-gui` next to this binary, which is where every packaging layout
/// (and a dev `target/release`) puts the pair.
fn gui_sibling() -> Option<PathBuf> {
    let dir = std::env::current_exe().ok()?.parent()?.to_path_buf();
    let path = dir.join(if cfg!(windows) {
        "hydra-gui.exe"
    } else {
        "hydra-gui"
    });
    path.exists().then_some(path)
}

/// Launch hydra-gui minimized: the capture dialog is the only surface that
/// should appear. On macOS the app bundle comes first — it carries the TCC
/// identity the user granted folder access to; a raw sibling binary would
/// hit EACCES on ~/Downloads. `open -ga` exits non-zero when the app is not
/// installed, so its exit status (not spawn success) is the real signal.
fn launch_gui() {
    let profile = portable_dir();
    let dir = profile.as_deref();
    if let Some(p) = std::env::var_os("HYDRA_GUI_BIN") {
        if spawn_direct(p, dir) {
            return;
        }
    }
    let sibling = gui_sibling();
    // A portable copy keeps hydra-gui beside this binary, and that build is
    // the one whose profile we are pointing at. It goes first: an installed
    // app bundle found by name would come up on the ordinary profile and
    // never publish the socket this host is waiting for.
    if profile.is_some() {
        if let Some(path) = sibling.clone() {
            if spawn_direct(path.into_os_string(), dir) {
                return;
            }
        }
    }
    #[cfg(target_os = "macos")]
    {
        let mut args = vec!["-ga", "Hydra Download Manager", "--args", "--minimized"];
        let profile_arg = profile.as_ref().map(|p| p.to_string_lossy().into_owned());
        if let Some(p) = profile_arg.as_deref() {
            args.extend_from_slice(&["--config", p]);
        }
        let ok = std::process::Command::new("open")
            .args(&args)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .map(|s| s.success())
            .unwrap_or(false);
        if ok {
            return;
        }
    }
    // Dev layout: hydra-host sits next to hydra-gui in target/release.
    if let Some(path) = sibling {
        if spawn_direct(path.into_os_string(), dir) {
            return;
        }
    }
    spawn_direct("hydra-gui".into(), dir);
}

/// Connect, launching the GUI and polling if needed.
fn connect(launch: bool) -> Option<(TcpStream, String)> {
    if let Some(c) = connect_once() {
        return Some(c);
    }
    if !launch {
        return None;
    }
    launch_gui();
    let deadline = Instant::now() + Duration::from_secs(20);
    while Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(300));
        if let Some(c) = connect_once() {
            return Some(c);
        }
    }
    None
}

/// One native-messaging frame from the browser. None on clean EOF.
fn read_frame(stdin: &mut impl Read) -> Option<Vec<u8>> {
    let mut len = [0u8; 4];
    stdin.read_exact(&mut len).ok()?;
    let len = u32::from_le_bytes(len) as usize;
    // Chrome caps extension->host messages well below this; anything larger
    // is framing corruption, and exiting lets the browser respawn us.
    if len == 0 || len > 64 * 1024 * 1024 {
        return None;
    }
    let mut buf = vec![0u8; len];
    stdin.read_exact(&mut buf).ok()?;
    Some(buf)
}

fn write_frame(stdout: &mut impl Write, payload: &[u8]) {
    let _ = stdout.write_all(&(payload.len() as u32).to_le_bytes());
    let _ = stdout.write_all(payload);
    let _ = stdout.flush();
}

fn error_reply(msg: &str) -> Vec<u8> {
    format!("{{\"ok\":false,\"error\":\"{msg}\"}}").into_bytes()
}

/// Send one request over an established GUI connection, returning the reply
/// line. Any IO failure returns None so the caller can reconnect once.
fn round_trip(conn: &mut (TcpStream, String), req: &mut serde_json::Value) -> Option<String> {
    req["token"] = serde_json::Value::String(conn.1.clone());
    let mut line = serde_json::to_string(req).ok()?;
    line.push('\n');
    conn.0.write_all(line.as_bytes()).ok()?;
    let mut reader = BufReader::new(conn.0.try_clone().ok()?);
    let mut reply = String::new();
    reader.read_line(&mut reply).ok()?;
    if reply.trim().is_empty() {
        return None;
    }
    Some(reply)
}

fn main() {
    let mut stdin = std::io::stdin().lock();
    let mut stdout = std::io::stdout().lock();
    // One GUI connection kept across frames: connectNative ports send many
    // requests through a single host process.
    let mut conn: Option<(TcpStream, String)> = None;

    while let Some(frame) = read_frame(&mut stdin) {
        let mut req: serde_json::Value = match serde_json::from_slice(&frame) {
            Ok(serde_json::Value::Object(o)) => serde_json::Value::Object(o),
            _ => {
                write_frame(&mut stdout, &error_reply("bad json"));
                continue;
            }
        };
        // Pings probe state; they must not boot the app. Everything else
        // (a capture the browser already cancelled!) must reach a GUI.
        let launch = req.get("type").and_then(|t| t.as_str()) != Some("ping");

        let mut reply = None;
        for attempt in 0..2 {
            if conn.is_none() {
                conn = connect(launch && attempt == 0);
            }
            let Some(c) = conn.as_mut() else { break };
            match round_trip(c, &mut req) {
                Some(r) => {
                    reply = Some(r);
                    break;
                }
                // Stale connection (GUI restarted): drop and retry fresh.
                None => conn = None,
            }
        }
        match reply {
            Some(r) => write_frame(&mut stdout, r.trim().as_bytes()),
            None => write_frame(&mut stdout, &error_reply("hydra is not running")),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::TcpListener;

    /// ipc.json outlives the instance that wrote it, so the file alone is
    /// never the answer to "is hydra running": the port has to accept a
    /// connection, or some unrelated process that inherited the number
    /// would be handed the user's downloads.
    ///
    /// Sets HOME/APPDATA, which `app_dir` reads and nothing else in this
    /// binary does.
    #[test]
    fn a_stale_ipc_file_is_not_mistaken_for_a_running_app() {
        let home = std::env::temp_dir().join(format!("hydra-host-{}", std::process::id()));
        let cfg = home.join(".config");
        std::fs::create_dir_all(cfg.join("hydra")).expect("test app dir");
        std::env::set_var("HOME", &home);
        std::env::set_var("APPDATA", &cfg);
        let publish = |port: u16| {
            std::fs::write(
                app_dir().join("ipc.json"),
                format!(r#"{{"port":{port},"token":"s3cret"}}"#),
            )
            .expect("write ipc.json");
        };

        // A port nobody is listening on any more: the file is what a
        // crashed instance leaves behind.
        //
        // Re-rolled rather than trusted once, because a released ephemeral
        // port is free only until something takes it — and the tests above
        // bind port 0 on the same loopback, from the same binary, at the same
        // moment. Handed back the number we had just let go, the connect
        // succeeds and the test reads a race as the defect. The property under
        // test is about a file naming a port with no listener, not about any
        // one number, so a port that turns out to be listening is grounds for
        // another draw. A file that is trusted WITHOUT connecting fails every
        // draw, which is the regression this is here to catch.
        let stale_reads_as_no_instance = (0..32).any(|_| {
            let dead = {
                let l = TcpListener::bind(("127.0.0.1", 0)).expect("free port");
                l.local_addr().expect("address").port()
            };
            publish(dead);
            connect(false).is_none()
        });
        assert!(
            stale_reads_as_no_instance,
            "a stale file must not read as a running app"
        );

        let live = TcpListener::bind(("127.0.0.1", 0)).expect("loopback listener");
        publish(live.local_addr().expect("address").port());
        let (_stream, token) = connect(false).expect("a listening instance is reachable");
        assert_eq!(token, "s3cret", "the token travels with the connection");

        let _ = std::fs::remove_dir_all(&home);
    }

    /// The pointer file (and `HYDRA_CONFIG`) is how a portable copy tells
    /// its own hydra-host which profile to talk to. Everything either could
    /// hold that is not a directory here and now has to read as "no
    /// override", or the browser stops reaching the ordinary install for no
    /// visible reason.
    #[test]
    fn a_profile_answers_only_when_it_names_a_directory_that_is_there() {
        let base = std::env::temp_dir().join(format!("hydra-pointer-{}", std::process::id()));
        let profile = base.join("data");
        std::fs::create_dir_all(&profile).expect("profile dir");
        let pointer = base.join(PROFILE_POINTER);
        let write = |body: &str| std::fs::write(&pointer, body).expect("write pointer");
        let resolved = |p: &std::path::Path| resolve_profile(None, p);

        write(&format!("{}\n", profile.display()));
        assert_eq!(resolved(&pointer), Some(profile.clone()));

        write(&base.join("gone").to_string_lossy());
        assert_eq!(resolved(&pointer), None, "an unplugged stick");
        write("");
        assert_eq!(resolved(&pointer), None, "an empty file");
        write("data");
        assert_eq!(resolved(&pointer), None, "a relative path");
        std::fs::remove_file(&pointer).expect("remove pointer");
        assert_eq!(resolved(&pointer), None, "no file at all");

        // HYDRA_CONFIG is the explicit answer and outranks the file, but is
        // held to the same test: a directory that is not there is not a
        // profile, whoever named it.
        write(&format!("{}\n", profile.display()));
        let env = |v: &str| resolve_profile(Some(v.into()), &pointer);
        assert_eq!(env(&base.to_string_lossy()), Some(base.clone()));
        assert_eq!(env(&base.join("gone").to_string_lossy()), None);

        std::fs::remove_dir_all(&base).ok();
    }

    /// The launched app must come up on the profile this host resolved.
    /// Without `--config` it publishes ipc.json in the default directory,
    /// which is not the one the connect loop is watching — the browser then
    /// waits out the full timeout and reports Hydra as unreachable while a
    /// window of it is on screen.
    #[test]
    fn a_portable_launch_carries_the_profile_it_resolved() {
        let dir = std::path::Path::new("/opt/hydra-portable/data");
        let args = |profile| {
            gui_command(std::ffi::OsStr::new("hydra-gui"), profile)
                .get_args()
                .map(|a| a.to_string_lossy().into_owned())
                .collect::<Vec<_>>()
        };
        assert_eq!(args(None), ["--minimized"]);
        assert_eq!(
            args(Some(dir)),
            ["--minimized", "--config", "/opt/hydra-portable/data"]
        );
    }

    /// The framing is the whole contract with the browser: four little-endian
    /// bytes, then exactly that many bytes of JSON.
    #[test]
    fn a_frame_survives_the_round_trip() {
        let payload = br#"{"type":"download","url":"https://example.invalid/f.zip"}"#;
        let mut wire = Vec::new();
        write_frame(&mut wire, payload);

        assert_eq!(&wire[..4], &(payload.len() as u32).to_le_bytes());
        assert_eq!(read_frame(&mut &wire[..]).as_deref(), Some(&payload[..]));
    }

    /// Anything the browser could not have sent ends the session instead of
    /// being guessed at — the browser then respawns us with a clean stream.
    #[test]
    fn a_frame_the_browser_could_not_have_sent_is_refused() {
        let framed = |len: u32, body: &[u8]| {
            let mut v = len.to_le_bytes().to_vec();
            v.extend_from_slice(body);
            v
        };

        assert_eq!(read_frame(&mut &[][..]), None, "clean EOF");
        assert_eq!(read_frame(&mut &[0u8, 1][..]), None, "half a length");
        assert_eq!(read_frame(&mut &framed(0, b"")[..]), None, "empty message");
        assert_eq!(
            read_frame(&mut &framed(64 * 1024 * 1024 + 1, b"")[..]),
            None,
            "a length no legitimate message has"
        );
        assert_eq!(
            read_frame(&mut &framed(8, b"short")[..]),
            None,
            "body shorter than its own length"
        );
    }

    /// ipc.json is the only thing standing between us and connecting to a
    /// port some unrelated process now owns, so a file that does not name a
    /// real endpoint has to read as "no instance".
    #[test]
    fn ipc_json_answers_only_when_it_names_a_real_endpoint() {
        assert_eq!(
            parse_ipc(r#"{"port":50726,"token":"cafe","ws_port":6799,"pid":42}"#),
            Some((50726, "cafe".to_string()))
        );
        for bad in [
            r#"{"port":50726}"#,                  // no token
            r#"{"token":"cafe"}"#,                // no port
            r#"{"port":"50726","token":"cafe"}"#, // port as a string
            r#"{"port":70000,"token":"cafe"}"#,   // beyond a port number
            r#"{"port":50726,"token":7}"#,        // token as a number
            "{\"port\":50726,",                   // half-written file
            "",
        ] {
            assert_eq!(parse_ipc(bad), None, "should be unusable: {bad}");
        }
    }

    /// The token lives in a file only this user can read; the browser never
    /// sees it and never sends it. Stamping it on is this process's whole
    /// reason for existing.
    #[test]
    fn a_request_is_stamped_with_the_token_the_browser_never_sees() {
        let listener = TcpListener::bind(("127.0.0.1", 0)).expect("loopback listener");
        let addr = listener.local_addr().expect("listener address");
        let gui = std::thread::spawn(move || {
            let (stream, _) = listener.accept().expect("the host connects");
            let mut out = stream.try_clone().expect("write half");
            let mut line = String::new();
            BufReader::new(stream)
                .read_line(&mut line)
                .expect("one request line");
            writeln!(out, r#"{{"ok":true,"capture":true}}"#).expect("reply");
            line
        });

        let mut conn = (
            TcpStream::connect(addr).expect("connect to the fake gui"),
            "s3cret".to_string(),
        );
        let mut req =
            serde_json::json!({"type": "download", "url": "https://example.invalid/f.zip"});
        assert!(req.get("token").is_none(), "the browser sends no token");

        let reply = round_trip(&mut conn, &mut req).expect("a reply line");
        assert_eq!(reply.trim(), r#"{"ok":true,"capture":true}"#);

        let on_the_wire: serde_json::Value =
            serde_json::from_str(&gui.join().expect("gui thread")).expect("json request");
        assert_eq!(on_the_wire["token"], "s3cret");
        assert_eq!(on_the_wire["url"], "https://example.invalid/f.zip");
    }

    /// The browser gets a JSON object even when there is nothing to talk to;
    /// the extension reads `ok` off it and keeps its own download.
    #[test]
    fn an_unreachable_app_still_answers_in_json() {
        let reply: serde_json::Value =
            serde_json::from_slice(&error_reply("hydra is not running")).expect("json");
        assert_eq!(reply["ok"], false);
        assert_eq!(reply["error"], "hydra is not running");
    }

    /// `spawn_direct` reports whether a process actually started — that is
    /// what `launch_gui` walks its candidate paths on. The current test
    /// binary stands in for the GUI: it exists on every platform this
    /// builds for, and exits immediately on an argument it has no test for.
    #[test]
    fn a_gui_launch_reports_whether_the_process_started() {
        let me = std::env::current_exe().expect("test binary path");
        assert!(
            spawn_direct(me.into_os_string(), None),
            "a real program starts"
        );
        assert!(
            !spawn_direct(
                std::env::temp_dir()
                    .join("hydra-gui-that-is-not-here")
                    .into_os_string(),
                None
            ),
            "a missing program does not"
        );
    }
}
