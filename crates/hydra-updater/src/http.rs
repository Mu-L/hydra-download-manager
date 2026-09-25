// Copyright (C) 2026 Javad Rajabzadeh
// SPDX-License-Identifier: GPL-3.0-or-later

//! Minimal HTTP GET on top of hya-net's connector: redirects, whole-body
//! fetches, and a streaming download with progress.
//!
//! hya-net's own fetch paths are shaped for the transfer engine (range
//! scheduling, probes); the updater needs the opposite shape — follow the
//! `github.com -> objects.githubusercontent.com` redirect chain, then either
//! hand back a small body whole or stream a large one to disk reporting
//! progress. Plain `http://` targets stay supported because that is what the
//! mock server in the tests speaks.
//!
//! Every wait is bounded: the connect, and each read while a body is
//! streaming. An update check that hangs on a half-open socket is a dialog
//! that never closes, and a cancel that is only noticed once bytes arrive is
//! no cancel at all on a stalled download.

use hya_net::{header_lookup, Connector, MaybeTls, Proxy, Target, TlsCapableConnector};
use std::io;
use std::time::{Duration, Instant};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

/// How long the socket may take to connect (and, through a proxy, to
/// tunnel), and how long a body read may go without a single byte.
#[derive(Clone, Copy, Debug)]
struct Timeouts {
    connect: Duration,
    idle: Duration,
}

const DEFAULT_TIMEOUTS: Timeouts = Timeouts {
    connect: Duration::from_secs(15),
    idle: Duration::from_secs(30),
};

/// How often a stalled read lets the progress callback say "stop".
const CANCEL_POLL: Duration = Duration::from_millis(250);

/// A parsed absolute URL, just enough for a GET.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Url {
    pub tls: bool,
    pub host: String,
    pub port: u16,
    pub path: String,
}

impl Url {
    pub fn parse(url: &str) -> io::Result<Url> {
        let (tls, rest) = if let Some(r) = url.strip_prefix("https://") {
            (true, r)
        } else if let Some(r) = url.strip_prefix("http://") {
            (false, r)
        } else {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("not an http(s) URL: {url}"),
            ));
        };
        let (authority, path) = match rest.find('/') {
            Some(i) => (&rest[..i], &rest[i..]),
            None => (rest, "/"),
        };
        let (host, port) = match authority.rsplit_once(':') {
            Some((h, p)) if p.chars().all(|c| c.is_ascii_digit()) && !p.is_empty() => (
                h,
                p.parse()
                    .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "bad port"))?,
            ),
            _ => (authority, if tls { 443 } else { 80 }),
        };
        if host.is_empty() {
            return Err(io::Error::new(io::ErrorKind::InvalidInput, "empty host"));
        }
        Ok(Url {
            tls,
            host: host.to_string(),
            port,
            path: path.to_string(),
        })
    }

    /// Whether the host is this machine: `localhost`, `127.x`, `::1`.
    pub fn is_loopback(&self) -> bool {
        let h = self.host.trim_matches(|c| c == '[' || c == ']');
        h.eq_ignore_ascii_case("localhost")
            || h == "::1"
            || h.parse::<std::net::IpAddr>()
                .is_ok_and(|ip| ip.is_loopback())
    }

    /// `host:port`, the authority a proxy is asked to reach.
    fn authority(&self) -> String {
        format!("{}:{}", self.host, self.port)
    }

    /// The `Host` header: the port only when it is not the scheme's default.
    fn host_header(&self) -> String {
        let default_port = (self.tls && self.port == 443) || (!self.tls && self.port == 80);
        if default_port {
            self.host.clone()
        } else {
            self.authority()
        }
    }

    /// Resolve a `Location` header against this URL (absolute, or
    /// origin-relative starting with `/`).
    fn join(&self, location: &str) -> io::Result<Url> {
        if location.starts_with("http://") || location.starts_with("https://") {
            Url::parse(location)
        } else if location.starts_with('/') {
            Ok(Url {
                path: location.to_string(),
                ..self.clone()
            })
        } else {
            Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("unresolvable redirect: {location}"),
            ))
        }
    }
}

/// The proxy the environment names for `url`, if any.
///
/// `HTTPS_PROXY`/`https_proxy` for TLS targets and `HTTP_PROXY`/`http_proxy`
/// for plain ones, the way curl reads them. Loopback targets are never
/// proxied: the mock server the tests run against lives there, and no proxy
/// can reach it anyway. A value that does not parse is ignored rather than
/// fatal — the updater has no logger, and a broken proxy setting should not
/// turn an update check into an error nobody can trace.
pub fn proxy_from_env(url: &str) -> Option<Proxy> {
    let u = Url::parse(url).ok()?;
    proxy_for(&u, |name| std::env::var(name).ok())
}

fn proxy_for(url: &Url, env: impl Fn(&str) -> Option<String>) -> Option<Proxy> {
    if url.is_loopback() {
        return None;
    }
    let names: &[&str] = if url.tls {
        &["HTTPS_PROXY", "https_proxy"]
    } else {
        &["HTTP_PROXY", "http_proxy"]
    };
    names
        .iter()
        .find_map(|n| env(n).filter(|v| !v.trim().is_empty()))
        .and_then(|raw| Proxy::parse(&raw).ok())
}

/// An open response: status line parsed, body not yet read.
struct Response {
    status: u16,
    head: String,
    stream: MaybeTls,
    /// Body bytes that arrived in the same read as the header terminator.
    prefix: Vec<u8>,
}

/// Headers GitHub's REST API asks every client to send: the media type
/// pins the JSON shape and the version pins the schema, so a future
/// default cannot silently change what `Release` deserialises.
const GITHUB_API_HEADERS: &str =
    "Accept: application/vnd.github+json\r\nX-GitHub-Api-Version: 2022-11-28\r\n";

async fn open(
    url: &Url,
    user_agent: &str,
    api: bool,
    proxy: Option<&Proxy>,
    t: &Timeouts,
) -> io::Result<Response> {
    let mut conn = TlsCapableConnector::new()?;
    // Through an HTTP proxy a plain request goes in absolute form (the
    // proxy reads the request line to know where to forward it); a TLS one
    // goes through a CONNECT tunnel the connector opens, after which the
    // origin-form line is what the origin sees. SOCKS sits below HTTP and
    // changes nothing in the request.
    let mut absolute_form = false;
    let mut target = match proxy {
        Some(p) if p.kind.is_socks() => {
            conn = conn.with_socks(p.clone());
            Target::direct(&url.host, url.port, &url.path)
        }
        Some(p) => {
            absolute_form = !url.tls;
            Target::via_proxy(&p.host, p.port, &url.authority(), &url.path)
        }
        None => Target::direct(&url.host, url.port, &url.path),
    };
    target.tls = url.tls;
    let mut stream = tokio::time::timeout(t.connect, conn.connect(&target))
        .await
        .map_err(|_| {
            io::Error::new(
                io::ErrorKind::TimedOut,
                format!("connecting to {} timed out", url.host),
            )
        })??;
    let request_target = if absolute_form {
        format!("http://{}{}", url.authority(), url.path)
    } else {
        url.path.clone()
    };
    let req = format!(
        "GET {} HTTP/1.1\r\nHost: {}\r\nUser-Agent: {}\r\n{}Accept: */*\r\nConnection: close\r\n\r\n",
        request_target,
        url.host_header(),
        user_agent,
        if api { GITHUB_API_HEADERS } else { "" },
    );
    stream.write_all(req.as_bytes()).await?;

    let mut buf = Vec::with_capacity(4096);
    let mut chunk = vec![0u8; 8192];
    let split = loop {
        let n = read_patiently(&mut stream, &mut chunk, t.idle, || true).await?;
        if n == 0 {
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "connection closed before response headers",
            ));
        }
        buf.extend_from_slice(&chunk[..n]);
        if let Some(i) = find_crlf2(&buf) {
            break i;
        }
        if buf.len() > 64 * 1024 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "response headers exceed 64 KB",
            ));
        }
    };
    let head = String::from_utf8_lossy(&buf[..split]).to_string();
    let status: u16 = head
        .split_whitespace()
        .nth(1)
        .and_then(|c| c.parse().ok())
        .unwrap_or(0);
    let prefix = buf[split..].to_vec();
    Ok(Response {
        status,
        head,
        stream,
        prefix,
    })
}

/// One read that gives `keep_going` a say every [`CANCEL_POLL`] while
/// nothing arrives, and gives up after `idle` without a byte.
///
/// Dropping a pending read on `MaybeTls` loses nothing: bytes that reached
/// the socket stay in it (or, under TLS, in the session's buffer) until the
/// next read.
async fn read_patiently(
    stream: &mut MaybeTls,
    buf: &mut [u8],
    idle: Duration,
    mut keep_going: impl FnMut() -> bool,
) -> io::Result<usize> {
    let started = Instant::now();
    loop {
        match tokio::time::timeout(CANCEL_POLL, stream.read(buf)).await {
            Ok(r) => return r,
            Err(_) => {
                if !keep_going() {
                    return Err(io::Error::new(
                        io::ErrorKind::Interrupted,
                        "download cancelled",
                    ));
                }
                if started.elapsed() >= idle {
                    return Err(io::Error::new(
                        io::ErrorKind::TimedOut,
                        format!("no data for {} s", idle.as_secs()),
                    ));
                }
            }
        }
    }
}

/// GET `url`, following up to 8 redirects, and require a 2xx.
async fn get(
    url: &str,
    user_agent: &str,
    api: bool,
    proxy: Option<&Proxy>,
    t: &Timeouts,
) -> io::Result<Response> {
    let mut u = Url::parse(url)?;
    for _ in 0..8 {
        let resp = open(&u, user_agent, api, proxy, t).await?;
        if matches!(resp.status, 301 | 302 | 303 | 307 | 308) {
            let loc = header_lookup(&resp.head, "location").ok_or_else(|| {
                io::Error::new(io::ErrorKind::InvalidData, "redirect without Location")
            })?;
            u = u.join(loc.trim())?;
            continue;
        }
        if (200..300).contains(&resp.status) {
            return Ok(resp);
        }
        return Err(io::Error::other(describe_failure(
            resp.status,
            &resp.head,
            &u.path,
            api,
        )));
    }
    Err(io::Error::other("too many redirects"))
}

/// A non-2xx status as the user should read it. GitHub's two routine
/// refusals get their own words: an anonymous client is allowed sixty API
/// calls an hour, and a repository with no release yet 404s on `latest`.
fn describe_failure(status: u16, head: &str, path: &str, api: bool) -> String {
    let remaining = header_lookup(head, "x-ratelimit-remaining").map(|v| v.trim() == "0");
    match status {
        403 if api && remaining == Some(true) => {
            match header_lookup(head, "x-ratelimit-reset").and_then(|v| v.trim().parse().ok()) {
                Some(reset) => format!(
                    "GitHub rate limit reached, try again after {}",
                    hhmm_utc(reset)
                ),
                None => "GitHub rate limit reached, try again later".to_string(),
            }
        }
        404 if api => "no release published yet".to_string(),
        _ => format!("server returned {status} for {path}"),
    }
}

/// `HH:MM UTC` for a Unix timestamp.
fn hhmm_utc(epoch: u64) -> String {
    let minute_of_day = (epoch / 60) % (24 * 60);
    format!("{:02}:{:02} UTC", minute_of_day / 60, minute_of_day % 60)
}

/// GET a small object whole. The cap is a refusal, not a truncation.
pub async fn get_bytes(url: &str, user_agent: &str, cap: usize) -> io::Result<Vec<u8>> {
    let proxy = proxy_from_env(url);
    fetch_bytes(
        url,
        user_agent,
        cap,
        false,
        proxy.as_ref(),
        &DEFAULT_TIMEOUTS,
    )
    .await
}

/// GET a GitHub REST resource whole, with the API's own headers and its
/// refusals spelled out ([`describe_failure`]).
pub async fn get_json(url: &str, user_agent: &str, cap: usize) -> io::Result<Vec<u8>> {
    let proxy = proxy_from_env(url);
    fetch_bytes(
        url,
        user_agent,
        cap,
        true,
        proxy.as_ref(),
        &DEFAULT_TIMEOUTS,
    )
    .await
}

async fn fetch_bytes(
    url: &str,
    user_agent: &str,
    cap: usize,
    api: bool,
    proxy: Option<&Proxy>,
    t: &Timeouts,
) -> io::Result<Vec<u8>> {
    let mut resp = get(url, user_agent, api, proxy, t).await?;
    let mut body = resp.prefix;
    let mut chunk = vec![0u8; 16 * 1024];
    loop {
        let n = match read_patiently(&mut resp.stream, &mut chunk, t.idle, || true).await {
            Ok(0) => break,
            Ok(n) => n,
            // `Connection: close` endings are routinely unclean under TLS.
            Err(e) if e.kind() == io::ErrorKind::UnexpectedEof => break,
            Err(e) => return Err(e),
        };
        body.extend_from_slice(&chunk[..n]);
        if body.len() > cap {
            return Err(io::Error::other("response exceeds the fetch cap"));
        }
    }
    if is_chunked(&resp.head) {
        return Ok(dechunk(&body));
    }
    Ok(body)
}

fn is_chunked(head: &str) -> bool {
    header_lookup(head, "transfer-encoding")
        .map(|v| v.to_ascii_lowercase().contains("chunked"))
        .unwrap_or(false)
}

/// GET a large object to `dest`, reporting `(bytes_so_far, total)` after
/// every write. The callback returning `false` cancels the download; the
/// partial file is removed and `ErrorKind::Interrupted` comes back. It is
/// also consulted every 250 ms while the connection is stalled, so a cancel
/// takes effect whether or not bytes are arriving.
pub async fn download_to_file(
    url: &str,
    user_agent: &str,
    dest: &std::path::Path,
    progress: impl FnMut(u64, Option<u64>) -> bool + Send,
) -> io::Result<()> {
    let proxy = proxy_from_env(url);
    download_with(
        url,
        user_agent,
        dest,
        proxy.as_ref(),
        &DEFAULT_TIMEOUTS,
        progress,
    )
    .await
}

async fn download_with(
    url: &str,
    user_agent: &str,
    dest: &std::path::Path,
    proxy: Option<&Proxy>,
    t: &Timeouts,
    mut progress: impl FnMut(u64, Option<u64>) -> bool + Send,
) -> io::Result<()> {
    let mut resp = get(url, user_agent, false, proxy, t).await?;
    // A proxy may re-frame a `Content-Length` body as chunks; the payload
    // is de-framed on the way to disk and the total is then unknown.
    let mut chunked = is_chunked(&resp.head).then(ChunkDecoder::default);
    let total: Option<u64> = if chunked.is_some() {
        None
    } else {
        header_lookup(&resp.head, "content-length").and_then(|v| v.trim().parse().ok())
    };

    if let Some(dir) = dest.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let mut file = tokio::fs::File::create(dest).await?;
    let mut got: u64 = 0;
    let mut payload = Vec::new();
    let prefix = std::mem::take(&mut resp.prefix);
    if !prefix.is_empty() {
        deliver(&mut chunked, &prefix, &mut payload)?;
        file.write_all(&payload).await?;
        got += payload.len() as u64;
        if !progress(got, total) {
            return cancel(dest).await;
        }
    }
    let mut chunk = vec![0u8; 64 * 1024];
    let finished = |chunked: &Option<ChunkDecoder>| chunked.as_ref().is_some_and(|d| d.is_done());
    while !finished(&chunked) && total.is_none_or(|want| got < want) {
        let n = match read_patiently(&mut resp.stream, &mut chunk, t.idle, || {
            progress(got, total)
        })
        .await
        {
            Ok(0) => break,
            Ok(n) => n,
            Err(e) if e.kind() == io::ErrorKind::UnexpectedEof => break,
            Err(e) if e.kind() == io::ErrorKind::Interrupted => return cancel(dest).await,
            Err(e) => {
                let _ = std::fs::remove_file(dest);
                return Err(e);
            }
        };
        deliver(&mut chunked, &chunk[..n], &mut payload)?;
        file.write_all(&payload).await?;
        got += payload.len() as u64;
        if !progress(got, total) {
            return cancel(dest).await;
        }
    }
    file.flush().await?;
    drop(file);
    let truncated = match (total, &chunked) {
        (Some(want), _) if got != want => {
            Some(format!("download truncated: {got} of {want} bytes"))
        }
        (_, Some(dec)) if !dec.is_done() => Some(format!(
            "download truncated: chunked body ended after {got} bytes"
        )),
        _ => None,
    };
    if let Some(msg) = truncated {
        let _ = std::fs::remove_file(dest);
        return Err(io::Error::new(io::ErrorKind::UnexpectedEof, msg));
    }
    Ok(())
}

/// The payload in `raw`: the bytes themselves, or what is left of them once
/// the chunk framing is taken off.
fn deliver(
    chunked: &mut Option<ChunkDecoder>,
    raw: &[u8],
    payload: &mut Vec<u8>,
) -> io::Result<()> {
    payload.clear();
    match chunked {
        Some(dec) => dec.feed(raw, payload),
        None => {
            payload.extend_from_slice(raw);
            Ok(())
        }
    }
}

async fn cancel(dest: &std::path::Path) -> io::Result<()> {
    let _ = tokio::fs::remove_file(dest).await;
    Err(io::Error::new(
        io::ErrorKind::Interrupted,
        "download cancelled",
    ))
}

fn find_crlf2(buf: &[u8]) -> Option<usize> {
    buf.windows(4).position(|w| w == b"\r\n\r\n").map(|i| i + 4)
}

/// Incremental de-framing of a chunked body, fed whatever the socket
/// delivers — a chunk-size line split across two reads included.
#[derive(Debug, Default)]
struct ChunkDecoder {
    state: ChunkState,
    remaining: usize,
    line: Vec<u8>,
}

#[derive(Debug, Default, PartialEq, Eq)]
enum ChunkState {
    /// Reading the `<hex>[;ext]\r\n` size line.
    #[default]
    Size,
    /// Inside a chunk's payload.
    Data,
    /// Consuming the `\r\n` that closes a chunk.
    DataEnd,
    /// The zero-size chunk was seen; trailers, if any, are ignored.
    Done,
}

impl ChunkDecoder {
    fn feed(&mut self, mut input: &[u8], out: &mut Vec<u8>) -> io::Result<()> {
        while !input.is_empty() {
            match self.state {
                ChunkState::Done => return Ok(()),
                ChunkState::Size | ChunkState::DataEnd => {
                    let Some(nl) = input.iter().position(|&b| b == b'\n') else {
                        self.line.extend_from_slice(input);
                        return Ok(());
                    };
                    self.line.extend_from_slice(&input[..nl]);
                    input = &input[nl + 1..];
                    let line = std::mem::take(&mut self.line);
                    if self.state == ChunkState::DataEnd {
                        self.state = ChunkState::Size;
                        continue;
                    }
                    let text = String::from_utf8_lossy(&line);
                    let size_field = text.split(';').next().unwrap_or("").trim();
                    let n = usize::from_str_radix(size_field, 16).map_err(|_| {
                        io::Error::new(
                            io::ErrorKind::InvalidData,
                            format!("bad chunk size: {size_field:?}"),
                        )
                    })?;
                    if n == 0 {
                        self.state = ChunkState::Done;
                        return Ok(());
                    }
                    self.remaining = n;
                    self.state = ChunkState::Data;
                }
                ChunkState::Data => {
                    let take = self.remaining.min(input.len());
                    out.extend_from_slice(&input[..take]);
                    input = &input[take..];
                    self.remaining -= take;
                    if self.remaining == 0 {
                        self.state = ChunkState::DataEnd;
                    }
                }
            }
        }
        Ok(())
    }

    fn is_done(&self) -> bool {
        self.state == ChunkState::Done
    }
}

/// De-frame a complete chunked body already held in memory. A malformed
/// frame keeps what was decoded before it.
fn dechunk(body: &[u8]) -> Vec<u8> {
    let mut out = Vec::new();
    let _ = ChunkDecoder::default().feed(body, &mut out);
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::net::TcpListener;

    #[test]
    fn url_parsing() {
        let u = Url::parse("https://api.github.com/repos/ja7ad/hydra/releases/latest").unwrap();
        assert_eq!(
            u,
            Url {
                tls: true,
                host: "api.github.com".into(),
                port: 443,
                path: "/repos/ja7ad/hydra/releases/latest".into()
            }
        );
        let u = Url::parse("http://127.0.0.1:8642/latest").unwrap();
        assert_eq!(u.port, 8642);
        assert!(!u.tls);
        let u = Url::parse("https://example.com").unwrap();
        assert_eq!(u.path, "/");
        assert!(Url::parse("ftp://example.com/x").is_err());
        assert!(Url::parse("https:///nohost").is_err());
    }

    #[test]
    fn redirect_join() {
        let base =
            Url::parse("https://github.com/ja7ad/hydra/releases/download/v1/x.tar.gz").unwrap();
        let abs = base
            .join("https://objects.githubusercontent.com/blob/1")
            .unwrap();
        assert_eq!(abs.host, "objects.githubusercontent.com");
        let rel = base.join("/other/path").unwrap();
        assert_eq!(rel.host, "github.com");
        assert_eq!(rel.path, "/other/path");
        assert!(base.join("no-scheme-relative").is_err());
    }

    #[test]
    fn dechunk_reassembles() {
        let body = b"4\r\nWiki\r\n5\r\npedia\r\n0\r\n\r\n";
        assert_eq!(dechunk(body), b"Wikipedia");
    }

    #[test]
    fn chunks_survive_arbitrary_read_boundaries() {
        // A proxy re-chunks on its own schedule and the socket hands the
        // frames over on another; a size line cut in two, or a chunk whose
        // payload spans three reads, must decode the same as one whole read.
        let body = b"4;ext=1\r\nWiki\r\nA\r\npedia rock\r\n0\r\nX-Trailer: 1\r\n\r\n";
        for step in 1..=body.len() {
            let mut dec = ChunkDecoder::default();
            let mut out = Vec::new();
            for piece in body.chunks(step) {
                dec.feed(piece, &mut out).unwrap();
            }
            assert_eq!(out, b"Wikipedia rock", "step {step}");
            assert!(dec.is_done(), "step {step}");
        }
        let mut dec = ChunkDecoder::default();
        assert!(dec.feed(b"zz\r\n", &mut Vec::new()).is_err());
    }

    #[test]
    fn github_refusals_are_spelled_out() {
        let limited = "HTTP/1.1 403 Forbidden\r\nx-ratelimit-remaining: 0\r\n\
                       x-ratelimit-reset: 1700000000\r\n\r\n";
        assert_eq!(
            describe_failure(403, limited, "/repos/x/releases/latest", true),
            "GitHub rate limit reached, try again after 22:13 UTC"
        );
        let no_reset = "HTTP/1.1 403 Forbidden\r\nx-ratelimit-remaining: 0\r\n\r\n";
        assert_eq!(
            describe_failure(403, no_reset, "/p", true),
            "GitHub rate limit reached, try again later"
        );
        // A 403 that is not the rate limit stays a plain status.
        let plain = "HTTP/1.1 403 Forbidden\r\nx-ratelimit-remaining: 41\r\n\r\n";
        assert_eq!(
            describe_failure(403, plain, "/p", true),
            "server returned 403 for /p"
        );
        assert_eq!(
            describe_failure(
                404,
                "HTTP/1.1 404\r\n\r\n",
                "/repos/x/releases/latest",
                true
            ),
            "no release published yet"
        );
        // An asset 404 is not "no release": the release is there, the file is not.
        assert_eq!(
            describe_failure(404, "HTTP/1.1 404\r\n\r\n", "/assets/SHA256SUMS.txt", false),
            "server returned 404 for /assets/SHA256SUMS.txt"
        );
    }

    #[test]
    fn a_proxy_comes_from_the_environment_for_its_scheme_only() {
        let env = |name: &str| match name {
            "HTTPS_PROXY" => Some("http://proxy.corp:3128".to_string()),
            "http_proxy" => Some("socks5://127.0.0.1:1080".to_string()),
            _ => None,
        };
        let https = proxy_for(&Url::parse("https://api.github.com/x").unwrap(), env).unwrap();
        assert_eq!((https.host.as_str(), https.port), ("proxy.corp", 3128));
        assert!(!https.kind.is_socks());
        let http = proxy_for(&Url::parse("http://mirror.example/x").unwrap(), env).unwrap();
        assert!(http.kind.is_socks());
        // The mock server lives on loopback, and no proxy could reach it.
        for local in [
            "http://127.0.0.1:8642/x",
            "http://localhost:8642/x",
            "http://[::1]:1/x",
        ] {
            assert!(
                proxy_for(&Url::parse(local).unwrap(), env).is_none(),
                "{local}"
            );
        }
        // Garbage in the variable is no proxy, not an error.
        assert!(
            proxy_for(&Url::parse("https://api.github.com/x").unwrap(), |_| Some(
                "ftp://nope".into()
            ))
            .is_none()
        );
        assert!(proxy_for(&Url::parse("https://api.github.com/x").unwrap(), |_| None).is_none());
    }

    /// Serve one canned response to every connection; hand back the request
    /// heads seen. `body` is sent as given after the status line and
    /// `headers`, so a test can send a chunked body or stall halfway.
    async fn serve(
        status: &'static str,
        headers: &'static str,
        body: Vec<u8>,
        stall_after: Option<usize>,
    ) -> (
        String,
        std::sync::Arc<std::sync::Mutex<Vec<String>>>,
        tokio::task::JoinHandle<()>,
    ) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://127.0.0.1:{}", listener.local_addr().unwrap().port());
        let seen = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let heads = seen.clone();
        let handle = tokio::spawn(async move {
            loop {
                let Ok((mut sock, _)) = listener.accept().await else {
                    return;
                };
                let heads = heads.clone();
                let body = body.clone();
                tokio::spawn(async move {
                    let mut buf = vec![0u8; 8192];
                    let mut req = Vec::new();
                    loop {
                        let n = sock.read(&mut buf).await.unwrap_or(0);
                        if n == 0 {
                            return;
                        }
                        req.extend_from_slice(&buf[..n]);
                        if req.windows(4).any(|w| w == b"\r\n\r\n") {
                            break;
                        }
                    }
                    heads
                        .lock()
                        .unwrap()
                        .push(String::from_utf8_lossy(&req).into_owned());
                    let head = format!("HTTP/1.1 {status}\r\n{headers}\r\n");
                    let _ = sock.write_all(head.as_bytes()).await;
                    match stall_after {
                        Some(n) => {
                            let _ = sock.write_all(&body[..n.min(body.len())]).await;
                            // Held open, saying nothing, until the client gives up.
                            tokio::time::sleep(Duration::from_secs(30)).await;
                        }
                        None => {
                            let _ = sock.write_all(&body).await;
                        }
                    }
                    let _ = sock.shutdown().await;
                });
            }
        });
        (base, seen, handle)
    }

    fn quick() -> Timeouts {
        Timeouts {
            connect: Duration::from_secs(5),
            idle: Duration::from_millis(700),
        }
    }

    fn temp_file(tag: &str) -> std::path::PathBuf {
        std::env::temp_dir().join(format!("hydra-http-{tag}-{}", std::process::id()))
    }

    #[tokio::test]
    async fn api_requests_carry_githubs_headers_and_assets_do_not() {
        let (base, seen, server) =
            serve("200 OK", "Content-Length: 2\r\n", b"{}".to_vec(), None).await;
        let t = quick();
        fetch_bytes(
            &format!("{base}/repos/x/releases/latest"),
            "ua",
            1024,
            true,
            None,
            &t,
        )
        .await
        .unwrap();
        fetch_bytes(&format!("{base}/assets/f"), "ua", 1024, false, None, &t)
            .await
            .unwrap();
        let heads = seen.lock().unwrap().clone();
        assert!(heads[0].contains("Accept: application/vnd.github+json\r\n"));
        assert!(heads[0].contains("X-GitHub-Api-Version: 2022-11-28\r\n"));
        assert!(!heads[1].contains("vnd.github"));
        assert!(heads[1].contains("Accept: */*\r\n"));
        server.abort();
    }

    #[tokio::test]
    async fn a_server_that_accepts_and_never_answers_times_out() {
        // The head is sent and then nothing: the body read has to end on
        // its own, in the idle limit, rather than pin the dialog forever.
        let (base, _seen, server) = serve(
            "200 OK",
            "Content-Length: 1000\r\n",
            vec![b'x'; 1000],
            Some(0),
        )
        .await;
        let started = Instant::now();
        let err = fetch_bytes(&format!("{base}/x"), "ua", 4096, false, None, &quick())
            .await
            .unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::TimedOut, "{err}");
        assert!(started.elapsed() < Duration::from_secs(5));
        server.abort();
    }

    #[tokio::test]
    async fn a_download_that_stalls_can_still_be_cancelled_and_times_out() {
        let body: Vec<u8> = (0..200_000u32).map(|i| i as u8).collect();
        let (base, _seen, server) =
            serve("200 OK", "Content-Length: 200000\r\n", body, Some(1024)).await;

        // Cancel from a stalled connection: the callback flips after one
        // poll, with no byte having arrived to prompt it.
        let dest = temp_file("stall-cancel");
        let mut polls = 0;
        let started = Instant::now();
        let slow = Timeouts {
            connect: Duration::from_secs(5),
            idle: Duration::from_secs(30),
        };
        let err = download_with(&format!("{base}/x"), "ua", &dest, None, &slow, |_, _| {
            polls += 1;
            polls < 3
        })
        .await
        .unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::Interrupted, "{err}");
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "cancel must not wait for the idle limit"
        );
        assert!(!dest.exists(), "the partial file is removed");

        // No cancel: the idle limit ends it, and the partial file goes too.
        let dest = temp_file("stall-timeout");
        let err = download_with(&format!("{base}/x"), "ua", &dest, None, &quick(), |_, _| {
            true
        })
        .await
        .unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::TimedOut, "{err}");
        assert!(!dest.exists());
        server.abort();
    }

    #[tokio::test]
    async fn a_chunked_download_is_deframed_to_disk() {
        let payload: Vec<u8> = (0..300_000u32).map(|i| (i % 251) as u8).collect();
        let mut wire = Vec::new();
        for piece in payload.chunks(70_000) {
            wire.extend_from_slice(format!("{:x}\r\n", piece.len()).as_bytes());
            wire.extend_from_slice(piece);
            wire.extend_from_slice(b"\r\n");
        }
        wire.extend_from_slice(b"0\r\n\r\n");
        let (base, _seen, server) =
            serve("200 OK", "Transfer-Encoding: chunked\r\n", wire, None).await;
        let dest = temp_file("chunked");
        let mut last = (0, Some(0));
        download_with(
            &format!("{base}/x"),
            "ua",
            &dest,
            None,
            &quick(),
            |got, total| {
                last = (got, total);
                true
            },
        )
        .await
        .unwrap();
        assert_eq!(std::fs::read(&dest).unwrap(), payload);
        assert_eq!(
            last,
            (payload.len() as u64, None),
            "no total for a chunked body"
        );
        let _ = std::fs::remove_file(&dest);

        // A chunked body cut off before its terminator is a truncation.
        let (base, _seen, server2) = serve(
            "200 OK",
            "Transfer-Encoding: chunked\r\n",
            b"5\r\nhello\r\n5\r\nwor".to_vec(),
            None,
        )
        .await;
        let dest = temp_file("chunked-short");
        let err = download_with(&format!("{base}/x"), "ua", &dest, None, &quick(), |_, _| {
            true
        })
        .await
        .unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::UnexpectedEof, "{err}");
        assert!(!dest.exists());
        server.abort();
        server2.abort();
    }

    #[tokio::test]
    async fn a_plain_request_through_an_http_proxy_is_absolute_form() {
        // The "proxy" is just a server that records what it was asked for:
        // a forward proxy needs the full URL on the request line, and the
        // Host header must still name the origin.
        let (proxy_base, seen, server) =
            serve("200 OK", "Content-Length: 2\r\n", b"ok".to_vec(), None).await;
        let proxy_url = Url::parse(&proxy_base).unwrap();
        let proxy = Proxy::parse(&format!("http://{}:{}", proxy_url.host, proxy_url.port)).unwrap();
        let body = fetch_bytes(
            "http://origin.example:8080/asset.tar.gz",
            "ua",
            16,
            false,
            Some(&proxy),
            &quick(),
        )
        .await
        .unwrap();
        assert_eq!(body, b"ok");
        let head = seen.lock().unwrap()[0].clone();
        assert!(
            head.starts_with("GET http://origin.example:8080/asset.tar.gz HTTP/1.1\r\n"),
            "{head}"
        );
        assert!(head.contains("Host: origin.example:8080\r\n"), "{head}");
        server.abort();
    }
}
