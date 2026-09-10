// Copyright (C) 2026 Javad Rajabzadeh
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! An origin that redirects every request to a URL signed for a few seconds.
//!
//! `data.dtu.dk/ndownloader/files/26003087` is the reported case. It answers
//! each request with a `302` to `s3q.ait.dtu.dk:9000/...?X-Amz-Expires=10` — a
//! credential good for TEN SECONDS. Collapsing that redirect once and reusing
//! the result is what broke a 22 GB download: the first connections streamed,
//! and every range requested after the tenth second came back `403`. The
//! reporter saw `unexpected status 403 for a range request` with 6 MB of 22 GB
//! on disk and eight dead connections.
//!
//! The fix is to treat the signed URL as a credential rather than an address:
//! the transfer keeps the address the user gave, and each range request
//! follows the redirect itself and is signed afresh.

use hya_core::{Scheduler, Source};
use hya_net::{Target, TcpConnector};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

/// Long enough that a first request always makes it, short enough that reusing
/// one credential for a whole transfer cannot.
const WINDOW: u64 = 1;
const SIZE: u64 = 4 * 1024 * 1024;

fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

fn byte_at(i: u64) -> u8 {
    (i % 251) as u8
}

/// Requests that arrived bearing a credential that had already run out.
static EXPIRED: AtomicU64 = AtomicU64::new(0);
/// Credentials minted, i.e. redirects served.
static SIGNED: AtomicU64 = AtomicU64::new(0);
/// One response is cut short, so the range it was carrying has to be asked for
/// AGAIN later — by which time the credential that fetched it is dead. This is
/// the "Disconnect." the reporter's connection list was full of, and it is what
/// makes a late request certain rather than a matter of scheduler timing.
static TRUNCATED_ONE: AtomicU64 = AtomicU64::new(0);
/// The origin also sits behind a bot challenge that fires intermittently — AWS
/// WAF answers `202` with no body and lets the very next request through. One
/// challenge used to end the transfer with `unexpected status 202 for a range
/// request`, throwing away a download that was otherwise complete.
static CHALLENGED: AtomicU64 = AtomicU64::new(0);

async fn spawn_origin() -> u16 {
    let l = TcpListener::bind(("127.0.0.1", 0)).await.expect("bind");
    let port = l.local_addr().unwrap().port();
    tokio::spawn(async move {
        loop {
            let Ok((mut s, _)) = l.accept().await else {
                continue;
            };
            tokio::spawn(async move {
                let mut buf = vec![0u8; 4096];
                let n = s.read(&mut buf).await.unwrap_or(0);
                if n == 0 {
                    return;
                }
                let req = String::from_utf8_lossy(&buf[..n]).into_owned();
                let line = req.lines().next().unwrap_or_default().to_string();
                let path = line.split_whitespace().nth(1).unwrap_or("/").to_string();

                // The unsigned address: hand out a fresh, short-lived credential.
                if !path.contains("?exp=") {
                    // A BURST of three, once the transfer is under way. One is
                    // absorbed by the ordinary failure tolerance and proves
                    // nothing; three in a row is what reaches the threshold
                    // that aborts a transfer, and a WAF that is challenging at
                    // all challenges more than once.
                    if SIGNED.load(Ordering::SeqCst) > 2
                        && CHALLENGED.fetch_add(1, Ordering::SeqCst) < 3
                    {
                        let r = "HTTP/1.1 202 Accepted\r\nContent-Length: 0\r\n\
x-amzn-waf-action: challenge\r\nConnection: close\r\n\r\n";
                        let _ = s.write_all(r.as_bytes()).await;
                        return;
                    }
                    SIGNED.fetch_add(1, Ordering::SeqCst);
                    // ABSOLUTE, with an explicit port — the shape a real object
                    // store uses. A relative Location exercises none of the
                    // authority parsing that made this fail in the field.
                    let loc = format!("http://127.0.0.1:{port}/object?exp={}", now() + WINDOW);
                    let r = format!(
                        "HTTP/1.1 302 Found\r\nLocation: {loc}\r\n\
Content-Length: 0\r\nConnection: close\r\n\r\n"
                    );
                    let _ = s.write_all(r.as_bytes()).await;
                    return;
                }

                let exp: u64 = path
                    .rsplit_once("?exp=")
                    .and_then(|(_, v)| v.parse().ok())
                    .unwrap_or(0);
                if now() > exp {
                    EXPIRED.fetch_add(1, Ordering::SeqCst);
                    let body = b"<Error><Code>AccessDenied</Code>\
<Message>Request has expired</Message></Error>";
                    let r = format!(
                        "HTTP/1.1 403 Forbidden\r\nContent-Length: {}\r\n\
Connection: close\r\n\r\n",
                        body.len()
                    );
                    let _ = s.write_all(r.as_bytes()).await;
                    let _ = s.write_all(body).await;
                    return;
                }

                let (lo, hi) = req
                    .lines()
                    .find_map(|l| {
                        let v = l.strip_prefix("Range: bytes=")?;
                        let (a, b) = v.trim().split_once('-')?;
                        Some((a.parse::<u64>().ok()?, b.parse::<u64>().ok()?))
                    })
                    .unwrap_or((0, SIZE - 1));
                let hi = hi.min(SIZE - 1);
                let body: Vec<u8> = (lo..=hi).map(byte_at).collect();
                let head = format!(
                    "HTTP/1.1 206 Partial Content\r\nContent-Length: {}\r\n\
Content-Range: bytes {lo}-{hi}/{SIZE}\r\nAccept-Ranges: bytes\r\n\
Connection: close\r\n\r\n",
                    body.len()
                );
                let _ = s.write_all(head.as_bytes()).await;
                let cut = TRUNCATED_ONE.fetch_add(1, Ordering::SeqCst) == 0;
                let mut sent = 0usize;
                // Paced, so the transfer outlives a credential's window several
                // times over. Without this the whole thing finishes inside one
                // window and proves nothing.
                for chunk in body.chunks(8 * 1024) {
                    if cut && sent >= body.len() / 2 {
                        return; // hang up mid-body: that range must be re-asked
                    }
                    if s.write_all(chunk).await.is_err() {
                        return;
                    }
                    sent += chunk.len();
                    tokio::time::sleep(std::time::Duration::from_millis(45)).await;
                }
            });
        }
    });
    port
}

/// The reported failure, end to end: eight connections against an origin whose
/// credentials outlive neither the transfer nor, usually, the next range.
#[tokio::test]
async fn a_transfer_outlives_the_signature_it_started_with() {
    let port = spawn_origin().await;
    let out = std::env::temp_dir().join(format!("hya-expiring-{}.bin", std::process::id()));
    let _ = std::fs::remove_file(&out);

    const N: usize = 8;
    // The DURABLE address — no credential in it. Handing the transfer the
    // resolved URL instead is precisely the bug.
    let target = Target::direct("127.0.0.1", port, "/download");
    let sched = Scheduler::new(SIZE, vec![Source::default()], &[N]);

    let (elapsed, reqs) = hya_net::run_transfer(
        Arc::new(TcpConnector),
        vec![target],
        &[N],
        SIZE,
        out.to_str().unwrap(),
        sched,
    )
    .await
    .expect("the transfer must survive its credentials expiring");

    let got = std::fs::read(&out).expect("the finished file");
    assert_eq!(got.len() as u64, SIZE, "short file after {reqs} requests");
    // Every byte, not just the length: a range spliced from the wrong offset
    // after a redirect would pass a length check and corrupt the file.
    let bad = got
        .iter()
        .enumerate()
        .find(|(i, b)| **b != byte_at(*i as u64));
    assert_eq!(bad.map(|(i, _)| i), None, "wrong byte");

    // The premise: the transfer really did outlive its first credential, and
    // really did have to mint more than one.
    assert!(
        elapsed > WINDOW as f64,
        "transfer took {elapsed:.1}s, inside one {WINDOW}s window — it proves nothing"
    );
    assert!(
        SIGNED.load(Ordering::SeqCst) > 1,
        "only one credential was ever minted"
    );
    // And nothing was ever fetched with a dead one.
    assert_eq!(
        EXPIRED.load(Ordering::SeqCst),
        0,
        "a request went out bearing an expired credential"
    );
    // And the bot challenge really did fire, and really was survived.
    assert!(
        CHALLENGED.load(Ordering::SeqCst) > 0,
        "the challenge never fired, so surviving one is untested"
    );
    let _ = std::fs::remove_file(&out);
}

/// Following a hop per request must not become a way to spin forever: an
/// origin that redirects endlessly costs a bounded number of requests and then
/// says so.
#[tokio::test]
async fn a_redirect_loop_is_bounded_rather_than_followed_forever() {
    let hops = Arc::new(AtomicU64::new(0));
    let l = TcpListener::bind(("127.0.0.1", 0)).await.expect("bind");
    let port = l.local_addr().unwrap().port();
    let seen = hops.clone();
    tokio::spawn(async move {
        loop {
            let Ok((mut s, _)) = l.accept().await else {
                continue;
            };
            let seen = seen.clone();
            tokio::spawn(async move {
                let mut buf = vec![0u8; 2048];
                if s.read(&mut buf).await.unwrap_or(0) == 0 {
                    return;
                }
                let n = seen.fetch_add(1, Ordering::SeqCst);
                let r = format!(
                    "HTTP/1.1 302 Found\r\nLocation: /hop{n}\r\n\
Content-Length: 0\r\nConnection: close\r\n\r\n"
                );
                let _ = s.write_all(r.as_bytes()).await;
            });
        }
    });

    let out = std::env::temp_dir().join(format!("hya-loop-{}.bin", std::process::id()));
    let _ = std::fs::remove_file(&out);
    let sched = Scheduler::new(SIZE, vec![Source::default()], &[1]);
    let err = hya_net::run_transfer(
        Arc::new(TcpConnector),
        vec![Target::direct("127.0.0.1", port, "/start")],
        &[1],
        SIZE,
        out.to_str().unwrap(),
        sched,
    )
    .await
    .expect_err("an endless redirect cannot succeed");
    assert!(
        err.to_string().contains("redirect budget exhausted"),
        "{err}"
    );
    let _ = std::fs::remove_file(&out);
}

/// A `Location` that names nothing usable is a broken origin, and worth saying
/// so rather than spending the hop budget discovering it again.
#[tokio::test]
async fn an_unusable_location_is_reported_not_retried() {
    let l = TcpListener::bind(("127.0.0.1", 0)).await.expect("bind");
    let port = l.local_addr().unwrap().port();
    tokio::spawn(async move {
        loop {
            let Ok((mut s, _)) = l.accept().await else {
                continue;
            };
            tokio::spawn(async move {
                let mut buf = vec![0u8; 2048];
                if s.read(&mut buf).await.unwrap_or(0) == 0 {
                    return;
                }
                let _ = s
                    .write_all(
                        b"HTTP/1.1 302 Found\r\nLocation: \r\n\
Content-Length: 0\r\nConnection: close\r\n\r\n",
                    )
                    .await;
            });
        }
    });

    let out = std::env::temp_dir().join(format!("hya-badloc-{}.bin", std::process::id()));
    let _ = std::fs::remove_file(&out);
    let sched = Scheduler::new(SIZE, vec![Source::default()], &[1]);
    let err = hya_net::run_transfer(
        Arc::new(TcpConnector),
        vec![Target::direct("127.0.0.1", port, "/start")],
        &[1],
        SIZE,
        out.to_str().unwrap(),
        sched,
    )
    .await
    .expect_err("a redirect to nowhere cannot succeed");
    assert!(
        err.to_string().contains("unusable redirect target"),
        "{err}"
    );
    let _ = std::fs::remove_file(&out);
}
