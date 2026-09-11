// Copyright (C) 2026 Javad Rajabzadeh
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! An origin behind AWS WAF challenges INTERMITTENTLY.
//!
//! `data.dtu.dk` waves a client through on one request and answers `202` with
//! `x-amzn-waf-action: challenge` on the next — the browser's own script
//! re-solves it so quietly that the challenge looks constant from outside. A
//! probe that failed on the first one reported "this link opens only in a
//! browser" about a link that had just delivered 122 MB, and made Resume
//! unusable against that origin.
//!
//! Both directions matter: a passing challenge must be ridden out, and a wall
//! must still be named clearly rather than retried forever.

use hya_net::{probe_resilient, Target, TcpConnector};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

const SIZE: u64 = 4096;

/// Challenge the first `challenges` requests, then answer normally.
/// Returns the port and the request counter.
async fn spawn(challenges: u64) -> (u16, Arc<AtomicU64>) {
    let l = TcpListener::bind(("127.0.0.1", 0)).await.expect("bind");
    let port = l.local_addr().unwrap().port();
    let seen = Arc::new(AtomicU64::new(0));
    let count = seen.clone();
    tokio::spawn(async move {
        loop {
            let Ok((mut s, _)) = l.accept().await else {
                continue;
            };
            let count = count.clone();
            tokio::spawn(async move {
                let mut buf = vec![0u8; 2048];
                if s.read(&mut buf).await.unwrap_or(0) == 0 {
                    return;
                }
                let n = count.fetch_add(1, Ordering::SeqCst);
                let reply = if n < challenges {
                    "HTTP/1.1 202 Accepted\r\nContent-Length: 0\r\n\
x-amzn-waf-action: challenge\r\nConnection: close\r\n\r\n"
                        .to_string()
                } else {
                    format!(
                        "HTTP/1.1 200 OK\r\nContent-Length: {SIZE}\r\n\
Accept-Ranges: bytes\r\nConnection: close\r\n\r\n"
                    )
                };
                let _ = s.write_all(reply.as_bytes()).await;
            });
        }
    });
    (port, seen)
}

/// The reported case: the challenge clears on the next ask, so the download
/// must go ahead rather than being told it needs a browser.
#[tokio::test]
async fn a_passing_bot_challenge_is_ridden_out() {
    let (port, seen) = spawn(1).await;
    let t = Target::direct("127.0.0.1", port, "/ndownloader/files/26003087");
    let p = probe_resilient(&TcpConnector, &t).await.expect("probe");

    assert_eq!(p.refusal(), None, "a passing challenge is not a refusal");
    assert_eq!(p.status, 200);
    assert_eq!(p.stated_length(), Some(SIZE));
    assert!(seen.load(Ordering::SeqCst) >= 2, "it never asked again");
}

/// Two in a row, because a WAF that is challenging at all rarely does it once.
#[tokio::test]
async fn a_repeated_challenge_is_still_ridden_out_within_the_budget() {
    let (port, _) = spawn(2).await;
    let t = Target::direct("127.0.0.1", port, "/f");
    let p = probe_resilient(&TcpConnector, &t).await.expect("probe");
    assert_eq!(p.refusal(), None, "{:?}", p.refusal());
    assert_eq!(p.stated_length(), Some(SIZE));
}

/// The other direction: an origin that will never let this client in is named
/// plainly, and the asking is bounded rather than a spin.
#[tokio::test]
async fn a_challenge_that_never_clears_is_reported_not_retried_forever() {
    let (port, seen) = spawn(u64::MAX).await;
    let t = Target::direct("127.0.0.1", port, "/f");
    let started = std::time::Instant::now();
    let p = probe_resilient(&TcpConnector, &t).await.expect("probe");

    let why = p.refusal().expect("a wall is a refusal");
    assert!(why.contains("202"), "{why}");
    assert!(why.contains("browser"), "{why}");
    // Bounded in both requests and time: this runs on the path to every
    // download, and a wall must not become a multi-second stall.
    let asked = seen.load(Ordering::SeqCst);
    assert!((2..=6).contains(&asked), "asked {asked} times");
    assert!(
        started.elapsed() < std::time::Duration::from_secs(5),
        "took {:?}",
        started.elapsed()
    );
}
