// Copyright (C) 2026 Javad Rajabzadeh
// SPDX-License-Identifier: GPL-3.0-or-later

//! `hydra --preview <url>`: what is inside a ZIP archive, without the archive.
//!
//! The same peek the GUI's Preview button makes — `hya_net::zipdir` reads
//! the index off the file's tail — drawn as a table for a terminal. One
//! probe to find the object and its size, one small ranged GET, and the
//! listing is on screen whatever the archive weighs.

use hya_net::zipdir::{self, DosTime, Entry, PeekError};

pub async fn run(url: &str, args: &crate::cli::Cli) -> Result<(), String> {
    let Some(u) = crate::url::Url::parse(url) else {
        return Err(format!("{url} is not an http, https or ftp URL"));
    };
    let conn = hya_net::TlsCapableConnector::with_insecure(args.insecure)
        .map_err(|e| format!("tls setup failed: {e}"))?;
    let (probe, url, jar) = crate::download::probe_public(&conn, &u, args).await?;
    if let Some(why) = probe.refusal() {
        return Err(format!("the {why} for {url}"));
    }
    let total = probe.size;
    if total == 0 {
        return Err(format!(
            "the server states no size for {url}, so its tail cannot be read"
        ));
    }
    let name = probe
        .suggested_filename()
        .unwrap_or_else(|| url.suggested_filename());

    let target = crate::download::target_for_public(&url, args)?
        .with_jar(&jar, hya_net::cookies::now_secs());
    let entries = match zipdir::fetch_listing(&conn, &target, total).await {
        Ok(e) => e,
        Err(PeekError::Zip(zipdir::Error::NotZip)) => {
            return Err(format!(
                "{name} is not a ZIP archive; --preview lists ZIP archives only"
            ));
        }
        Err(e) => return Err(format!("could not list {name}: {e}")),
    };
    print_table(&name, total, &entries);
    Ok(())
}

/// four columns, with the variable-width name last so the numbers
/// line up. Files only: a directory entry says nothing the paths of the
/// files inside it do not already say.
fn print_table(name: &str, total: u64, entries: &[Entry]) {
    let files: Vec<&Entry> = entries.iter().filter(|e| !e.is_dir()).collect();
    println!(
        "{name}, {} ({total} bytes), {} file{}",
        hya_core::fmt::bytes(total),
        files.len(),
        if files.len() == 1 { "" } else { "s" }
    );
    if files.is_empty() {
        return;
    }
    println!("{:>10}  {:>10}  {:<16}  Name", "Size", "Packed", "Modified");
    for e in files {
        // WinRAR's convention: an encrypted entry's name carries a trailing `*`.
        let mark = if e.encrypted { "*" } else { "" };
        println!(
            "{:>10}  {:>10}  {:<16}  {}{mark}",
            hya_core::fmt::bytes(e.size),
            hya_core::fmt::bytes(e.packed),
            e.modified.map(stamp).unwrap_or_default(),
            e.name,
        );
    }
}

/// `2026-09-04 13:27`, as the archiver's clock recorded it: ZIP stamps carry
/// no zone, so there is nothing to convert.
fn stamp(t: DosTime) -> String {
    format!(
        "{:04}-{:02}-{:02} {:02}:{:02}",
        t.year, t.month, t.day, t.hour, t.minute
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::sync::Arc;

    #[test]
    fn stamps_are_iso_like() {
        let t = DosTime {
            year: 2026,
            month: 9,
            day: 4,
            hour: 13,
            minute: 27,
            second: 30,
        };
        assert_eq!(stamp(t), "2026-09-04 13:27");
    }

    /// One stored entry whose data pushes the archive past the tail, so the
    /// listing needs a ranged GET rather than one fetch of the whole object.
    fn stored_zip(name: &str, data_len: usize) -> Vec<u8> {
        let (name, len) = (name.as_bytes(), data_len as u32);
        let mut z = Vec::new();
        z.extend_from_slice(&0x0403_4b50u32.to_le_bytes());
        z.extend_from_slice(&[20, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0]);
        z.extend_from_slice(&len.to_le_bytes());
        z.extend_from_slice(&len.to_le_bytes());
        z.extend_from_slice(&(name.len() as u16).to_le_bytes());
        z.extend_from_slice(&[0, 0]);
        z.extend_from_slice(name);
        z.resize(z.len() + data_len, 0);
        let dir_at = z.len() as u32;
        z.extend_from_slice(&0x0201_4b50u32.to_le_bytes());
        z.extend_from_slice(&[20, 0, 20, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0]);
        z.extend_from_slice(&len.to_le_bytes());
        z.extend_from_slice(&len.to_le_bytes());
        z.extend_from_slice(&(name.len() as u16).to_le_bytes());
        z.extend_from_slice(&[0; 12]);
        z.extend_from_slice(&0u32.to_le_bytes());
        z.extend_from_slice(name);
        let dir_len = z.len() as u32 - dir_at;
        z.extend_from_slice(&0x0605_4b50u32.to_le_bytes());
        z.extend_from_slice(&[0, 0, 0, 0, 1, 0, 1, 0]);
        z.extend_from_slice(&dir_len.to_le_bytes());
        z.extend_from_slice(&dir_at.to_le_bytes());
        z.extend_from_slice(&[0, 0]);
        z
    }

    /// A login-gated host: `/a.zip` answers `403` to any request without
    /// `sid=x`, and `/login` hands that cookie out on its way there. Counts
    /// the ranged GETs it served the archive to.
    async fn spawn_gated_origin(object: Vec<u8>) -> (u16, Arc<AtomicU64>) {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let l = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind");
        let port = l.local_addr().expect("addr").port();
        let object = Arc::new(object);
        let ranged = Arc::new(AtomicU64::new(0));
        let served = ranged.clone();
        tokio::spawn(async move {
            while let Ok((mut s, _)) = l.accept().await {
                let (object, served) = (object.clone(), served.clone());
                tokio::spawn(async move {
                    let mut head = Vec::new();
                    let mut buf = [0u8; 4096];
                    while !head.windows(4).any(|w| w == b"\r\n\r\n") {
                        match s.read(&mut buf).await {
                            Ok(0) | Err(_) => return,
                            Ok(n) => head.extend_from_slice(&buf[..n]),
                        }
                    }
                    let text = String::from_utf8_lossy(&head).to_ascii_lowercase();
                    let mut words = text.split_whitespace();
                    let (method, path) = (words.next().unwrap_or(""), words.next().unwrap_or(""));
                    let field = |name: &str| {
                        text.lines()
                            .find_map(|l| l.strip_prefix(name).map(|v| v.trim().to_string()))
                    };
                    let authed = field("cookie:").is_some_and(|c| c.contains("sid=x"));
                    let total = object.len();
                    let (head, body): (String, &[u8]) = match (path, field("range: bytes=")) {
                        ("/login", _) => (
                            format!(
                                "HTTP/1.1 302 Found\r\nLocation: http://127.0.0.1:{port}/a.zip\r\n\
                                 Set-Cookie: sid=x; Path=/\r\nContent-Length: 0\r\n\
                                 Connection: close\r\n\r\n"
                            ),
                            &[],
                        ),
                        (_, _) if !authed => (
                            "HTTP/1.1 403 Forbidden\r\nContent-Length: 0\r\n\
                             Connection: close\r\n\r\n"
                                .into(),
                            &[],
                        ),
                        (_, Some(r)) => {
                            let (lo, hi) = r.split_once('-').expect("a closed range");
                            let lo: usize = lo.parse().expect("lo");
                            let hi = hi.parse::<usize>().expect("hi").min(total - 1);
                            if hi - lo > 1 {
                                served.fetch_add(1, Ordering::SeqCst);
                            }
                            (
                                format!(
                                    "HTTP/1.1 206 Partial Content\r\n\
                                     Content-Range: bytes {lo}-{hi}/{total}\r\n\
                                     Content-Length: {}\r\nConnection: close\r\n\r\n",
                                    hi - lo + 1
                                ),
                                &object[lo..=hi],
                            )
                        }
                        (_, None) => (
                            format!(
                                "HTTP/1.1 200 OK\r\nAccept-Ranges: bytes\r\n\
                                 Content-Length: {total}\r\nConnection: close\r\n\r\n"
                            ),
                            &object[..],
                        ),
                    };
                    let _ = s.write_all(head.as_bytes()).await;
                    if method != "head" {
                        let _ = s.write_all(body).await;
                    }
                });
            }
        });
        (port, ranged)
    }

    fn cli(argv: &[&str]) -> crate::cli::Cli {
        let mut all = vec!["hydra", "--no-proxy", "--preview"];
        all.extend_from_slice(argv);
        crate::cli::Cli::parse_with_queries(all).expect("argv")
    }

    #[tokio::test]
    async fn the_listing_carries_the_cookie_the_user_passed() {
        let (port, ranged) = spawn_gated_origin(stored_zip("big.bin", 100_000)).await;
        let url = format!("http://127.0.0.1:{port}/a.zip");

        run(&url, &cli(&["-b", "sid=x", &url]))
            .await
            .expect("a listing");
        assert_eq!(ranged.load(Ordering::SeqCst), 1, "one ranged GET, authed");
    }

    #[tokio::test]
    async fn the_listing_carries_a_cookie_the_redirect_chain_set() {
        let (port, ranged) = spawn_gated_origin(stored_zip("big.bin", 100_000)).await;
        let url = format!("http://127.0.0.1:{port}/login");

        run(&url, &cli(&[&url])).await.expect("a listing");
        assert_eq!(ranged.load(Ordering::SeqCst), 1, "one ranged GET, authed");
    }

    #[tokio::test]
    async fn without_the_cookie_the_gate_is_reported() {
        let (port, ranged) = spawn_gated_origin(stored_zip("big.bin", 100_000)).await;
        let url = format!("http://127.0.0.1:{port}/a.zip");

        let err = run(&url, &cli(&[&url])).await.expect_err("refused");
        assert!(err.contains("403"), "{err}");
        assert_eq!(ranged.load(Ordering::SeqCst), 0);
    }
}
