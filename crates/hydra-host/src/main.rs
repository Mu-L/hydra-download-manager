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
//!
//! One request is answered here rather than forwarded: `{"type":"ws-token"}`
//! hands the extension the WebSocket port and token out of ipc.json, once a
//! ping has shown the GUI is live. The browser lets only the extensions the
//! host manifest allow-lists reach this process, so holding the token is
//! what proves an extension is Hydra's own on the WebSocket.

use std::ffi::{OsStr, OsString};
use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpStream;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

/// How long one request may wait on the GUI. It has to exceed the GUI's
/// extbus ACK_TIMEOUT (10 s) — a capture is acknowledged only once the
/// dialog has taken it — while still bounding the wait on a port that some
/// unrelated process inherited from a stale ipc.json.
const REPLY_TIMEOUT: Duration = Duration::from_secs(15);

/// How long a freshly launched GUI gets to publish its socket.
const LAUNCH_BUDGET: Duration = Duration::from_secs(20);

/// Product name the macOS bundle is registered under with LaunchServices.
#[cfg(any(target_os = "macos", test))]
const MACOS_APP_NAME: &str = "Hydra Download Manager";

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
fn resolve_profile(env: Option<OsString>, pointer: &Path) -> Option<PathBuf> {
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

/// What the GUI publishes in ipc.json.
#[derive(Debug, PartialEq, Eq)]
struct Ipc {
    port: u16,
    token: String,
    /// The extension-facing WebSocket port; absent when both fixed ports
    /// were taken and the GUI came up without one.
    ws_port: Option<u16>,
}

/// An ipc.json body. A file from a crashed run, a half-written one, or one
/// from a build that spelled the fields differently all read as "no
/// instance" rather than as a bad address.
fn parse_ipc(text: &str) -> Option<Ipc> {
    let v: serde_json::Value = serde_json::from_str(text).ok()?;
    let port = u16::try_from(v.get("port")?.as_u64()?).ok()?;
    let token = v.get("token")?.as_str()?.to_string();
    let ws_port = v
        .get("ws_port")
        .and_then(|p| p.as_u64())
        .and_then(|p| u16::try_from(p).ok());
    Some(Ipc {
        port,
        token,
        ws_port,
    })
}

fn read_ipc(path: &Path) -> Option<Ipc> {
    parse_ipc(&std::fs::read_to_string(path).ok()?)
}

/// Try to connect to the GUI right now. The file may be stale from a
/// previous run, so a parse success still has to survive the connect — and
/// a port that already proved to accept connections without ever answering
/// (`stale`) is skipped outright, or the retry would sit through the same
/// timeout again instead of launching the app.
fn connect_once(
    ipc: &Path,
    stale: Option<u16>,
    reply_timeout: Duration,
) -> Option<(TcpStream, String)> {
    let Ipc { port, token, .. } = read_ipc(ipc)?;
    if stale == Some(port) {
        return None;
    }
    let stream = TcpStream::connect_timeout(
        &std::net::SocketAddr::from(([127, 0, 0, 1], port)),
        Duration::from_millis(600),
    )
    .ok()?;
    stream.set_nodelay(true).ok();
    stream.set_read_timeout(Some(reply_timeout)).ok();
    stream.set_write_timeout(Some(reply_timeout)).ok();
    Some((stream, token))
}

/// A minimized GUI launch, stdio detached from ours.
///
/// `profile` is the `--config DIR` this host resolved: the launched app has
/// to come up on the SAME profile, or the browser would start an ordinary
/// instance and then fail to find the socket it is waiting for.
fn gui_command(program: &OsStr, profile: Option<&Path>) -> std::process::Command {
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
fn spawn_direct(program: OsString, profile: Option<&Path>) -> bool {
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

/// The `.app` bundle `path` sits inside, if any: the nearest enclosing
/// directory named `*.app`.
#[cfg(any(target_os = "macos", test))]
fn app_bundle_of(path: &Path) -> Option<PathBuf> {
    path.ancestors()
        .find(|p| p.extension().is_some_and(|e| e.eq_ignore_ascii_case("app")))
        .map(Path::to_path_buf)
}

/// Arguments for macOS `open`: the bundle this host ships in when it is
/// inside one, else the product name. LaunchServices resolves a name to
/// whichever copy it registered last, so with a second Hydra bundle on the
/// disk — an older version in ~/Applications, a build in a checkout — the
/// name can start the wrong one, which then publishes no socket on the
/// profile this host is watching. `-g` keeps the launch in the background
/// either way: the capture dialog is the only surface that should appear.
#[cfg(any(target_os = "macos", test))]
fn open_args(bundle: Option<&Path>, profile: Option<&Path>) -> Vec<OsString> {
    let mut args: Vec<OsString> = match bundle {
        Some(b) => vec!["-g".into(), b.into()],
        None => vec!["-ga".into(), MACOS_APP_NAME.into()],
    };
    args.extend(["--args".into(), "--minimized".into()]);
    if let Some(p) = profile {
        args.extend(["--config".into(), p.into()]);
    }
    args
}

/// Launch hydra-gui minimized; true when something was started. On macOS
/// the app bundle comes first — it carries the TCC identity the user granted
/// folder access to; a raw sibling binary would hit EACCES on ~/Downloads.
/// `open` exits non-zero when the app is not installed, so its exit status
/// (not spawn success) is the real signal.
fn launch_gui() -> bool {
    let profile = portable_dir();
    let dir = profile.as_deref();
    if let Some(p) = std::env::var_os("HYDRA_GUI_BIN") {
        if spawn_direct(p, dir) {
            return true;
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
                return true;
            }
        }
    }
    #[cfg(target_os = "macos")]
    {
        let bundle = std::env::current_exe()
            .ok()
            .and_then(|exe| app_bundle_of(&exe));
        let ok = std::process::Command::new("open")
            .args(open_args(bundle.as_deref(), dir))
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .map(|s| s.success())
            .unwrap_or(false);
        if ok {
            return true;
        }
    }
    // Dev layout: hydra-host sits next to hydra-gui in target/release.
    if let Some(path) = sibling {
        if spawn_direct(path.into_os_string(), dir) {
            return true;
        }
    }
    spawn_direct("hydra-gui".into(), dir)
}

/// Why no GUI connection could be made. The two are different answers for
/// the browser: a launch that has not published its socket within the
/// budget is still coming up, and the extension can offer the download
/// again once its WebSocket attaches; an app that could not be started at
/// all will not.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Unreachable {
    NotRunning,
    Starting,
}

impl Unreachable {
    fn message(self) -> &'static str {
        match self {
            Unreachable::NotRunning => "hydra is not running",
            Unreachable::Starting => "hydra is starting",
        }
    }
}

/// The bridge to one GUI: where its ipc.json is, the connection kept across
/// frames (connectNative ports send many requests through a single host
/// process), and what this process has learned about ports that are not it.
struct Host {
    ipc: PathBuf,
    conn: Option<(TcpStream, String)>,
    /// A port that took a request and never answered. Kept for the life of
    /// this process so a later frame does not sit through the same timeout.
    stale: Option<u16>,
    reply_timeout: Duration,
    launch_budget: Duration,
}

impl Host {
    fn new(ipc: PathBuf) -> Host {
        Host {
            ipc,
            conn: None,
            stale: None,
            reply_timeout: REPLY_TIMEOUT,
            launch_budget: LAUNCH_BUDGET,
        }
    }

    /// Connect, launching the GUI and polling for up to the budget if
    /// `launch` allows.
    fn connect(&self, launch: bool) -> Result<(TcpStream, String), Unreachable> {
        let dial = || connect_once(&self.ipc, self.stale, self.reply_timeout);
        if let Some(c) = dial() {
            return Ok(c);
        }
        if !launch || !launch_gui() {
            return Err(Unreachable::NotRunning);
        }
        let deadline = Instant::now() + self.launch_budget;
        while Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(300));
            if let Some(c) = dial() {
                return Ok(c);
            }
        }
        Err(Unreachable::Starting)
    }

    /// Forward one request and return the GUI's reply line. One reconnect is
    /// allowed: the connection may be from before a GUI restart, or the port
    /// may turn out not to be hydra at all, in which case the retry skips it
    /// and (for anything but a ping) launches the app.
    fn dispatch(
        &mut self,
        req: &mut serde_json::Value,
        launch: bool,
    ) -> Result<String, Unreachable> {
        let mut failure = Unreachable::NotRunning;
        for _ in 0..2 {
            if self.conn.is_none() {
                match self.connect(launch) {
                    Ok(c) => self.conn = Some(c),
                    Err(why) => {
                        failure = why;
                        break;
                    }
                }
            }
            let Some(c) = self.conn.as_mut() else { break };
            match round_trip(c, req) {
                Round::Reply(r) => return Ok(r),
                Round::Dropped => self.conn = None,
                Round::Unanswered => {
                    self.stale = c.0.peer_addr().ok().map(|a| a.port());
                    self.conn = None;
                }
            }
        }
        Err(failure)
    }

    /// The WebSocket port and token for the extension, from the file the
    /// browser cannot read. Only for a GUI that is up and answering with
    /// this very token: a stale file would otherwise hand out a port some
    /// other process now owns. Never launches — the extension asks when its
    /// socket opened, so the app is there or the question is moot.
    fn ws_token(&mut self) -> Vec<u8> {
        let mut ping = serde_json::json!({"type": "ping"});
        let live = match self.dispatch(&mut ping, false) {
            Ok(reply) => serde_json::from_str::<serde_json::Value>(&reply)
                .ok()
                .is_some_and(|r| r["ok"] == true),
            Err(why) => return error_reply(why.message()),
        };
        let Some(ipc) = read_ipc(&self.ipc).filter(|_| live) else {
            return error_reply(Unreachable::NotRunning.message());
        };
        match ipc.ws_port {
            Some(ws_port) => {
                serde_json::json!({"ok": true, "ws_port": ws_port, "token": ipc.token})
                    .to_string()
                    .into_bytes()
            }
            None => error_reply("hydra has no websocket port"),
        }
    }
}

/// The reply frame for one request from the browser.
fn handle(host: &mut Host, req: &mut serde_json::Value) -> Vec<u8> {
    match req.get("type").and_then(|t| t.as_str()) {
        Some("ws-token") => host.ws_token(),
        // Pings probe state; they must not boot the app. Everything else
        // (a capture the browser already cancelled!) must reach a GUI.
        kind => match host.dispatch(req, kind != Some("ping")) {
            Ok(r) => r.trim().as_bytes().to_vec(),
            Err(why) => error_reply(why.message()),
        },
    }
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

/// What one request over an established GUI connection came back with.
#[derive(Debug, PartialEq, Eq)]
enum Round {
    /// A JSON object line from the GUI.
    Reply(String),
    /// The connection went away (the GUI restarted, or the write failed):
    /// worth one reconnect through ipc.json.
    Dropped,
    /// The peer took the request and sent nothing usable back within
    /// [`REPLY_TIMEOUT`] — a port some other process now owns, which is not
    /// hydra whatever the file says.
    Unanswered,
}

fn round_trip(conn: &mut (TcpStream, String), req: &mut serde_json::Value) -> Round {
    req["token"] = serde_json::Value::String(conn.1.clone());
    let Ok(mut line) = serde_json::to_string(req) else {
        return Round::Dropped;
    };
    line.push('\n');
    let Ok(clone) = conn.0.try_clone() else {
        return Round::Dropped;
    };
    if let Err(e) = conn.0.write_all(line.as_bytes()) {
        return if is_timeout(&e) {
            Round::Unanswered
        } else {
            Round::Dropped
        };
    }
    let mut reply = String::new();
    match BufReader::new(clone).read_line(&mut reply) {
        Ok(0) => return Round::Dropped,
        Ok(_) => {}
        Err(e) if is_timeout(&e) => return Round::Unanswered,
        Err(_) => return Round::Dropped,
    }
    match serde_json::from_str::<serde_json::Value>(reply.trim()) {
        Ok(serde_json::Value::Object(_)) => Round::Reply(reply),
        _ => Round::Unanswered,
    }
}

/// A read or write that hit the socket timeout. Unix reports it as
/// `WouldBlock`, Windows as `TimedOut`.
fn is_timeout(e: &std::io::Error) -> bool {
    matches!(
        e.kind(),
        std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
    )
}

fn main() {
    let mut stdin = std::io::stdin().lock();
    let mut stdout = std::io::stdout().lock();
    let mut host = Host::new(app_dir().join("ipc.json"));

    while let Some(frame) = read_frame(&mut stdin) {
        let mut req: serde_json::Value = match serde_json::from_slice(&frame) {
            Ok(serde_json::Value::Object(o)) => serde_json::Value::Object(o),
            _ => {
                write_frame(&mut stdout, &error_reply("bad json"));
                continue;
            }
        };
        write_frame(&mut stdout, &handle(&mut host, &mut req));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::TcpListener;

    /// A private ipc.json for one test, so tests never share a file.
    fn ipc_file(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("hydra-host-{tag}-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("test app dir");
        dir.join("ipc.json")
    }

    fn publish(ipc: &Path, port: u16) {
        std::fs::write(ipc, format!(r#"{{"port":{port},"token":"s3cret"}}"#))
            .expect("write ipc.json");
    }

    /// A fake GUI on a loopback port that answers every request line with
    /// `reply` and reports what it was sent; `n` requests, then it exits.
    fn fake_gui(reply: &'static str, n: usize) -> (u16, std::thread::JoinHandle<Vec<String>>) {
        let listener = TcpListener::bind(("127.0.0.1", 0)).expect("loopback listener");
        let port = listener.local_addr().expect("address").port();
        let served = std::thread::spawn(move || {
            let (stream, _) = listener.accept().expect("the host connects");
            let mut out = stream.try_clone().expect("write half");
            let mut reader = BufReader::new(stream);
            let mut lines = Vec::new();
            for _ in 0..n {
                let mut line = String::new();
                if reader.read_line(&mut line).unwrap_or(0) == 0 {
                    break;
                }
                writeln!(out, "{reply}").expect("reply");
                lines.push(line);
            }
            lines
        });
        (port, served)
    }

    /// ipc.json outlives the instance that wrote it, so the file alone is
    /// never the answer to "is hydra running": the port has to accept a
    /// connection, or some unrelated process that inherited the number
    /// would be handed the user's downloads.
    /// A host whose launch never waits, for tests that must not sit through
    /// the real budget.
    fn quick_host(ipc: &Path) -> Host {
        Host {
            launch_budget: Duration::ZERO,
            reply_timeout: Duration::from_millis(200),
            ..Host::new(ipc.to_path_buf())
        }
    }

    #[test]
    fn a_stale_ipc_file_is_not_mistaken_for_a_running_app() {
        let ipc = ipc_file("stale");
        let mut host = quick_host(&ipc);
        host.reply_timeout = REPLY_TIMEOUT;

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
            publish(&ipc, dead);
            host.connect(false).err() == Some(Unreachable::NotRunning)
        });
        assert!(
            stale_reads_as_no_instance,
            "a stale file must not read as a running app"
        );

        let live = TcpListener::bind(("127.0.0.1", 0)).expect("loopback listener");
        let port = live.local_addr().expect("address").port();
        publish(&ipc, port);
        let (stream, token) = host
            .connect(false)
            .expect("a listening instance is reachable");
        assert_eq!(token, "s3cret", "the token travels with the connection");
        assert_eq!(
            stream.read_timeout().expect("query timeout"),
            Some(REPLY_TIMEOUT),
            "no request may wait on the peer forever"
        );

        // The same live port, once it has proven not to be hydra, is skipped
        // rather than dialled a second time.
        host.stale = Some(port);
        assert_eq!(host.connect(false).err(), Some(Unreachable::NotRunning));

        let _ = std::fs::remove_dir_all(ipc.parent().expect("dir"));
    }

    /// A stale ipc.json can name a port that some unrelated process now
    /// listens on. It accepts the connection and then says nothing — or
    /// something that is not JSON — and before the socket carried a
    /// timeout the host blocked in `read_line` for good, with the browser's
    /// download parked behind it.
    #[test]
    fn a_port_that_accepts_but_never_answers_is_not_hydra() {
        let silent = TcpListener::bind(("127.0.0.1", 0)).expect("loopback listener");
        let silent_addr = silent.local_addr().expect("address");
        let chatty = TcpListener::bind(("127.0.0.1", 0)).expect("loopback listener");
        let chatty_addr = chatty.local_addr().expect("address");
        let peers = std::thread::spawn(move || {
            let (held, _) = silent.accept().expect("the host connects");
            let (mut other, _) = chatty.accept().expect("the host connects");
            let _ = other.write_all(b"220 some ftp daemon ready\r\n");
            // Held open until the host has given up on it.
            std::thread::sleep(Duration::from_millis(600));
            drop(held);
        });

        let dial = |addr: std::net::SocketAddr| {
            let s = TcpStream::connect(addr).expect("connect");
            s.set_read_timeout(Some(Duration::from_millis(200)))
                .expect("short timeout for the test");
            (s, "s3cret".to_string())
        };
        let mut req = serde_json::json!({"type": "download", "url": "https://x.invalid/f"});

        let mut conn = dial(silent_addr);
        let started = Instant::now();
        assert_eq!(round_trip(&mut conn, &mut req), Round::Unanswered);
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "the timeout is what ended the wait"
        );

        let mut conn = dial(chatty_addr);
        assert_eq!(
            round_trip(&mut conn, &mut req),
            Round::Unanswered,
            "a non-JSON greeting is not a hydra reply"
        );
        peers.join().expect("peer thread");
    }

    /// A GUI that was started but has not published its socket within the
    /// budget is "starting", not "not running": the browser can offer the
    /// parked download again once its own socket attaches, and must not
    /// report a dead install to the user while a window is coming up.
    ///
    /// Sets HYDRA_GUI_BIN, which only `launch_gui` reads.
    #[test]
    fn a_launch_still_coming_up_reads_as_starting() {
        let ipc = ipc_file("starting");
        let me = std::env::current_exe().expect("test binary path");
        std::env::set_var("HYDRA_GUI_BIN", &me);
        let mut host = quick_host(&ipc);
        host.launch_budget = Duration::from_millis(400);
        assert_eq!(host.connect(true).err(), Some(Unreachable::Starting));
        assert_eq!(
            host.connect(false).err(),
            Some(Unreachable::NotRunning),
            "a ping never launches, so nothing is coming up"
        );
        assert_eq!(Unreachable::Starting.message(), "hydra is starting");
        assert_eq!(Unreachable::NotRunning.message(), "hydra is not running");
        let _ = std::fs::remove_dir_all(ipc.parent().expect("dir"));
    }

    /// The whole defect, end to end: ipc.json names a port that some other
    /// process now owns. The first frame must come back — with the port
    /// remembered as not-hydra — and the next capture must go past it to
    /// launching the app, instead of every frame dialling it again.
    ///
    /// Sets HYDRA_GUI_BIN, which only `launch_gui` reads.
    #[test]
    fn a_squatted_port_is_given_up_on_and_the_app_is_launched_instead() {
        let ipc = ipc_file("squatted");
        let squatter = TcpListener::bind(("127.0.0.1", 0)).expect("loopback listener");
        let squatted = squatter.local_addr().expect("address").port();
        let holder = std::thread::spawn(move || {
            let (held, _) = squatter.accept().expect("the host connects once");
            std::thread::sleep(Duration::from_millis(300));
            drop(held);
        });
        publish(&ipc, squatted);
        let mut host = quick_host(&ipc);
        let mut req = serde_json::json!({"type": "download", "url": "https://x.invalid/f"});

        // A ping never launches: it just learns the port is not hydra.
        let started = Instant::now();
        assert_eq!(
            host.dispatch(&mut req, false).err(),
            Some(Unreachable::NotRunning)
        );
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "bounded by the reply timeout"
        );
        assert_eq!(
            host.stale,
            Some(squatted),
            "the port is remembered as not hydra"
        );
        assert!(host.conn.is_none());

        // A capture goes straight past it to a launch, whose budget here is
        // nil, so it is reported as still starting rather than as absent.
        std::env::set_var(
            "HYDRA_GUI_BIN",
            std::env::current_exe().expect("test binary path"),
        );
        assert_eq!(
            host.dispatch(&mut req, true).err(),
            Some(Unreachable::Starting)
        );

        // The app comes up on a fresh port and rewrites the file: the next
        // frame reaches it, and the answer is the GUI's own.
        let gui = TcpListener::bind(("127.0.0.1", 0)).expect("loopback listener");
        publish(&ipc, gui.local_addr().expect("address").port());
        let answer = std::thread::spawn(move || {
            let (stream, _) = gui.accept().expect("the host connects");
            let mut out = stream.try_clone().expect("write half");
            let mut line = String::new();
            BufReader::new(stream)
                .read_line(&mut line)
                .expect("one request");
            writeln!(out, r#"{{"ok":true}}"#).expect("reply");
        });
        let reply = host
            .dispatch(&mut req, true)
            .expect("the real instance answers");
        assert_eq!(reply.trim(), r#"{"ok":true}"#);
        assert!(
            host.conn.is_some(),
            "the connection is kept for the next frame"
        );
        answer.join().expect("gui thread");
        holder.join().expect("squatter thread");
        let _ = std::fs::remove_dir_all(ipc.parent().expect("dir"));
    }

    /// With two Hydra bundles on disk, `open -a <name>` starts whichever
    /// LaunchServices registered last. The bundle this host ships in is the
    /// one whose profile the browser is waiting on, so it is named by path
    /// whenever there is one.
    #[test]
    fn a_macos_launch_names_its_own_bundle_when_it_has_one() {
        let exe = Path::new("/Applications/Hydra Download Manager.app/Contents/MacOS/hydra-host");
        let bundle = app_bundle_of(exe).expect("inside a bundle");
        assert_eq!(
            bundle,
            Path::new("/Applications/Hydra Download Manager.app")
        );
        assert_eq!(app_bundle_of(Path::new("/usr/local/bin/hydra-host")), None);
        assert_eq!(
            app_bundle_of(Path::new("/opt/hydra/target/release/hydra-host")),
            None
        );

        let strs = |args: Vec<OsString>| {
            args.into_iter()
                .map(|a| a.to_string_lossy().into_owned())
                .collect::<Vec<_>>()
        };
        assert_eq!(
            strs(open_args(Some(&bundle), None)),
            [
                "-g",
                "/Applications/Hydra Download Manager.app",
                "--args",
                "--minimized"
            ]
        );
        assert_eq!(
            strs(open_args(None, Some(Path::new("/Volumes/USB/hydra/data")))),
            [
                "-ga",
                MACOS_APP_NAME,
                "--args",
                "--minimized",
                "--config",
                "/Volumes/USB/hydra/data"
            ]
        );
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
            Some(Ipc {
                port: 50726,
                token: "cafe".to_string(),
                ws_port: Some(6799),
            })
        );
        // The WebSocket port is the GUI's to have or not; the line port is
        // what makes the file an endpoint.
        assert_eq!(
            parse_ipc(r#"{"port":50726,"token":"cafe","ws_port":null}"#).map(|i| i.ws_port),
            Some(None)
        );
        assert_eq!(
            parse_ipc(r#"{"port":50726,"token":"cafe"}"#).map(|i| i.ws_port),
            Some(None)
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

        let Round::Reply(reply) = round_trip(&mut conn, &mut req) else {
            panic!("a reply line");
        };
        assert_eq!(reply.trim(), r#"{"ok":true,"capture":true}"#);

        let on_the_wire: serde_json::Value =
            serde_json::from_str(&gui.join().expect("gui thread")).expect("json request");
        assert_eq!(on_the_wire["token"], "s3cret");
        assert_eq!(on_the_wire["url"], "https://example.invalid/f.zip");
    }

    /// The extension's way onto the WebSocket: the token only this process
    /// can read, handed out once the GUI has answered a ping with it. The
    /// same frame against a dead port, a file with no WebSocket, or a GUI
    /// that rejects the token yields an error and never launches anything.
    #[test]
    fn a_ws_token_is_handed_out_only_by_a_live_app() {
        let ipc = ipc_file("ws-token");
        let mut host = quick_host(&ipc);
        let parse =
            |bytes: Vec<u8>| serde_json::from_slice::<serde_json::Value>(&bytes).expect("json");
        let mut req = serde_json::json!({"type": "ws-token", "browser": "Google Chrome"});

        let (port, gui) = fake_gui(r#"{"ok":true,"version":"0.6.1"}"#, 1);
        std::fs::write(
            &ipc,
            format!(r#"{{"port":{port},"token":"s3cret","ws_port":6799,"pid":1}}"#),
        )
        .expect("write ipc.json");
        let reply = parse(handle(&mut host, &mut req));
        assert_eq!(reply["ok"], true);
        assert_eq!(reply["ws_port"], 6799);
        assert_eq!(reply["token"], "s3cret");
        let asked: serde_json::Value =
            serde_json::from_str(&gui.join().expect("gui thread")[0]).expect("json request");
        assert_eq!(asked["type"], "ping", "liveness is proven with a ping");
        assert_eq!(asked["token"], "s3cret");

        // The GUI has gone: the file still names its port, but nobody answers.
        host.conn = None;
        let reply = parse(handle(&mut host, &mut req));
        assert_eq!(reply["ok"], false);
        assert_eq!(reply["error"], "hydra is not running");

        // Up, but without a WebSocket: an honest error, not a bogus port.
        let (port, gui) = fake_gui(r#"{"ok":true}"#, 1);
        std::fs::write(
            &ipc,
            format!(r#"{{"port":{port},"token":"s3cret","ws_port":null,"pid":1}}"#),
        )
        .expect("write ipc.json");
        host.conn = None;
        let reply = parse(handle(&mut host, &mut req));
        assert_eq!(reply["ok"], false);
        assert_eq!(reply["error"], "hydra has no websocket port");
        gui.join().expect("gui thread");

        // A port that answers but refuses the token is not the instance the
        // file describes.
        let (port, gui) = fake_gui(r#"{"ok":false,"error":"bad token"}"#, 1);
        std::fs::write(
            &ipc,
            format!(r#"{{"port":{port},"token":"s3cret","ws_port":6799,"pid":1}}"#),
        )
        .expect("write ipc.json");
        host.conn = None;
        let reply = parse(handle(&mut host, &mut req));
        assert_eq!(reply["ok"], false);
        assert_eq!(reply["error"], "hydra is not running");
        gui.join().expect("gui thread");

        let _ = std::fs::remove_dir_all(ipc.parent().expect("dir"));
    }

    /// The routing `main` does: a ping never launches, a capture does, and
    /// `ws-token` is answered here instead of being forwarded.
    #[test]
    fn a_frame_is_routed_by_its_type() {
        let ipc = ipc_file("route");
        let mut host = quick_host(&ipc);
        let (port, gui) = fake_gui(r#"{"ok":true,"capture":true}"#, 2);
        publish(&ipc, port);

        let mut ping = serde_json::json!({"type": "ping"});
        let reply: serde_json::Value =
            serde_json::from_slice(&handle(&mut host, &mut ping)).expect("json");
        assert_eq!(reply["capture"], true, "forwarded verbatim");
        let mut dl = serde_json::json!({"type": "download", "url": "https://x.invalid/f"});
        let reply: serde_json::Value =
            serde_json::from_slice(&handle(&mut host, &mut dl)).expect("json");
        assert_eq!(reply["ok"], true);
        let seen = gui.join().expect("gui thread");
        assert_eq!(seen.len(), 2);
        assert!(seen[1].contains("\"url\":\"https://x.invalid/f\""));
        assert!(
            !seen.iter().any(|l| l.contains("ws-token")),
            "ws-token is never forwarded"
        );
        let _ = std::fs::remove_dir_all(ipc.parent().expect("dir"));
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
