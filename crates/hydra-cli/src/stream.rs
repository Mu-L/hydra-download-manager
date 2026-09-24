// Copyright (C) 2026 Javad Rajabzadeh
// SPDX-License-Identifier: GPL-3.0-or-later

//! HLS and DASH on the command line.
//!
//! `hydra <url>` takes a manifest the same way it takes a file — the URL is
//! fetched, and if the body turns out to be a playlist or an MPD it goes to
//! the stream path instead of the range scheduler. That is why detection
//! reads the BODY rather than the extension: a `.m3u8` URL that answers with
//! a login page is not a stream, and a manifest served under any other name
//! still is one.
//!
//! The parsing, planning and assembly all live in `hya-stream`, shared with
//! the desktop app, so both produce identical files from the same manifest.

use std::io::Write;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;

use hya_net::cookies::CookieJar;
use hya_net::{Target, TlsCapableConnector};
use hya_stream::{dash, hls};

/// What the caller asked for.
#[derive(Clone, Debug, Default)]
pub struct Job {
    pub url: String,
    pub output: Option<PathBuf>,
    pub output_dir: Option<PathBuf>,
    /// `--quality 720`: the rendition height to aim for. The nearest at or
    /// below it is used, never a larger one.
    pub quality: Option<u32>,
    /// `mp4` or `ts`.
    pub container: String,
    pub headers: Vec<String>,
    pub user_agent: String,
    /// The cookie flags' jar, read-only: a manifest, its segments and its keys
    /// commonly sit on different hosts, and each request takes only what the
    /// jar holds for its own.
    pub jar: Arc<CookieJar>,
    pub limit_rate: u64,
    pub quiet: bool,
    pub no_progress: bool,
    pub insecure: bool,
    /// `--list-streams`: report the renditions and exit.
    pub list: bool,
    /// `--record-seconds`: stop a live recording after this long and finish
    /// the file.
    pub record_seconds: Option<u64>,
    /// Segments in flight (`-x`). 0 takes the library's default.
    ///
    /// Fixed: `--adaptive` governs ranged file downloads and is not applied
    /// to segments. See `hya_stream::hls::Concurrency` for the measurement
    /// behind that.
    pub conns: usize,
}

/// How a stream attempt ended.
#[derive(Debug)]
pub enum Verdict {
    /// The body was not a manifest. The caller falls back to an ordinary
    /// download, which is what makes auto-detection safe.
    NotAManifest,
    Listed,
    /// Finished. The fields are what a `--json` summary would report; the
    /// exit code is all `main` needs today.
    Done {
        #[allow(dead_code)]
        path: PathBuf,
        #[allow(dead_code)]
        bytes: u64,
    },
    Failed(String),
}

fn target(seg: &hls::Segment, job: &Job) -> Result<Target, String> {
    let u = hya_stream::parse_url(&seg.url)?;
    let base = if u.tls {
        Target::direct_tls(&u.host, u.port, &u.path)
    } else {
        Target::direct(&u.host, u.port, &u.path)
    };
    let mut headers = job.headers.clone();
    // A playlist may carve every segment out of ONE file; without this the
    // whole file is fetched once per segment.
    if let Some(range) = seg.range_header() {
        headers.push(format!("Range: {range}"));
    }
    Ok(base
        .with_headers(headers, Some(job.user_agent.clone()))
        .with_jar(&job.jar, hya_net::cookies::now_secs()))
}

/// A segment redirected elsewhere: the same segment at the address the
/// origin named. The byte RANGE travels with it — a playlist that carves
/// segments out of one file, redirected to an edge, would otherwise fetch
/// the whole file per segment.
fn redirect_target(seg: &hls::Segment, e: &std::io::Error) -> Option<hls::Segment> {
    let loc = &hya_net::Redirect::of(e)?.location;
    let url = hya_stream::join(&seg.url, loc)?;
    Some(hls::Segment {
        url,
        range: seg.range,
        // The key travels too: a redirected segment is the same segment.
        key: seg.key.clone(),
    })
}

/// Hops a manifest fetch will follow. A CDN commonly answers the published
/// URL with a redirect to a regional edge, sometimes twice.
const MAX_REDIRECTS: usize = 5;

/// Fetch a manifest, following redirects, and report the URL it finally came
/// from — every relative URI inside it resolves against THAT, not against
/// the address originally asked for.
async fn get_at(
    conn: &Arc<TlsCapableConnector>,
    url: &str,
    job: &Job,
    cap: usize,
) -> std::io::Result<(Vec<u8>, String)> {
    let mut at = url.to_string();
    for _ in 0..MAX_REDIRECTS {
        let t = target(&hls::Segment::new(&at), job)
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidInput, e))?;
        match hya_net::fetch_small(conn.as_ref(), &t, cap).await {
            Ok(body) => return Ok((body, at)),
            Err(e) => {
                // `fetch_small` hands back the destination; a redirect is a
                // hop to take, not a failure to report.
                let Some(r) = hya_net::Redirect::of(&e) else {
                    return Err(e);
                };
                let Some(next) = hya_stream::join(&at, &r.location) else {
                    return Err(e);
                };
                at = next;
            }
        }
    }
    Err(std::io::Error::other("too many redirects"))
}

async fn get(
    conn: &Arc<TlsCapableConnector>,
    url: &str,
    job: &Job,
    cap: usize,
) -> std::io::Result<Vec<u8>> {
    get_at(conn, url, job, cap).await.map(|(body, _)| body)
}

/// A name for the finished file, when `--output` did not give one.
///
/// Manifests are almost always called something generic, so `index.m3u8`
/// would save every stream on the internet as `index`. The directory above
/// it is nearly always the asset id, which at least distinguishes them.
fn output_name(url: &str, ext: &str) -> String {
    let bare = url.split(['?', '#']).next().unwrap_or(url);
    // Only the PATH can name a file. Without this the host would, and
    // `https://cdn.example/` would save as `cdn.mp4`.
    let path = match bare.find("://") {
        Some(i) => {
            let rest = &bare[i + 3..];
            rest.find('/').map(|j| &rest[j..]).unwrap_or("")
        }
        None => bare,
    };
    let mut parts = path.rsplit('/').filter(|p| !p.is_empty());
    let file = parts.next().unwrap_or("stream");
    let stem = file.rsplit_once('.').map(|(s, _)| s).unwrap_or(file);
    const GENERIC: &[&str] = &[
        "index", "master", "manifest", "playlist", "mono", "stream", "media", "video", "main",
    ];
    let name = if GENERIC.contains(&stem.to_ascii_lowercase().as_str()) {
        parts.next().unwrap_or(stem)
    } else {
        stem
    };
    // Decoded here rather than before the generic-stem test above: the escape is
    // what the reader should not see, but `master`/`index` never carry one, and
    // deciding on the raw segment keeps that comparison exact. Sanitizing straight
    // afterwards is what makes decoding safe — `%2F` becomes a separator, `%00` a
    // NUL the OS would truncate the name at, and both land in the map below.
    let name: String = crate::url::pct_decode(name)
        .chars()
        .map(|c| match c {
            '/' | '\\' | ':' | '*' | '?' | '"' | '<' | '>' | '|' => '_',
            c if (c as u32) < 0x20 => '_',
            c => c,
        })
        .collect();
    format!("{}.{ext}", if name.is_empty() { "stream" } else { &name })
}

fn describe(v: &hls::Variant) -> String {
    let q = v.height.map(|h| format!("{h}p")).unwrap_or_default();
    let r = v
        .bandwidth
        .map(|b| format!("{} kbps", b / 1000))
        .unwrap_or_default();
    [q, r, v.codecs.clone().unwrap_or_default()]
        .iter()
        .filter(|s| !s.is_empty())
        .cloned()
        .collect::<Vec<_>>()
        .join("  ")
}

/// The jar the cookie flags describe, opened for the host `url` names: the
/// one a `-b` literal and a browser import are scoped to.
pub async fn open_jar(args: &crate::cli::Cli, url: &str) -> Result<Arc<CookieJar>, String> {
    let jar = match crate::url::Url::parse(url) {
        Some(u) => {
            crate::download::open_jar_for(args, &u.host, hya_net::cookies::now_secs()).await?
        }
        // Not fetchable as a stream either; that failure is reported where it
        // happens, with the URL in it.
        None => CookieJar::new(),
    };
    Ok(Arc::new(jar))
}

/// Whether `url` is worth trying as a manifest at all.
///
/// The decision is finally made on the BODY, but fetching every URL to find
/// out would mean pulling megabytes of an ordinary file before discovering
/// it is one. So the extension gates the attempt and the body settles it: a
/// `.m3u8` that answers with a login page still falls through to a normal
/// download, and it cost one small GET to learn that.
pub fn looks_like_manifest(url: &str) -> bool {
    let path = url
        .split(['?', '#'])
        .next()
        .unwrap_or(url)
        .to_ascii_lowercase();
    path.ends_with(".m3u8") || path.ends_with(".m3u") || path.ends_with(".mpd")
}

/// Try `url` as a stream. Returns [`Verdict::NotAManifest`] when it is not
/// one, so the caller can fall through to a normal download.
pub async fn run(job: Job) -> Verdict {
    let conn = match TlsCapableConnector::with_insecure(job.insecure) {
        Ok(c) => Arc::new(c),
        Err(e) => return Verdict::Failed(e.to_string()),
    };

    // `base` is where the manifest ACTUALLY came from after redirects, not
    // the address asked for. Every relative URI inside resolves against it,
    // so following a hop and then parsing against the original would aim
    // every segment at the wrong host.
    let (body, base) = match get_at(&conn, &job.url, &job, hls::playlist_cap()).await {
        Ok(b) => b,
        // Unreachable, or larger than any manifest: either way this is not a
        // stream, and the caller reports it the way it reports anything else.
        Err(_) => return Verdict::NotAManifest,
    };
    let text = String::from_utf8_lossy(&body).into_owned();
    let is_dash = if text.trim_start().starts_with("#EXTM3U") {
        false
    } else if text.contains("<MPD") {
        true
    } else {
        return Verdict::NotAManifest;
    };

    let cancel = Arc::new(AtomicBool::new(false));
    // Ctrl-C stops a recording and keeps the file, which is the only useful
    // meaning it can have for a live stream.
    {
        let cancel = cancel.clone();
        tokio::spawn(async move {
            if tokio::signal::ctrl_c().await.is_ok() {
                cancel.store(true, Ordering::Relaxed);
            }
        });
    }

    let want_ext = if job.container.eq_ignore_ascii_case("ts") {
        "ts"
    } else {
        "mp4"
    };
    let out_path = job.output.clone().unwrap_or_else(|| {
        let name = output_name(&job.url, want_ext);
        match &job.output_dir {
            Some(d) => d.join(name),
            None => PathBuf::from(name),
        }
    });

    if is_dash {
        run_dash(&conn, &job, &text, &base, &out_path, want_ext, &cancel).await
    } else {
        run_hls(&conn, &job, text, &base, &out_path, want_ext, &cancel).await
    }
}

#[allow(clippy::too_many_arguments)]
async fn run_hls(
    conn: &Arc<TlsCapableConnector>,
    job: &Job,
    text: String,
    base: &str,
    out_path: &PathBuf,
    want_ext: &str,
    cancel: &Arc<AtomicBool>,
) -> Verdict {
    let mut playlist = hls::parse(&text, base);
    let mut bandwidth = None;
    let mut source = base.to_string();
    // Set from the master, before `playlist` becomes the media playlist:
    // once that happens the rendition groups are gone.
    let mut audio_url: Option<String> = None;

    if playlist.is_master() {
        if job.list {
            println!("HLS  {}", job.url);
            for v in &playlist.variants {
                println!("  {}", describe(v));
            }
            return Verdict::Listed;
        }
        let Some(chosen) = hls::choose(&playlist.variants, None, job.quality).cloned() else {
            return Verdict::Failed("the master playlist lists no variants".into());
        };
        bandwidth = chosen.bandwidth;
        audio_url = playlist.audio_for(&chosen).and_then(|r| r.url.clone());
        if !job.quiet {
            eprintln!(
                "stream: HLS {}{}",
                describe(&chosen),
                if audio_url.is_some() { "  + audio" } else { "" }
            );
        }
        let body = match get(conn, &chosen.url, job, hls::playlist_cap()).await {
            Ok(b) => b,
            Err(e) => return Verdict::Failed(format!("could not read the variant playlist: {e}")),
        };
        source = chosen.url.clone();
        playlist = hls::parse(&String::from_utf8_lossy(&body), &chosen.url);
    } else if job.list {
        println!("HLS  {}  (single rendition)", job.url);
        return Verdict::Listed;
    }

    if playlist.live {
        // `Plan::build` — which refuses DRM and encryption — is only reached
        // by the VOD path below, so its refusals are restated here. The
        // recorder cannot decrypt (keys rotate mid-live and nothing fetches
        // them), and appending ciphertext would finish "successfully" over a
        // file of noise.
        if let Some(d) = &playlist.drm {
            return Verdict::Failed(format!("{d} DRM is not supported"));
        }
        if let Some(enc) = &playlist.encryption {
            return Verdict::Failed(format!("recording {enc} live streams is not supported yet"));
        }
        let mut windows = vec![Window::of(&playlist)];
        if let Some(au) = &audio_url {
            // Primed here rather than left to the first refresh: the track
            // count is fixed from this point, and a second track that starts
            // a window late is a recording whose sound is permanently behind
            // its picture.
            match media_playlist(conn, job, au).await {
                Ok(apl) => windows.push(Window::of(&apl)),
                Err(e) => {
                    eprintln!("stream: audio unusable, recording video only ({e})");
                    audio_url = None;
                }
            }
        }
        return record(
            conn,
            job,
            Live::Hls {
                url: source,
                audio_url,
            },
            windows,
            playlist.refresh_after(),
            out_path,
            want_ext,
            cancel,
        )
        .await;
    }

    let plan = match hls::Plan::build(&playlist, bandwidth) {
        Ok(p) => p,
        Err(r) => return Verdict::Failed(r.to_string()),
    };
    // A variant that names an audio rendition group carries no sound of its
    // own; muxing the rendition back in is what keeps the finished file from
    // being silent.
    let mut plans = vec![plan];
    if let Some(au) = &audio_url {
        match media_playlist(conn, job, au).await {
            // Audio that will not resolve is not a reason to lose the video.
            Ok(apl) => match hls::Plan::build(&apl, None) {
                Ok(p) => plans.push(p),
                Err(e) => eprintln!("stream: audio unusable, video only ({e})"),
            },
            Err(e) => eprintln!("stream: audio unusable, video only ({e})"),
        }
    }
    assemble(conn, job, plans, out_path, want_ext, cancel).await
}

#[allow(clippy::too_many_arguments)]
async fn run_dash(
    conn: &Arc<TlsCapableConnector>,
    job: &Job,
    text: &str,
    base: &str,
    out_path: &PathBuf,
    want_ext: &str,
    cancel: &Arc<AtomicBool>,
) -> Verdict {
    let mf = dash::parse(text, base);
    if job.list {
        println!("DASH  {}", job.url);
        for t in &mf.video {
            println!(
                "  video  {}  {}",
                t.height.map(|h| format!("{h}p")).unwrap_or_default(),
                t.bandwidth
                    .map(|b| format!("{} kbps", b / 1000))
                    .unwrap_or_default()
            );
        }
        for t in &mf.audio {
            println!(
                "  audio  {}  {} kbps",
                t.id,
                t.bandwidth.unwrap_or(0) / 1000
            );
        }
        return Verdict::Listed;
    }
    let Some(video) = mf.choose_video(job.quality) else {
        return Verdict::Failed("the manifest lists no video renditions".into());
    };
    let audio = mf.choose_audio();
    if !job.quiet {
        eprintln!(
            "stream: DASH {}  {} kbps{}",
            video.height.map(|h| format!("{h}p")).unwrap_or_default(),
            video.bandwidth.unwrap_or(0) / 1000,
            if audio.is_some() { " + audio" } else { "" }
        );
    }

    if mf.live {
        // As above: the VOD refusal below is never reached on this path.
        if let Some(d) = &mf.drm {
            return Verdict::Failed(format!("{d} DRM is not supported"));
        }
        let mut windows = vec![Window::dash(video)];
        if let Some(a) = audio {
            windows.push(Window::dash(a));
        }
        return record(
            conn,
            job,
            Live::Dash {
                url: base.to_string(),
                video_id: video.id.clone(),
                audio_id: audio.map(|a| a.id.clone()),
            },
            windows,
            mf.refresh_after(),
            out_path,
            want_ext,
            cancel,
        )
        .await;
    }

    let mut plans = match mf.plan(video) {
        Ok(p) => vec![p],
        Err(r) => return Verdict::Failed(r.to_string()),
    };
    if let Some(a) = audio {
        match mf.plan(a) {
            Ok(p) => plans.push(p),
            Err(e) => eprintln!("stream: audio unusable, video only ({e})"),
        }
    }
    assemble(conn, job, plans, out_path, want_ext, cancel).await
}

/// Fetch a playlist and parse it against the URL it came from.
async fn media_playlist(
    conn: &Arc<TlsCapableConnector>,
    job: &Job,
    url: &str,
) -> Result<hls::Playlist, String> {
    let body = get(conn, url, job, hls::playlist_cap())
        .await
        .map_err(|e| e.to_string())?;
    Ok(hls::parse(&String::from_utf8_lossy(&body), url))
}

// ------------------------------------------------------------- assembly

fn fetcher(
    conn: &Arc<TlsCapableConnector>,
    job: &Job,
    cancel: &Arc<AtomicBool>,
) -> impl hya_stream::Fetcher {
    // ONE limiter for the whole transfer, built here rather than inside the
    // closure. A limiter per segment is a limit per segment: with eight in
    // flight, `--limit-rate 1M` would have allowed eight.
    let limiter = Arc::new(hya_net::polite::RateLimiter::new(job.limit_rate));
    let (conn, job, cancel) = (conn.clone(), job.clone(), cancel.clone());
    move |seg: hls::Segment, dest: String, counter: Arc<AtomicU64>| {
        let (conn, job, cancel, limiter) =
            (conn.clone(), job.clone(), cancel.clone(), limiter.clone());
        Box::pin(async move {
            // A CDN may bounce a segment to a regional edge; follow it.
            let mut seg = seg;
            for hop in 0..=MAX_REDIRECTS {
                let t = target(&seg, &job)
                    .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidInput, e))?;
                let pace = hya_net::polite::Pace::shared(limiter.clone());
                // Keep the socket: a playlist is hundreds of small objects on
                // one origin, and a handshake each would dominate the
                // transfer.
                let pool = hya_net::Connector::pool(conn.as_ref());
                match hya_net::fetch_object(
                    conn.as_ref(),
                    &t,
                    &dest,
                    &counter,
                    Some(&cancel),
                    &pace,
                    pool.as_ref(),
                )
                .await
                {
                    Ok(n) => return Ok(n),
                    Err(e) if hop < MAX_REDIRECTS => {
                        let Some(next) = redirect_target(&seg, &e) else {
                            return Err(e);
                        };
                        seg = next;
                    }
                    Err(e) => return Err(e),
                }
            }
            Err(std::io::Error::other("too many redirects"))
        })
    }
}

pub(crate) fn human(n: u64) -> String {
    const U: [&str; 4] = ["B", "KB", "MB", "GB"];
    let mut v = n as f64;
    let mut i = 0;
    while v >= 1024.0 && i < 3 {
        v /= 1024.0;
        i += 1;
    }
    format!("{v:.1} {}", U[i])
}

/// A progress line on stderr, so stdout stays clean for `--json` and pipes.
fn spawn_progress(
    meter: Arc<hls::Meter>,
    total: Option<u64>,
    cancel: Arc<AtomicBool>,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let start = std::time::Instant::now();
        loop {
            tokio::time::sleep(std::time::Duration::from_millis(400)).await;
            if cancel.load(Ordering::Relaxed) {
                return;
            }
            let (bytes, segs, _) = meter.snapshot();
            let secs = start.elapsed().as_secs_f64().max(0.001);
            let rate = bytes as f64 / secs;
            let where_ = match total {
                Some(t) => format!("segment {}/{t}", segs.min(t)),
                None => format!("recording, {segs} segments"),
            };
            eprint!(
                "\r\x1b[K{where_}  {}  {}/s",
                human(bytes),
                human(rate as u64)
            );
            let _ = std::io::stderr().flush();
        }
    })
}

async fn assemble(
    conn: &Arc<TlsCapableConnector>,
    job: &Job,
    plans: Vec<hls::Plan>,
    out_path: &PathBuf,
    want_ext: &str,
    cancel: &Arc<AtomicBool>,
) -> Verdict {
    let staging = format!("{}.part", out_path.display());
    // AES-128 keys, fetched with the same session as the manifest.
    let mut keys = hls::Keys::new();
    for plan in &plans {
        for uri in plan.key_uris() {
            if keys.contains_key(&uri) {
                continue;
            }
            match get(conn, &uri, job, hls::KEY_FETCH_CAP).await {
                Ok(bytes) if bytes.len() == 16 => {
                    let mut k = [0u8; 16];
                    k.copy_from_slice(&bytes);
                    keys.insert(uri, k);
                }
                Ok(bytes) => {
                    return Verdict::Failed(format!(
                        "the AES-128 key at {uri} is {} bytes, not 16",
                        bytes.len()
                    ))
                }
                Err(e) => {
                    return Verdict::Failed(format!("could not fetch the AES-128 key {uri}: {e}"))
                }
            }
        }
    }
    if !keys.is_empty() && !job.quiet {
        eprintln!("stream: AES-128, {} key(s)", keys.len());
    }

    let meter = Arc::new(hls::Meter::default());
    let total: u64 = plans.iter().map(|p| p.segments.len() as u64).sum();
    let progress = (!job.quiet && !job.no_progress)
        .then(|| spawn_progress(meter.clone(), Some(total), cancel.clone()));
    let fetch = fetcher(conn, job, cancel);

    let mut parts = Vec::new();
    for (i, plan) in plans.iter().enumerate() {
        let part = format!("{staging}.t{i}");
        let ckpt = format!("{part}.ck");
        let saved = hls::Checkpoint::read(&ckpt);
        let resumable = saved.usable(&part, plan);
        let file = if resumable {
            meter.preload(saved.bytes, saved.segments);
            std::fs::OpenOptions::new().append(true).open(&part)
        } else {
            let _ = std::fs::remove_file(&ckpt);
            std::fs::File::create(&part)
        };
        let mut out = match file {
            Ok(f) => f,
            Err(e) => return Verdict::Failed(format!("{}: {e}", part)),
        };
        let resume = hls::Resume {
            skip: if resumable {
                saved.segments as usize
            } else {
                0
            },
            bytes: if resumable { saved.bytes } else { 0 },
            checkpoint: Some(ckpt.as_str()),
        };
        match hls::fetch_all(
            plan,
            &mut out,
            &format!("{staging}.s{i}"),
            fetch.clone(),
            &meter,
            cancel,
            resume,
            hls::Concurrency::fixed(job.conns),
            &keys,
        )
        .await
        {
            Ok(_) => parts.push((part, Some(ckpt))),
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {
                if let Some(p) = progress {
                    p.abort();
                }
                eprintln!("\nstopped; {} kept for resume", part);
                return Verdict::Failed("interrupted".into());
            }
            Err(e) => {
                if let Some(p) = progress {
                    p.abort();
                }
                return Verdict::Failed(format!("segment download failed: {e}"));
            }
        }
    }
    if let Some(p) = progress {
        p.abort();
    }
    if !job.quiet {
        eprintln!();
    }
    let kinds: Vec<hls::Segments> = plans.iter().map(|p| p.kind).collect();
    finish(&kinds, &parts, out_path, want_ext, job)
}

fn finish(
    // One per staging file, in the same order.
    kinds: &[hls::Segments],
    // `(staging file, checkpoint sidecar)`; a recording has no sidecar.
    parts: &[(String, Option<String>)],
    out_path: &PathBuf,
    want_ext: &str,
    job: &Job,
) -> Verdict {
    // Nothing arrived: say so, rather than asking a muxer to explain it.
    if parts
        .iter()
        .any(|(p, _)| std::fs::metadata(p).map(|m| m.len()).unwrap_or(0) == 0)
    {
        for (p, c) in parts {
            let _ = std::fs::remove_file(p);
            if let Some(c) = c {
                let _ = std::fs::remove_file(c);
            }
        }
        return Verdict::Failed("nothing was downloaded".into());
    }
    if let Some(dir) = out_path.parent() {
        if !dir.as_os_str().is_empty() {
            let _ = std::fs::create_dir_all(dir);
        }
    }
    let result = if parts.len() == 2 {
        if !job.quiet {
            eprintln!("stream: combining video and audio");
        }
        hls::mux(
            std::path::Path::new(&parts[0].0),
            std::path::Path::new(&parts[1].0),
            out_path,
            kinds[1],
        )
        .map(|()| hls::Finished::Remuxed)
    } else {
        hls::finish(
            kinds[0],
            want_ext,
            std::path::Path::new(&parts[0].0),
            out_path,
        )
    };
    match result {
        Ok(finished) => {
            if let hls::Finished::RemuxSkipped(why) = &finished {
                if !job.quiet {
                    // Playable, but the fragmented assembly rather than the
                    // faststart MP4 that was asked for.
                    eprintln!("stream: kept the assembled file ({why})");
                }
            }
            for (p, c) in parts {
                let _ = std::fs::remove_file(p);
                if let Some(c) = c {
                    let _ = std::fs::remove_file(c);
                }
            }
            let bytes = std::fs::metadata(out_path).map(|m| m.len()).unwrap_or(0);
            if !job.quiet {
                eprintln!("stream: {} ({})", out_path.display(), human(bytes));
            }
            Verdict::Done {
                path: out_path.clone(),
                bytes,
            }
        }
        // The tracks are playable on their own; keeping them beats deleting
        // the download because the last step could not run.
        Err(e) => Verdict::Failed(e),
    }
}

// ---------------------------------------------------------------- live

enum Live {
    Hls {
        url: String,
        /// The `#EXT-X-MEDIA` rendition the variant plays with, when the
        /// sound is a playlist of its own.
        audio_url: Option<String>,
    },
    Dash {
        url: String,
        video_id: String,
        audio_id: Option<String>,
    },
}

/// A track's current window: its init map, then `(sequence, segment,
/// seconds)` per entry. The DURATION rides along because `--record-seconds`
/// is a promise about media, and only the manifest knows how much media a
/// segment is worth.
struct Window {
    init: Option<hls::Segment>,
    segments: Vec<(u64, hls::Segment, f64)>,
    /// What the segments are framed in. Two tracks of a recording need not
    /// agree — an HLS variant may be MPEG-TS while its audio rendition is
    /// fragmented MP4 — and the muxer has to be told which.
    kind: hls::Segments,
}

type WindowList = Vec<Window>;

impl Window {
    fn of(pl: &hls::Playlist) -> Window {
        Window {
            init: pl.init.clone(),
            segments: pl.timed_window(),
            kind: pl.segments_kind.unwrap_or(hls::Segments::Ts),
        }
    }

    fn dash(t: &dash::Track) -> Window {
        Window {
            init: t.init.clone(),
            segments: dash::Manifest::timed_window(t),
            kind: hls::Segments::Fmp4,
        }
    }
}

async fn refresh(
    conn: &Arc<TlsCapableConnector>,
    job: &Job,
    source: &Live,
) -> Result<(WindowList, bool, std::time::Duration), String> {
    match source {
        Live::Hls { url, audio_url } => {
            let pl = media_playlist(conn, job, url)
                .await
                .map_err(|e| format!("could not re-read the playlist: {e}"))?;
            if let Some(d) = &pl.drm {
                return Err(format!("{d} DRM is not supported"));
            }
            // Encryption can appear part-way through a live stream, so this
            // is checked on every refresh and not only at the start.
            if let Some(enc) = &pl.encryption {
                return Err(format!("recording {enc} live streams is not supported yet"));
            }
            let mut windows = vec![Window::of(&pl)];
            if let Some(au) = audio_url {
                // The track count was fixed when the recording started, so a
                // window that cannot be read now has to stop the recording
                // rather than quietly leave the audio behind the picture.
                let apl = media_playlist(conn, job, au)
                    .await
                    .map_err(|e| format!("could not re-read the audio playlist: {e}"))?;
                windows.push(Window::of(&apl));
            }
            Ok((windows, pl.ended, pl.refresh_after()))
        }
        Live::Dash {
            url,
            video_id,
            audio_id,
        } => {
            let body = get(conn, url, job, hls::playlist_cap())
                .await
                .map_err(|e| format!("could not re-read the manifest: {e}"))?;
            let mf = dash::parse(&String::from_utf8_lossy(&body), url);
            if let Some(d) = &mf.drm {
                return Err(format!("{d} DRM is not supported"));
            }
            let Some(v) = mf.video.iter().find(|t| &t.id == video_id) else {
                return Err("the manifest no longer offers that video rendition".into());
            };
            let mut w = vec![Window::dash(v)];
            if let Some(aid) = audio_id {
                if let Some(a) = mf.audio.iter().find(|t| &t.id == aid) {
                    w.push(Window::dash(a));
                }
            }
            Ok((w, false, mf.refresh_after()))
        }
    }
}

/// Record a live stream until it ends or Ctrl-C.
#[allow(clippy::too_many_arguments)]
async fn record(
    conn: &Arc<TlsCapableConnector>,
    job: &Job,
    source: Live,
    primed: WindowList,
    first_refresh: std::time::Duration,
    out_path: &PathBuf,
    want_ext: &str,
    cancel: &Arc<AtomicBool>,
) -> Verdict {
    let staging = format!("{}.part", out_path.display());
    let tracks = primed.len();
    let mut files = Vec::with_capacity(tracks);
    let mut parts = Vec::with_capacity(tracks);
    for i in 0..tracks {
        let part = format!("{staging}.t{i}");
        match std::fs::File::create(&part) {
            Ok(f) => {
                files.push(f);
                // A live recording keeps no checkpoint: there is nothing
                // to resume into once the broadcast has moved on.
                parts.push((part, None));
            }
            Err(e) => return Verdict::Failed(format!("{part}: {e}")),
        }
    }
    if !job.quiet {
        match job.record_seconds {
            Some(s) => eprintln!("stream: recording {s}s, then finishing the file"),
            None => eprintln!("stream: recording; press Ctrl-C to stop and keep the file"),
        }
    }

    let meter = Arc::new(hls::Meter::default());
    let progress = (!job.quiet && !job.no_progress)
        .then(|| spawn_progress(meter.clone(), None, cancel.clone()));
    let fetch = fetcher(conn, job, cancel);

    // Segments in flight. Resolved exactly as the assembly path resolves it,
    // so `-n` means the same thing for a recording as for a download.
    let conns = hls::Concurrency::fixed(job.conns).ceiling();
    let gate = Arc::new(tokio::sync::Semaphore::new(conns));
    meter.open_lanes(conns);

    let mut last: Vec<Option<u64>> = vec![None; tracks];
    let mut init_done = vec![false; tracks];
    let mut kinds = vec![hls::Segments::Ts; tracks];
    // Media already PLANNED per track, which is what the length ask is
    // measured against. Per track because video and audio cover the same
    // span, so summing them would halve the recording.
    let mut planned_secs = vec![0.0f64; tracks];
    let mut taken = 0u64;
    let mut barren = 0u32;
    // How far BELOW the last taken number counts as a restarted sequence
    // rather than a stale republish.
    const RESET_GAP: u64 = 64;
    let started = std::time::Instant::now();
    let deadline = job
        .record_seconds
        .map(|s| started + std::time::Duration::from_secs(s));
    let done_recording = |cancel: &AtomicBool| {
        cancel.load(Ordering::Relaxed) || deadline.is_some_and(|d| std::time::Instant::now() >= d)
    };
    let mut primed = Some((primed, false, first_refresh));
    let mut error = None;

    'recording: loop {
        let got = match primed.take() {
            Some(w) => Ok(w),
            None => refresh(conn, job, &source).await,
        };
        let (windows, ended, wait) = match got {
            Ok(w) => w,
            Err(e) if taken > 0 => {
                eprintln!("\nstream: {e}");
                break;
            }
            Err(e) => {
                error = Some(e);
                break;
            }
        };

        for (i, w) in windows.iter().enumerate().take(tracks) {
            let (init, segments) = (&w.init, &w.segments);
            kinds[i] = w.kind;
            if !init_done[i] {
                if let Some(u) = init {
                    let dest = format!("{staging}.i{i}");
                    let c = Arc::new(AtomicU64::new(0));
                    meter.begin(format!("init {i}"), c.clone());
                    // An init map is load-bearing: it carries ftyp+moov, and
                    // fragments written without it in front are not a file
                    // any player will open. Swallowing a failure here and
                    // recording on would manufacture exactly that, silently.
                    let placed = match fetch(u.clone(), dest.clone(), c.clone()).await {
                        Ok(n) => append(&dest, &mut files[i]).map(|()| n),
                        Err(e) => Err(e),
                    };
                    match placed {
                        Ok(n) => meter.settle(&c, n),
                        Err(e) => {
                            meter.drop_inflight(&c);
                            let _ = std::fs::remove_file(&dest);
                            error = Some(format!("could not place the init segment: {e}"));
                            break 'recording;
                        }
                    }
                }
                init_done[i] = true;
            }
            // Planned in order, fetched concurrently — the same shape the
            // GUI recorder uses. The checks below are stateful, so they must
            // run in sequence; the fetching need not, and taking one segment
            // at a time made a recording run on a single connection however
            // many `-n` asked for.
            let mut wanted: Vec<(u64, hls::Segment, f64)> = Vec::new();
            for (seq, url, secs) in segments {
                // A restarted encoder renumbers from a low value; without
                // this the recording silently freezes after a restart,
                // because every new segment looks like one already taken.
                if let Some(l) = last[i] {
                    if seq.saturating_add(RESET_GAP) < l {
                        if !job.quiet {
                            eprintln!("\nstream: sequence restarted at {seq} (was {l})");
                        }
                        last[i] = None;
                    }
                }
                if last[i].is_some_and(|l| *seq <= l) {
                    continue;
                }
                // Marked taken at planning time: a segment that then fails is
                // a gap the recording moves past, as it was before.
                last[i] = Some(*seq);
                wanted.push((*seq, url.clone(), *secs));
            }
            if done_recording(cancel) {
                break 'recording;
            }

            // `--record-seconds` is a promise about how much MEDIA comes
            // back, so it cuts the plan rather than the loop. The rule lives
            // in hya-stream, shared with the desktop recorder.
            if let Some(max) = job.record_seconds {
                hls::trim_to_budget(
                    &mut wanted,
                    &mut planned_secs[i],
                    max as f64,
                    |(_, _, secs)| *secs,
                );
            }

            let mut queued: std::collections::VecDeque<_> = std::collections::VecDeque::new();
            for (n, (seq, url, _secs)) in wanted.into_iter().enumerate() {
                // Distinct per segment: one shared staging name would have
                // concurrent fetches writing over each other.
                let dest = format!("{staging}.s{i}.{n}");
                let c = Arc::new(AtomicU64::new(0));
                meter.begin(format!("segment {seq}"), c.clone());
                let (f, g, d2, c2, m2) = (
                    fetch.clone(),
                    gate.clone(),
                    dest.clone(),
                    c.clone(),
                    meter.clone(),
                );
                let task = tokio::spawn(async move {
                    let _permit = match g.acquire().await {
                        Ok(p) => p,
                        Err(_) => return Err(std::io::Error::other("fetch gate closed")),
                    };
                    let lane = m2.occupy(&c2);
                    // Bounded like every other attempt: an origin that
                    // accepts and then goes silent would otherwise block the
                    // append loop forever, with the deadline never reached
                    // and Ctrl-C unable to land.
                    let r = match tokio::time::timeout(hls::ATTEMPT_TIMEOUT, f(url, d2, c2.clone()))
                        .await
                    {
                        Ok(r) => r,
                        Err(_) => Err(std::io::Error::new(
                            std::io::ErrorKind::TimedOut,
                            "live segment stalled",
                        )),
                    };
                    if let Some(l) = lane {
                        l.finish(r.is_ok());
                    }
                    r
                });
                queued.push_back((dest, c, task));
            }

            // Appended in playlist order however they land. A stop drains
            // the rest rather than breaking out, so nothing is left staged.
            let mut interrupted = false;
            while let Some((dest, c, task)) = queued.pop_front() {
                let outcome = match task.await {
                    Ok(r) => r,
                    Err(e) => Err(std::io::Error::other(format!("segment task: {e}"))),
                };
                // Per segment, not per window: a `--record-seconds` ask must
                // not overrun by however much the window happened to hold.
                if !interrupted && done_recording(cancel) {
                    interrupted = true;
                }
                if interrupted {
                    meter.drop_inflight(&c);
                    let _ = std::fs::remove_file(&dest);
                    continue;
                }
                match outcome {
                    Ok(n) if append(&dest, &mut files[i]).is_ok() => {
                        meter.settle(&c, n);
                        taken += 1;
                    }
                    Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {
                        meter.drop_inflight(&c);
                        let _ = std::fs::remove_file(&dest);
                        interrupted = true;
                    }
                    // One lost segment is a gap in a live recording, not the
                    // end of it.
                    _ => {
                        meter.drop_inflight(&c);
                        let _ = std::fs::remove_file(&dest);
                    }
                }
            }
            if interrupted {
                break 'recording;
            }
        }

        if taken == 0 {
            barren += 1;
            if barren >= 3 {
                error = Some("the manifest published no segments; nothing to record".into());
                break;
            }
        }
        if ended || done_recording(cancel) {
            break;
        }
        let until = std::time::Instant::now() + wait;
        while std::time::Instant::now() < until {
            if done_recording(cancel) {
                break 'recording;
            }
            tokio::time::sleep(std::time::Duration::from_millis(200)).await;
        }
    }

    if let Some(p) = progress {
        p.abort();
    }
    for i in 0..tracks {
        let _ = std::fs::remove_file(format!("{staging}.s{i}"));
        let _ = std::fs::remove_file(format!("{staging}.i{i}"));
    }
    drop(files);
    if !job.quiet {
        eprintln!();
    }
    // A recording that captured nothing is a failure, not a file. Without
    // this the empty staging file is handed to the muxer, which fails with
    // something about invalid data instead of the actual problem.
    if error.is_none() && taken == 0 {
        error = Some(
            "no segments could be fetched from this stream (the origin refused every one)".into(),
        );
    }
    if let Some(e) = error {
        for (p, _) in &parts {
            let _ = std::fs::remove_file(p);
        }
        return Verdict::Failed(e);
    }
    finish(&kinds, &parts, out_path, want_ext, job)
}

fn append(src: &str, out: &mut std::fs::File) -> std::io::Result<()> {
    let mut f = std::fs::File::open(src)?;
    std::io::copy(&mut f, out)?;
    drop(f);
    let _ = std::fs::remove_file(src);
    Ok(())
}

// ------------------------------------------------------- file inspection

/// `--inspect` on something that is not a manifest: report what the server
/// says about the object, without downloading it.
///
/// Everything here comes from ONE request's headers, which is why it is
/// worth having — the alternative is starting a download to find out how big
/// it is, what it is called, and whether it can be resumed.
pub async fn inspect_file(job: &Job) -> Result<(), String> {
    let conn = TlsCapableConnector::with_insecure(job.insecure).map_err(|e| e.to_string())?;
    let mut url = job.url.clone();
    let mut hops = 0u32;

    let probe = loop {
        let t = target(&hls::Segment::new(&url), job)?;
        let p = hya_net::probe_resilient(&conn, &t)
            .await
            .map_err(|e| format!("could not reach {url}: {e}"))?;
        // A redirector's own headers describe the redirect, not the file.
        if p.is_redirect() {
            let Some(next) = p
                .location
                .as_deref()
                .and_then(|loc| hya_stream::join(&url, loc))
            else {
                break p;
            };
            hops += 1;
            if hops > 10 {
                return Err("too many redirects".into());
            }
            println!("  redirect   -> {next}");
            url = next;
            continue;
        }
        break p;
    };

    if let Some(why) = probe.refusal() {
        return Err(format!("the {why} for {url}"));
    }

    let name = probe
        .suggested_filename()
        .unwrap_or_else(|| file_name_of(&url));
    let ext = name
        .rsplit_once('.')
        .map(|(_, e)| e.to_ascii_lowercase())
        .filter(|e| !e.is_empty() && e.len() <= 5)
        .or_else(|| extension_for(probe.content_type.as_deref()?).map(str::to_string));

    println!("URL          {url}");
    println!("File name    {name}");
    match probe.stated_length() {
        Some(n) => println!("Size         {} ({n} bytes)", human(n)),
        None => println!("Size         unknown (the server states no length)"),
    }
    if let Some(ct) = &probe.content_type {
        println!("Content type {ct}");
    }
    if let Some(e) = &ext {
        println!("Extension    .{e}");
    }
    // The single most useful fact about a download: whether losing the
    // connection costs you everything.
    println!(
        "Resumable    {}",
        if probe.ranges {
            "yes (the server accepts byte ranges)"
        } else {
            "no (a lost connection restarts it)"
        }
    );
    if let Some(v) = &probe.validator {
        println!(
            "Validator    {v}{}",
            if probe.weak_validator { "  (weak)" } else { "" }
        );
    }
    if let Some(lm) = &probe.last_modified {
        // Servers state times in GMT; the person reading this is not in GMT.
        println!("Modified     {}", to_local(lm));
    }
    if let Some(d) = &probe.disposition {
        println!("Disposition  {d}");
    }
    Ok(())
}

/// Render an HTTP date in the reader's own time zone.
///
/// RFC 9110 requires GMT on the wire, which is right for the protocol and
/// unhelpful on a terminal: "was this newer than my copy?" is a question
/// about local time. The original is left alone when it cannot be parsed —
/// a wrong local time would be worse than an honest GMT one.
fn to_local(http_date: &str) -> String {
    use chrono::{DateTime, Local, NaiveDateTime, TimeZone, Utc};
    let raw = http_date.trim();
    // RFC 1123 (the required form), then the two obsolete ones RFC 9110 says
    // a client must still accept.
    let parsed = DateTime::parse_from_rfc2822(raw)
        .map(|d| d.with_timezone(&Utc))
        .ok()
        .or_else(|| {
            NaiveDateTime::parse_from_str(raw, "%A, %d-%b-%y %H:%M:%S GMT")
                .ok()
                .map(|n| Utc.from_utc_datetime(&n))
        })
        .or_else(|| {
            NaiveDateTime::parse_from_str(raw, "%a %b %e %H:%M:%S %Y")
                .ok()
                .map(|n| Utc.from_utc_datetime(&n))
        });
    match parsed {
        Some(utc) => utc
            .with_timezone(&Local)
            .format("%Y-%m-%d %H:%M:%S %Z")
            .to_string(),
        None => raw.to_string(),
    }
}

fn file_name_of(url: &str) -> String {
    crate::url::Url::parse(url)
        .map(|u| u.suggested_filename())
        .unwrap_or_else(|| "download".into())
}

/// The extension a MIME type implies, for a URL whose path has none.
fn extension_for(content_type: &str) -> Option<&'static str> {
    let ct = content_type
        .split(';')
        .next()
        .unwrap_or("")
        .trim()
        .to_ascii_lowercase();
    Some(match ct.as_str() {
        "video/mp4" => "mp4",
        "video/webm" => "webm",
        "video/x-matroska" => "mkv",
        "video/quicktime" => "mov",
        "video/mp2t" => "ts",
        "audio/mpeg" => "mp3",
        "audio/mp4" | "audio/x-m4a" => "m4a",
        "audio/ogg" => "ogg",
        "audio/flac" | "audio/x-flac" => "flac",
        "audio/wav" | "audio/x-wav" => "wav",
        "application/pdf" => "pdf",
        "application/zip" => "zip",
        "application/gzip" | "application/x-gzip" => "gz",
        "application/x-7z-compressed" => "7z",
        "application/x-rar-compressed" | "application/vnd.rar" => "rar",
        "application/x-tar" => "tar",
        "application/x-apple-diskimage" => "dmg",
        "application/x-msdownload" | "application/x-msdos-program" => "exe",
        "application/x-debian-package" | "application/vnd.debian.binary-package" => "deb",
        "application/x-iso9660-image" => "iso",
        "image/jpeg" => "jpg",
        "image/png" => "png",
        "image/gif" => "gif",
        "image/webp" => "webp",
        "text/plain" => "txt",
        "text/html" => "html",
        "application/json" => "json",
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{BufRead, BufReader};
    use std::net::TcpListener;

    /// Each request an origin saw: its path, and the `Cookie:` it carried.
    type Seen = Arc<std::sync::Mutex<Vec<(String, Option<String>)>>>;

    /// An HTTP/1.1 origin serving a fixed route table and recording what it
    /// was asked for. A stream is hundreds of small objects, and the only way
    /// to show that a track was fetched is to watch an origin be asked for
    /// it. A route whose body is already a response (`HTTP/1.1 ...`) is sent
    /// as it is, which is how a test serves a redirect.
    fn serve(routes: Vec<(String, Vec<u8>)>) -> (String, Seen) {
        serve_on(TcpListener::bind(("127.0.0.1", 0)).expect("bind"), routes)
    }

    /// [`serve`] on a listener the caller bound, for routes that must name
    /// its port.
    fn serve_on(listener: TcpListener, routes: Vec<(String, Vec<u8>)>) -> (String, Seen) {
        let port = listener.local_addr().unwrap().port();
        let seen: Seen = Arc::default();
        let (log, routes) = (seen.clone(), Arc::new(routes));
        std::thread::spawn(move || {
            for conn in listener.incoming() {
                let Ok(mut sock) = conn else { continue };
                let (routes, log) = (routes.clone(), log.clone());
                // A thread per connection: serving inside the accept loop
                // would serialise a client that fetches segments in parallel.
                std::thread::spawn(move || {
                    let Ok(peek) = sock.try_clone() else { return };
                    let mut r = BufReader::new(peek);
                    let mut line = String::new();
                    if r.read_line(&mut line).unwrap_or(0) == 0 {
                        return;
                    }
                    let path = line.split_whitespace().nth(1).unwrap_or("/").to_string();
                    let mut cookie = None;
                    loop {
                        let mut h = String::new();
                        if r.read_line(&mut h).unwrap_or(0) == 0 || h == "\r\n" || h == "\n" {
                            break;
                        }
                        if let Some((name, value)) = h.split_once(':') {
                            if name.eq_ignore_ascii_case("cookie") {
                                cookie = Some(value.trim().to_string());
                            }
                        }
                    }
                    if let Ok(mut g) = log.lock() {
                        g.push((path.clone(), cookie));
                    }
                    let resp = match routes.iter().find(|(p, _)| *p == path) {
                        Some((_, b)) if b.starts_with(b"HTTP/1.1 ") => b.clone(),
                        Some((_, b)) => {
                            let mut out = format!(
                                "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                                b.len()
                            )
                            .into_bytes();
                            out.extend_from_slice(b);
                            out
                        }
                        None => b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n".to_vec(),
                    };
                    let _ = sock.write_all(&resp);
                    let _ = sock.flush();
                });
            }
        });
        (format!("http://127.0.0.1:{port}"), seen)
    }

    fn job_for(url: String, out: PathBuf) -> Job {
        Job {
            url,
            output: Some(out),
            container: "mp4".into(),
            user_agent: "hydra-test/1".into(),
            quiet: true,
            no_progress: true,
            ..Job::default()
        }
    }

    #[tokio::test]
    async fn hls_alternate_audio_is_downloaded_alongside_the_video() {
        // The shape X serves: the variant carries video only and the sound
        // is a rendition group it points at. Ignoring the group is how a
        // finished download ends up silent.
        let (base, seen) = serve(vec![
            (
                "/master.m3u8".into(),
                concat!(
                    "#EXTM3U\n",
                    "#EXT-X-MEDIA:TYPE=AUDIO,GROUP-ID=\"aac\",NAME=\"English\",",
                    "DEFAULT=YES,URI=\"a/en.m3u8\"\n",
                    "#EXT-X-STREAM-INF:BANDWIDTH=900000,RESOLUTION=640x360,AUDIO=\"aac\"\n",
                    "v/index.m3u8\n"
                )
                .into(),
            ),
            (
                "/v/index.m3u8".into(),
                "#EXTM3U\n#EXT-X-PLAYLIST-TYPE:VOD\n#EXTINF:4.0,\nv0.ts\n#EXT-X-ENDLIST\n".into(),
            ),
            (
                "/a/en.m3u8".into(),
                "#EXTM3U\n#EXT-X-PLAYLIST-TYPE:VOD\n#EXTINF:4.0,\na0.ts\n#EXT-X-ENDLIST\n".into(),
            ),
            ("/v/v0.ts".into(), b"VVVV".to_vec()),
            ("/a/a0.ts".into(), b"AAAA".to_vec()),
        ]);
        let dir = std::env::temp_dir().join("hydra-cli-hls-av");
        std::fs::create_dir_all(&dir).unwrap();
        let out = dir.join("out.mp4");
        let verdict = run(job_for(format!("{base}/master.m3u8"), out.clone())).await;

        let reqs = seen.lock().unwrap().clone();
        for want in ["/a/en.m3u8", "/a/a0.ts", "/v/v0.ts"] {
            assert!(
                reqs.iter().any(|(path, _)| path == want),
                "the audio rendition was skipped: {reqs:?}"
            );
        }
        // Combining is ffmpeg's step and these four-byte fixtures are not
        // media, so it may refuse them; what this pins is that both tracks
        // were fetched and handed to it.
        if let Verdict::Failed(msg) = &verdict {
            assert!(msg.contains("ffmpeg"), "unexpected failure: {msg}");
        }
        std::fs::remove_dir_all(&dir).ok();
    }

    #[tokio::test]
    async fn an_unreachable_audio_rendition_still_yields_the_video() {
        // Audio that will not resolve is not a reason to lose the download.
        let (base, _seen) = serve(vec![
            (
                "/master.m3u8".into(),
                concat!(
                    "#EXTM3U\n",
                    "#EXT-X-MEDIA:TYPE=AUDIO,GROUP-ID=\"aac\",NAME=\"English\",",
                    "DEFAULT=YES,URI=\"a/gone.m3u8\"\n",
                    "#EXT-X-STREAM-INF:BANDWIDTH=900000,RESOLUTION=640x360,AUDIO=\"aac\"\n",
                    "v/index.m3u8\n"
                )
                .into(),
            ),
            (
                "/v/index.m3u8".into(),
                "#EXTM3U\n#EXT-X-PLAYLIST-TYPE:VOD\n#EXTINF:4.0,\nv0.ts\n#EXT-X-ENDLIST\n".into(),
            ),
            ("/v/v0.ts".into(), b"VVVV".to_vec()),
        ]);
        let dir = std::env::temp_dir().join("hydra-cli-hls-noaudio");
        std::fs::create_dir_all(&dir).unwrap();
        let out = dir.join("out.ts");
        let job = Job {
            container: "ts".into(),
            ..job_for(format!("{base}/master.m3u8"), out.clone())
        };
        match run(job).await {
            Verdict::Done { path, .. } => {
                assert_eq!(std::fs::read(&path).unwrap(), b"VVVV");
            }
            other => panic!("a missing audio rendition should not fail the video: {other:?}"),
        }
        std::fs::remove_dir_all(&dir).ok();
    }

    fn cli(flags: &[&str], url: &str) -> crate::cli::Cli {
        use clap::Parser as _;
        let mut argv = vec!["hydra", "-q"];
        argv.extend_from_slice(flags);
        argv.push(url);
        crate::cli::Cli::parse_from(argv)
    }

    async fn jar_from_flags(flags: &[&str], url: &str) -> Arc<CookieJar> {
        open_jar(&cli(flags, url), url).await.unwrap()
    }

    fn cookies_sent(seen: &Seen, path: &str) -> Vec<Option<String>> {
        let reqs = seen.lock().unwrap();
        reqs.iter()
            .filter(|(p, _)| p == path)
            .map(|(_, c)| c.clone())
            .collect()
    }

    #[tokio::test]
    async fn a_jar_file_that_cannot_be_read_stops_the_stream_before_it_starts() {
        let url = "http://127.0.0.1:9/x.m3u8";
        let missing = std::env::temp_dir().join("hydra-no-such-jar/jar.txt");
        let flags = ["--load-cookies", missing.to_str().unwrap()];
        let e = open_jar(&cli(&flags, url), url).await.unwrap_err();
        assert!(e.contains("jar.txt"), "{e}");
    }

    #[tokio::test]
    async fn a_url_with_no_host_opens_an_empty_jar() {
        let jar = open_jar(&cli(&["-b", "a=1"], "x.m3u8"), "x.m3u8").await;
        assert!(jar.unwrap().is_empty());
    }

    #[tokio::test]
    async fn inspect_sends_the_cookie_flag_to_the_object_it_describes() {
        let (base, seen) = serve(vec![("/x.rar".into(), b"RAR!".to_vec())]);
        let url = format!("{base}/x.rar");
        let job = Job {
            jar: jar_from_flags(&["-b", "a=1"], &url).await,
            ..job_for(url.clone(), PathBuf::new())
        };
        inspect_file(&job).await.unwrap();

        let sent = cookies_sent(&seen, "/x.rar");
        assert!(!sent.is_empty(), "the object was never asked for");
        assert!(
            sent.iter().all(|c| c.as_deref() == Some("a=1")),
            "a probe went out without the cookie: {sent:?}"
        );
    }

    #[tokio::test]
    async fn inspect_without_a_cookie_flag_sends_no_cookie() {
        let (base, seen) = serve(vec![("/x.rar".into(), b"RAR!".to_vec())]);
        let job = job_for(format!("{base}/x.rar"), PathBuf::new());
        inspect_file(&job).await.unwrap();

        let sent = cookies_sent(&seen, "/x.rar");
        assert!(
            !sent.is_empty() && sent.iter().all(Option::is_none),
            "{sent:?}"
        );
    }

    #[tokio::test]
    async fn stream_requests_carry_the_cookies_their_own_host_is_owed() {
        // The manifest is published on one host and redirects to an edge on
        // another, where the segments live too. `-b` is scoped to the host the
        // user named, and a jar file's cookie to the host it names: each hop
        // must carry its own, and neither may leak to the other.
        let listener = TcpListener::bind(("127.0.0.1", 0)).expect("bind");
        let edge = format!("localhost:{}", listener.local_addr().unwrap().port());
        let (base, seen) = serve_on(
            listener,
            vec![
                (
                    "/master.m3u8".into(),
                    format!(
                        "HTTP/1.1 302 Found\r\nLocation: http://{edge}/m/index.m3u8\r\n\
                         Content-Length: 0\r\nConnection: close\r\n\r\n"
                    )
                    .into_bytes(),
                ),
                (
                    "/m/index.m3u8".into(),
                    "#EXTM3U\n#EXT-X-PLAYLIST-TYPE:VOD\n#EXTINF:4.0,\ns0.ts\n#EXT-X-ENDLIST\n"
                        .into(),
                ),
                ("/m/s0.ts".into(), b"SSSS".to_vec()),
            ],
        );
        let dir =
            std::env::temp_dir().join(format!("hydra-cli-hls-cookies-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let jar_file = dir.join("jar.txt");
        std::fs::write(&jar_file, "localhost\tFALSE\t/\tFALSE\t0\tedge\t2\n").unwrap();
        let url = format!("{base}/master.m3u8");
        let flags = ["-b", "a=1", "--load-cookies", jar_file.to_str().unwrap()];
        let job = Job {
            container: "ts".into(),
            jar: jar_from_flags(&flags, &url).await,
            ..job_for(url, dir.join("out.ts"))
        };

        let verdict = run(job).await;
        assert!(matches!(verdict, Verdict::Done { .. }), "{verdict:?}");
        assert_eq!(cookies_sent(&seen, "/master.m3u8"), [Some("a=1".into())]);
        for hop in ["/m/index.m3u8", "/m/s0.ts"] {
            let sent = cookies_sent(&seen, hop);
            assert!(!sent.is_empty(), "{hop} was never asked for");
            assert!(
                sent.iter().all(|c| c.as_deref() == Some("edge=2")),
                "{hop} carried the wrong cookies: {sent:?}"
            );
        }
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn a_generic_manifest_name_borrows_the_directory_above_it() {
        // Every stream on the internet is called index.m3u8; the asset id is
        // the directory, so that is what distinguishes the files.
        assert_eq!(
            output_name(
                "https://hls.example.net/live_cdn/nsqIStpj8PaG-Ev/emcQJ0pGpremocy/index.m3u8",
                "mp4"
            ),
            "emcQJ0pGpremocy.mp4"
        );
        assert_eq!(
            output_name("https://cdn.example/a/master.m3u8", "ts"),
            "a.ts"
        );
        assert_eq!(
            output_name("https://cdn.example/x/tracks-v1a1/mono.m3u8", "mp4"),
            "tracks-v1a1.mp4"
        );
        // A manifest with a real name keeps it.
        assert_eq!(
            output_name("https://cdn.example/a/bbb_30fps.mpd", "mp4"),
            "bbb_30fps.mp4"
        );
        // Query strings are not part of the name.
        assert_eq!(
            output_name("https://cdn.example/a/show.m3u8?token=abc", "mp4"),
            "show.mp4"
        );
        // A URL with no path names nothing; the host is not a filename.
        assert_eq!(output_name("https://cdn.example/", "mp4"), "stream.mp4");
        assert_eq!(output_name("https://cdn.example", "mp4"), "stream.mp4");
    }

    #[test]
    fn a_name_from_a_url_cannot_contain_path_separators() {
        // Decoded, `%2F` IS a separator and `%00` truncates the name where the OS
        // stops reading it; the sanitizer has to see both.
        assert_eq!(output_name("https://e/a/b%2Fc.m3u8", "mp4"), "b_c.mp4");
        assert_eq!(output_name("https://e/a/b%5Cc.m3u8", "mp4"), "b_c.mp4");
        assert_eq!(output_name("https://e/a/b%00c.m3u8", "mp4"), "b_c.mp4");
        assert_eq!(output_name("https://e/a/b:c*d.m3u8", "mp4"), "b_c_d.mp4");
    }

    #[test]
    fn a_stream_is_saved_under_the_name_a_reader_would_type() {
        assert_eq!(
            output_name("https://e/a/Big%20Buck%20Bunny.m3u8", "mp4"),
            "Big Buck Bunny.mp4"
        );
        // The generic-stem rule still reaches for the directory above, and that
        // name is decoded too.
        assert_eq!(
            output_name("https://e/My%20Show%20S01E02/index.m3u8", "mp4"),
            "My Show S01E02.mp4"
        );
    }

    #[test]
    fn a_mime_type_names_an_extension_when_the_url_has_none() {
        assert_eq!(extension_for("video/mp4"), Some("mp4"));
        // Parameters after the type are not part of it.
        assert_eq!(
            extension_for("video/mp4; codecs=\"avc1.42E01E\""),
            Some("mp4")
        );
        assert_eq!(extension_for("APPLICATION/PDF"), Some("pdf"));
        assert_eq!(extension_for("audio/x-m4a"), Some("m4a"));
        // Unknown types get no invented extension.
        assert_eq!(extension_for("application/octet-stream"), None);
        assert_eq!(extension_for(""), None);
    }

    #[test]
    fn a_filename_comes_from_the_path_not_the_query_or_the_host() {
        assert_eq!(file_name_of("https://e.com/a/b/setup.exe?t=1"), "setup.exe");
        assert_eq!(file_name_of("https://e.com/a/b/"), "b");
        // Nothing in the path at all still yields something usable.
        assert_eq!(file_name_of("https://e.com/"), "download");
        assert_eq!(file_name_of("https://e.com"), "download");
        // And the name a reader sees, not the encoded form the path carries.
        assert_eq!(file_name_of("https://e.com/My%20File.bin"), "My File.bin");
        // A string that is not a URL at all still has to answer with something
        // writable rather than panic on the caller's behalf.
        assert_eq!(file_name_of("gopher://e.com/f"), "download");
    }

    #[test]
    fn http_dates_are_shown_in_the_readers_own_zone() {
        use chrono::{Local, TimeZone, Utc};
        let out = to_local("Fri, 17 Oct 2025 10:23:20 GMT");
        // Same instant, expressed locally.
        let want = Utc
            .with_ymd_and_hms(2025, 10, 17, 10, 23, 20)
            .unwrap()
            .with_timezone(&Local)
            .format("%Y-%m-%d %H:%M:%S")
            .to_string();
        assert!(
            out.starts_with(&want),
            "got {out}, expected to start {want}"
        );
        assert!(!out.contains("GMT") || Local::now().offset().to_string() == "+00:00");

        // The obsolete forms RFC 9110 still requires a client to accept.
        assert!(to_local("Friday, 17-Oct-25 10:23:20 GMT").starts_with(&want));
        assert!(to_local("Fri Oct 17 10:23:20 2025").starts_with(&want));

        // Anything unparsable is passed through rather than guessed at: a
        // wrong local time would be worse than an honest unknown one.
        assert_eq!(to_local("not a date"), "not a date");
        assert_eq!(to_local(""), "");
    }

    #[test]
    fn sizes_read_the_way_people_write_them() {
        assert_eq!(human(0), "0.0 B");
        assert_eq!(human(1536), "1.5 KB");
        assert_eq!(human(5 * 1024 * 1024), "5.0 MB");
    }
}
