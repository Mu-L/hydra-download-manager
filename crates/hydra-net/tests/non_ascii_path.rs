// Copyright (C) 2026 Javad Rajabzadeh
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! A path written in kana must reach the object, not a `400`.
//!
//! An alist share exposes `/d/guest/Public8/あけあけ/packs/2025-10d.part1.rar`.
//! Pasted as the browser shows it, the path is raw UTF-8, and a request-target
//! is ASCII — so the bytes went on the wire as-is and the origin answered
//! `400 Bad Request` on an address that downloads in any browser. The same
//! address spelled with `%E3%81%82…` worked, which is the whole report.
//!
//! The origin here rejects exactly what a real one rejects: a target carrying
//! a byte outside the printable ASCII range. Both spellings of the address
//! must arrive as the same target and fetch the same bytes.

use hya_core::{Scheduler, Source};
use hya_net::origin::byte_at;
use hya_net::{probe_resilient, run_transfer, Target, TcpConnector};
use std::sync::{Arc, Mutex};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

const SIZE: u64 = 256 * 1024;
const RAW: &str = "/d/guest/Public8/あけあけ/packs/2025-10d.part1.rar";
const ENCODED: &str =
    "/d/guest/Public8/%E3%81%82%E3%81%91%E3%81%82%E3%81%91/packs/2025-10d.part1.rar";

async fn read_head(s: &mut tokio::net::TcpStream) -> Option<(String, String, Option<(u64, u64)>)> {
    let mut head = Vec::new();
    let mut buf = [0u8; 1024];
    while !head.windows(4).any(|w| w == b"\r\n\r\n") {
        let n = s.read(&mut buf).await.ok()?;
        if n == 0 {
            return None;
        }
        head.extend_from_slice(&buf[..n]);
        if head.len() > 16 * 1024 {
            return None;
        }
    }
    let text = String::from_utf8_lossy(&head).to_string();
    let mut parts = text.split_whitespace();
    let method = parts.next()?.to_string();
    let target = parts.next().unwrap_or("/").to_string();
    let range = text
        .lines()
        .find(|l| l.to_ascii_lowercase().starts_with("range:"))
        .and_then(|l| l.split_once('=').map(|(_, v)| v.trim().to_string()))
        .and_then(|spec| {
            let (lo, hi) = spec.split_once('-')?;
            Some((lo.parse().ok()?, hi.parse().ok()?))
        });
    Some((method, target, range))
}

/// Serve the object at [`ENCODED`] only, and answer `400` to any target that is
/// not a legal request-target — which is what the reported server does.
async fn spawn() -> (u16, Arc<Mutex<Vec<String>>>) {
    let l = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let port = l.local_addr().expect("addr").port();
    let seen: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
    let log = seen.clone();
    tokio::spawn(async move {
        loop {
            let Ok((mut s, _)) = l.accept().await else {
                return;
            };
            let log = log.clone();
            tokio::spawn(async move {
                let Some((method, target, range)) = read_head(&mut s).await else {
                    return;
                };
                log.lock().expect("log").push(target.clone());
                if target.bytes().any(|b| !(0x21..=0x7E).contains(&b)) {
                    let _ = s
                        .write_all(
                            b"HTTP/1.1 400 Bad Request\r\nContent-Length: 0\r\n\
                              Connection: close\r\n\r\n",
                        )
                        .await;
                    return;
                }
                if target != ENCODED {
                    let _ = s
                        .write_all(
                            b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\n\
                              Connection: close\r\n\r\n",
                        )
                        .await;
                    return;
                }
                if method == "HEAD" {
                    let head = format!(
                        "HTTP/1.1 200 OK\r\nContent-Length: {SIZE}\r\nAccept-Ranges: bytes\r\n\
                         ETag: \"kana\"\r\nConnection: close\r\n\r\n"
                    );
                    let _ = s.write_all(head.as_bytes()).await;
                    return;
                }
                let (lo, hi) = range.unwrap_or((0, SIZE - 1));
                let hi = hi.min(SIZE - 1);
                let body: Vec<u8> = (lo..=hi).map(byte_at).collect();
                let head = format!(
                    "HTTP/1.1 206 Partial Content\r\nContent-Range: bytes {lo}-{hi}/{SIZE}\r\n\
                     Content-Length: {}\r\nETag: \"kana\"\r\nConnection: close\r\n\r\n",
                    body.len()
                );
                let _ = s.write_all(head.as_bytes()).await;
                let _ = s.write_all(&body).await;
            });
        }
    });
    (port, seen)
}

/// The reported case, end to end: the kana address downloads, and every
/// request-target the origin saw was the encoded spelling.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_kana_path_downloads_byte_exactly() {
    let (port, seen) = spawn().await;
    let t = Target::direct("127.0.0.1", port, RAW);

    let p = probe_resilient(&TcpConnector, &t).await.expect("probe");
    assert_eq!(p.status, 200, "the kana path must not be refused");
    assert_eq!(p.stated_length(), Some(SIZE));

    let out = std::env::temp_dir().join("hydra_kana_path.bin");
    let outs = out.to_string_lossy().to_string();
    let source = Source {
        gamma_est: 2e6,
        delta_est: 0.01,
        ..Default::default()
    };
    let sched = Scheduler::new(SIZE, vec![source], &[2]);
    run_transfer(Arc::new(TcpConnector), vec![t], &[2], SIZE, &outs, sched)
        .await
        .expect("transfer");

    let got = std::fs::read(&out).expect("output");
    assert_eq!(got.len() as u64, SIZE);
    assert!(
        got.iter().enumerate().all(|(i, b)| *b == byte_at(i as u64)),
        "the assembled file must match the origin byte for byte"
    );
    let _ = std::fs::remove_file(&out);

    let log = seen.lock().expect("log");
    assert!(!log.is_empty());
    assert!(
        log.iter().all(|t| t == ENCODED),
        "every request must carry the encoded target, got {log:?}"
    );
}

/// The address copied out of the browser's address bar is already encoded.
/// Encoding it again would ask for `%25E3…` and 404.
#[tokio::test]
async fn an_encoded_path_is_not_encoded_twice() {
    let (port, seen) = spawn().await;
    let t = Target::direct("127.0.0.1", port, ENCODED);

    let p = probe_resilient(&TcpConnector, &t).await.expect("probe");
    assert_eq!(p.status, 200, "the encoded spelling must reach the object");
    assert_eq!(p.stated_length(), Some(SIZE));
    assert!(seen.lock().expect("log").iter().all(|t| t == ENCODED));
}
