//! wget and curl command-line compatibility.
//!
//! # Why this is a layer and not just more flags
//!
//! wget and curl assign *incompatible* meanings to the same short flags. Of 19
//! commonly-used short options, 15 conflict:
//!
//! | flag | wget | curl |
//! |---|---|---|
//! | `-O` | output document to FILE | remote-name (no argument) |
//! | `-o` | log file | output file |
//! | `-c` | continue | cookie-jar |
//! | `-q` | quiet | disable `.curlrc` |
//! | `-H` | span-hosts | custom header |
//! | `-U` | user-agent | proxy-user |
//! | `-t` | tries | telnet-option |
//! | `-T` | timeout | upload-file |
//! | `-r` | recursive | byte range |
//! | `-A` | accept list | user-agent |
//! | `-L` | relative-only | follow redirects |
//! | `-i` | input file | include headers |
//! | `-N` | timestamping | no-buffer |
//! | `-P` | directory-prefix | ftp-port |
//! | `-x` | force-directories | proxy |
//!
//! `-O out.bin` therefore cannot mean one thing. A single namespace claiming to
//! be "wget and curl compatible" would silently do the wrong thing for half its
//! users, which is worse than not claiming compatibility at all.
//!
//! # The solution: personalities
//!
//! The active dialect is chosen by, in order of precedence:
//!
//! 1. `--compat=wget|curl|native` on the command line;
//! 2. the name the binary was invoked as — symlink or copy `hydra` to `wget` or
//!    `curl` (or `hydra-wget` / `hydra-curl`) and it adopts that dialect, so
//!    existing scripts work unchanged;
//! 3. otherwise native, which is a superset using unambiguous long options and
//!    the short flags both tools agree on.
//!
//! This module rewrites `argv` into canonical native long-form *before* the
//! argument parser sees it, so there is exactly one parser and one set of
//! semantics downstream.
//!
//! # Unsupported flags are errors, not no-ops
//!
//! A silently-ignored `--limit-rate` can saturate a metered link; a
//! silently-ignored `--post-data` sends a request the user did not ask for. Every
//! recognised flag is either honoured, translated, or **rejected with a message
//! naming the reason**. Only flags that are genuinely inert for a downloader
//! (`--no-dns-cache`, `--tcp-nodelay`) are accepted and dropped, and they are
//! listed explicitly rather than matched by a catch-all.

use std::collections::HashSet;

#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum Personality {
    #[default]
    Native,
    Wget,
    Curl,
}

impl Personality {
    pub fn name(self) -> &'static str {
        match self {
            Personality::Native => "native",
            Personality::Wget => "wget",
            Personality::Curl => "curl",
        }
    }
}

/// Decide the dialect from the invoked name and the arguments.
///
/// A `--compat` value that names no dialect is an error: silently falling back
/// to native would parse `-O` with the wrong meaning and look like it worked.
pub fn detect(argv0: &str, args: &[String]) -> Result<Personality, String> {
    // An explicit --compat wins over everything.
    for (i, a) in args.iter().enumerate() {
        if let Some(v) = a.strip_prefix("--compat=") {
            return parse_name(v);
        }
        if a == "--compat" {
            if let Some(v) = args.get(i + 1) {
                return parse_name(v);
            }
        }
    }
    Ok(detect_from_name(argv0))
}

fn detect_from_name(argv0: &str) -> Personality {
    let stem = std::path::Path::new(argv0)
        .file_stem()
        .map(|s| s.to_string_lossy().to_ascii_lowercase())
        .unwrap_or_default();
    // Accept `wget`, `hydra-wget`, `wget.exe`, and the same for curl.
    if stem == "wget" || stem.ends_with("-wget") || stem.ends_with("_wget") {
        Personality::Wget
    } else if stem == "curl" || stem.ends_with("-curl") || stem.ends_with("_curl") {
        Personality::Curl
    } else {
        Personality::Native
    }
}

fn parse_name(v: &str) -> Result<Personality, String> {
    match v.to_ascii_lowercase().as_str() {
        "wget" => Ok(Personality::Wget),
        "curl" => Ok(Personality::Curl),
        "native" | "hydra" => Ok(Personality::Native),
        other => Err(format!(
            "--compat {other:?}: not a dialect (want native, wget or curl)"
        )),
    }
}

/// Flags after which curl writes to a file rather than to stdout.
const CURL_OUTPUT_FLAGS: &[&str] = &[
    "--output",
    "--stdout",
    "--remote-name",
    "--content-disposition",
    "--spider",
    "--json",
    "--inspect",
    "--list-streams",
    "--preview",
    "--help",
    "--version",
];

/// A translation outcome for one input token.
enum Map {
    /// Emit these canonical tokens.
    Emit(Vec<String>),
    /// Accepted for compatibility but has no effect here; explain at -v.
    Inert(&'static str),
    /// Refuse, with a reason the user can act on.
    Reject(String),
}

/// Flags that take a value in wget's dialect (short forms).
const WGET_SHORT_WITH_VALUE: &[char] = &[
    'O', 'o', 'a', 'e', 't', 'T', 'w', 'Q', 'P', 'l', 'A', 'R', 'D', 'I', 'X', 'B', 'U', 'i',
];
/// Flags that take a value in curl's dialect (short forms).
const CURL_SHORT_WITH_VALUE: &[char] = &[
    'o', 'H', 'd', 'F', 'u', 'U', 'A', 'e', 'b', 'c', 'C', 'r', 'X', 'y', 'Y', 'z', 'K', 'E', 'T',
    't', 'w', 'm', 'D', 'Q', 'P', 'x',
];

/// Rewrite `args` from `p`'s dialect into canonical native long options.
///
/// Returns the canonical argv (excluding argv0) plus notes to print at `-v`.
pub fn canonicalize(p: Personality, args: &[String]) -> Result<(Vec<String>, Vec<String>), String> {
    let mut out: Vec<String> = Vec::new();
    let mut notes: Vec<String> = Vec::new();
    let mut it = args.iter().peekable();
    let mut seen_dashdash = false;
    // Canonical boolean flags already emitted, so a repeat can be dropped. See
    // [`push_canonical`].
    let mut emitted_bools: HashSet<String> = HashSet::new();

    while let Some(raw) = it.next() {
        if seen_dashdash {
            out.push(raw.clone());
            continue;
        }
        if raw == "--" {
            seen_dashdash = true;
            out.push(raw.clone());
            continue;
        }
        // --compat itself is consumed here; it has already been read by detect().
        if raw == "--compat" {
            let _ = it.next();
            continue;
        }
        if raw.starts_with("--compat=") {
            continue;
        }

        // In native mode the canonical parser owns the namespace: pass everything
        // through untouched. Translating here would mean every new native flag
        // also needed an entry in the table below, and forgetting one would make
        // the flag vanish with a confusing "not recognised" from this layer rather
        // than from the parser that actually defines the options.
        if p == Personality::Native {
            out.push(raw.clone());
            continue;
        }

        // Long option, possibly --name=value.
        if let Some(body) = raw.strip_prefix("--") {
            let (name, inline) = match body.split_once('=') {
                Some((n, v)) => (n, Some(v.to_string())),
                None => (body, None),
            };
            let mut take_value = |need: bool| -> Option<String> {
                if !need {
                    return None;
                }
                match inline.clone() {
                    Some(v) => Some(v),
                    None => it.next().cloned(),
                }
            };
            match map_long(p, name, &mut take_value) {
                Map::Emit(v) => push_canonical(&mut out, &mut emitted_bools, v),
                Map::Inert(why) => notes.push(format!("--{name}: accepted, no effect ({why})")),
                Map::Reject(msg) => return Err(msg),
            }
            continue;
        }

        // wget has multi-character "short" options that are NOT clusters: -nv,
        // -nc, -nd, -nH, -np. Treating them as clusters would read -nc as
        // "-n -c" and silently enable resume, so they are matched first.
        if p == Personality::Wget {
            match raw.as_str() {
                "-nv" => {
                    out.push("--no-verbose".into());
                    continue;
                }
                "-nc" => {
                    out.push("--no-clobber".into());
                    continue;
                }
                "-nd" | "-nH" => {
                    notes.push(format!(
                        "{raw}: accepted, no effect (no directory tree is created)"
                    ));
                    continue;
                }
                "-np" => {
                    return Err("-np: hydra downloads named objects and does not crawl".to_string())
                }
                _ => {}
            }
        }

        // Short option cluster, e.g. -qO- or -x4.
        if raw.len() > 1 && raw.starts_with('-') {
            let chars: Vec<char> = raw.chars().skip(1).collect();
            let mut idx = 0usize;
            while idx < chars.len() {
                let ch = chars[idx];
                let with_value = match p {
                    Personality::Wget => WGET_SHORT_WITH_VALUE.contains(&ch),
                    Personality::Curl => CURL_SHORT_WITH_VALUE.contains(&ch),
                    // Native keeps its own set; handled by the parser directly.
                    Personality::Native => false,
                };
                // A value may be glued to the flag (-x4) or be the next argv item.
                let glued: Option<String> = if with_value && idx + 1 < chars.len() {
                    Some(chars[idx + 1..].iter().collect())
                } else {
                    None
                };
                let mut fetched = false;
                let mut take_value = |need: bool| -> Option<String> {
                    if !need {
                        return None;
                    }
                    fetched = true;
                    match glued.clone() {
                        Some(g) => Some(g),
                        None => it.next().cloned(),
                    }
                };
                match map_short(p, ch, &mut take_value) {
                    Map::Emit(v) => push_canonical(&mut out, &mut emitted_bools, v),
                    Map::Inert(why) => notes.push(format!("-{ch}: accepted, no effect ({why})")),
                    Map::Reject(msg) => return Err(msg),
                }
                if fetched && glued.is_some() {
                    break; // the rest of the cluster was the value
                }
                idx += 1;
            }
            continue;
        }

        out.push(raw.clone());
    }
    // curl writes the body to stdout unless told where to put it; a `curl URL`
    // that quietly wrote a file would surprise every pipeline built on it.
    if p == Personality::Curl
        && !seen_dashdash_output(&out)
        && !out.iter().any(|t| CURL_OUTPUT_FLAGS.contains(&t.as_str()))
        && out.iter().any(|t| !t.starts_with('-'))
    {
        out.push("--stdout".into());
    }
    Ok((out, notes))
}

/// A subcommand word (`checksum`, `formats`, ...) is not a URL to stream.
fn seen_dashdash_output(out: &[String]) -> bool {
    out.iter().any(|t| {
        matches!(
            t.as_str(),
            "interactive"
                | "parity"
                | "checksum"
                | "metalink"
                | "formats"
                | "bench"
                | "completions"
                | "update"
                | "compat-link"
                | "install-completions"
        )
    })
}

/// Append canonical tokens, dropping a boolean flag already emitted.
///
/// wget and curl both accept a repeated flag — `curl -s -s`, `wget -q -q` — and
/// so does anything that wraps them by prepending its own flags to a command the
/// user already wrote. clap rejects a repeated `SetTrue` argument outright
/// ("the argument '--quiet' cannot be used multiple times", exit 2), so without
/// this a faithful dialect would fail on input the real tool runs. Under a
/// dialect the source tool's behaviour wins, which here means the second `-s` is
/// redundant rather than an error.
///
/// Only a lone `--flag` is deduplicated. A flag with a value is left alone
/// because repeating it is meaningful (`--header` accumulates) or because the
/// parser's own last-wins rule should decide, and `--verbose` is exempt because
/// it counts occurrences: `-vv` is a level, not a repeat.
fn push_canonical(out: &mut Vec<String>, seen: &mut HashSet<String>, tokens: Vec<String>) {
    if tokens.len() == 1 {
        let tok = &tokens[0];
        if tok.starts_with("--") && tok != "--verbose" && !seen.insert(tok.clone()) {
            return;
        }
    }
    out.extend(tokens);
}

/// `--canonical <value>`: emit the canonical long flag and the value taken
/// from the dialect argv. Shared by [`map_long`] and [`map_short`]; the `val!`
/// it expands is each function's own, so the error message keeps the spelling
/// (`--flag` vs `-f`) the user actually typed.
macro_rules! kv {
    ($canon:expr, $f:expr) => {
        Map::Emit(vec![format!("--{}", $canon), val!($f)])
    };
}
/// `--canonical` with no value.
macro_rules! bare {
    ($canon:expr) => {
        Map::Emit(vec![format!("--{}", $canon)])
    };
}

/// wget's `-t 0` means retry forever and curl's `--retry 0` means never;
/// the native parser takes a count of at least one.
fn tries_value(p: Personality, v: String) -> String {
    match (p, v.trim()) {
        (Personality::Wget, "0" | "inf") => u32::MAX.to_string(),
        (Personality::Curl, "0") => "1".to_string(),
        _ => v,
    }
}

/// Long options, shared where the two tools agree.
fn map_long(p: Personality, name: &str, take: &mut dyn FnMut(bool) -> Option<String>) -> Map {
    let need = |t: &mut dyn FnMut(bool) -> Option<String>, flag: &str| -> Result<String, Map> {
        t(true).ok_or_else(|| Map::Reject(format!("--{flag} requires a value")))
    };
    macro_rules! val {
        ($f:expr) => {
            match need(take, $f) {
                Ok(v) => v,
                Err(m) => return m,
            }
        };
    }

    match name {
        // ---- agreed between both tools -------------------------------------
        "limit-rate" => kv!("limit-rate", "limit-rate"),
        "header" => kv!("header", "header"),
        "user-agent" => kv!("user-agent", "user-agent"),
        "output" => {
            let v = val!("output");
            if v == "-" {
                bare!("stdout")
            } else {
                Map::Emit(vec!["--output".into(), v])
            }
        }
        "continue" => bare!("continue"),
        "quiet" => bare!("quiet"),
        "verbose" => bare!("verbose"),
        "version" => bare!("version"),
        "help" => bare!("help"),
        "referer" | "referrer" => Map::Emit(vec![
            "--header".into(),
            format!("Referer: {}", val!("referer")),
        ]),
        "no-clobber" => bare!("no-clobber"),
        "no-verbose" => bare!("no-verbose"),
        "create-dirs" => bare!("create-dirs"),
        "output-dir" | "directory-prefix" => kv!("output-dir", name),
        "max-redirect" | "max-redirs" => kv!("max-redirs", name),
        "max-filesize" => kv!("max-filesize", "max-filesize"),
        "insecure" | "no-check-certificate" => bare!("insecure"),
        "location" => bare!("location"),
        "remote-name" => bare!("remote-name"),
        "remote-time" | "use-server-timestamps" => bare!("remote-time"),
        "checksum" => kv!("checksum", "checksum"),
        // curl's `--json DATA` is a POST; wget has no `--json`, so there it is
        // hydra's own machine-readable output.
        "json" if p == Personality::Curl => {
            let _ = take(true);
            Map::Reject(
                "--json: in curl this sends a JSON request body, and hydra only issues GET \
                 and HEAD. For hydra's machine-readable result use --compat native --json."
                    .into(),
            )
        }
        "json" => bare!("json"),
        "silent" => bare!("quiet"),
        "no-progress-meter" | "no-progress" => bare!("no-progress"),
        "show-progress" => bare!("show-progress"),
        "spider" | "head" => bare!("spider"),
        "range" => kv!("range", "range"),
        "continue-at" => {
            let v = val!("continue-at");
            if v == "-" {
                bare!("continue")
            } else {
                Map::Emit(vec!["--continue".into(), "--start-pos".into(), v])
            }
        }
        "start-pos" => kv!("start-pos", "start-pos"),
        "proxy" => kv!("proxy", "proxy"),
        // curl's `--noproxy HOSTS` takes a list; only "*" maps onto a flag that
        // disables proxying altogether.
        "noproxy" => {
            let v = val!("noproxy");
            if v.trim() == "*" {
                bare!("no-proxy")
            } else {
                Map::Reject(format!(
                    "--noproxy {v:?}: a per-host exemption list is read from the no_proxy \
                     environment variable; --noproxy '*' disables the proxy for every host"
                ))
            }
        }
        "no-proxy" => bare!("no-proxy"),
        "ipv4" | "inet4-only" => bare!("ipv4"),
        "ipv6" | "inet6-only" => bare!("ipv6"),
        "compat" => Map::Emit(vec![]),
        "tries" | "retry" => Map::Emit(vec!["--tries".into(), tries_value(p, val!(name))]),
        "retry-delay" | "waitretry" => {
            let _ = take(true);
            Map::Reject(format!(
                "--{name}: the pause between retries is fixed in this client and cannot be set"
            ))
        }
        "wait" => kv!("wait", "wait"),
        "timeout" | "max-time" => kv!("timeout", name),
        "connect-timeout" => kv!("connect-timeout", "connect-timeout"),
        "read-timeout" => kv!("timeout", "read-timeout"),
        "input-file" => kv!("input-file", "input-file"),
        "etag-compare" => kv!("etag-compare", "etag-compare"),
        "etag-save" => kv!("etag-save", "etag-save"),
        "fail" | "fail-with-body" | "fail-early" => bare!("fail"),
        "show-error" => bare!("show-error"),
        "parallel" => bare!("parallel"),
        "parallel-max" => kv!("parallel-max", "parallel-max"),
        "split" | "max-connection-per-server" => kv!("max-connection-per-server", name),

        // ---- accepted, genuinely inert for this client ----------------------
        "no-dns-cache" | "dns-timeout" | "tcp-nodelay" | "tcp-fastopen" | "no-keepalive"
        | "keepalive-time" | "no-http-keep-alive" | "no-buffer" | "no-sessionid" | "no-alpn"
        | "no-npn" | "styled-output" | "no-iri" | "trust-server-names" | "no-hsts" | "no-netrc"
        | "netrc-optional" | "path-as-is" | "globoff" | "disable" => {
            // Consume a value for the ones that carry one.
            if matches!(name, "dns-timeout" | "keepalive-time") {
                let _ = take(true);
            }
            Map::Inert("this client does not expose the corresponding behaviour")
        }
        "progress" | "report-speed" => {
            let _ = take(true);
            Map::Inert("progress style is fixed; use --no-progress to disable")
        }

        // ---- refused, with the reason --------------------------------------
        "recursive"
        | "mirror"
        | "page-requisites"
        | "convert-links"
        | "level"
        | "accept"
        | "reject"
        | "accept-regex"
        | "reject-regex"
        | "domains"
        | "exclude-domains"
        | "include-directories"
        | "exclude-directories"
        | "no-parent"
        | "span-hosts"
        | "follow-tags"
        | "ignore-tags"
        | "force-html"
        | "base" => Map::Reject(format!(
            "--{name}: hydra downloads named objects and does not crawl. \
             Use wget -r for recursive mirroring, then hydra for the large files."
        )),
        "request" | "method" => match val!(name).to_ascii_uppercase().as_str() {
            "GET" => Map::Emit(vec![]),
            "HEAD" => bare!("spider"),
            other => Map::Reject(format!(
                "--{name} {other}: hydra is a downloader; it only issues GET and HEAD."
            )),
        },
        "post-data" | "post-file" | "body-data" | "body-file" | "data" | "data-ascii"
        | "data-binary" | "data-raw" | "data-urlencode" | "form" | "form-string"
        | "upload-file" | "get" => Map::Reject(format!(
            "--{name}: hydra is a downloader; it only issues GET and HEAD. \
             Use curl for request bodies and other methods."
        )),
        // Cookies: the six flags are native, and both dialects spell them the
        // same way. curl's `--cookie-jar` is write-only where hydra's also
        // reads; that is a widening, so a script written against curl still
        // gets the file it asked for.
        "cookie" => kv!("cookie", "cookie"),
        "cookie-jar" => kv!("cookie-jar", "cookie-jar"),
        "load-cookies" => kv!("load-cookies", "load-cookies"),
        "save-cookies" => kv!("save-cookies", "save-cookies"),
        "keep-session-cookies" => bare!("keep-session-cookies"),
        "junk-session-cookies" => bare!("junk-session-cookies"),
        // HTTP Basic is the one scheme both tools share a spelling for.
        "user" | "http-user" => kv!("user", name),
        "password" | "http-password" => kv!("password", name),
        "basic" => Map::Emit(vec![]),
        "ftp-user" | "ftp-password" | "proxy-user" | "proxy-password" | "ask-password"
        | "netrc" | "digest" | "ntlm" | "negotiate" | "anyauth" | "oauth2-bearer" | "aws-sigv4" => {
            let _ = take(!matches!(
                name,
                "ask-password" | "netrc" | "digest" | "ntlm" | "negotiate" | "anyauth"
            ));
            Map::Reject(format!(
                "--{name}: only HTTP Basic is implemented (--user, --password, or \
                 user:pass@ in the URL); use --header 'Authorization: ...' for anything else, \
                 and --proxy http://user:pass@host:port for a proxy login"
            ))
        }
        "compressed" | "compression" | "tr-encoding" => Map::Reject(format!(
            "--{name}: content encoding is refused deliberately. A compressed body \
             cannot be assembled from byte ranges fetched in parallel, because the \
             ranges are of the COMPRESSED stream and the client cannot know where \
             they fall in the decoded object."
        )),
        "warc-file" | "warc-header" | "warc-max-size" | "warc-cdx" | "warc-dedup" => {
            let _ = take(true);
            Map::Reject(format!("--{name}: WARC archiving is not implemented."))
        }
        other => Map::Reject(format!(
            "--{other}: not recognised in {} mode (try --help)",
            p.name()
        )),
    }
}

/// Short options, which is where the dialects diverge.
fn map_short(p: Personality, ch: char, take: &mut dyn FnMut(bool) -> Option<String>) -> Map {
    macro_rules! val {
        ($f:expr) => {
            match take(true) {
                Some(v) => v,
                None => return Map::Reject(format!("-{} requires a value", $f)),
            }
        };
    }

    // Flags both tools agree on.
    match ch {
        'v' => return bare!("verbose"),
        'V' => return bare!("version"),
        'h' => return bare!("help"),
        '4' => return bare!("ipv4"),
        '6' => return bare!("ipv6"),
        _ => {}
    }

    match p {
        Personality::Wget => match ch {
            'O' => {
                let v = val!('O');
                // wget -O- means stdout.
                if v == "-" {
                    bare!("stdout")
                } else {
                    Map::Emit(vec!["--output".into(), v])
                }
            }
            'o' => kv!("logfile", 'o'),
            'a' => kv!("logfile-append", 'a'),
            'c' => bare!("continue"),
            'q' => bare!("quiet"),
            't' => Map::Emit(vec!["--tries".into(), tries_value(p, val!('t'))]),
            'T' => kv!("timeout", 'T'),
            'w' => kv!("wait", 'w'),
            'U' => kv!("user-agent", 'U'),
            'P' => kv!("output-dir", 'P'),
            'N' => bare!("remote-time"),
            'S' => bare!("server-response"),
            'd' => Map::Emit(vec!["--verbose".into(), "--verbose".into()]),
            'e' => {
                let _ = val!('e');
                Map::Inert("wgetrc commands are not interpreted")
            }
            'Q' => {
                let _ = val!('Q');
                Map::Reject("-Q (quota) is not implemented".into())
            }
            'b' => Map::Reject(
                "-b (background) is not implemented; use your shell's job control".into(),
            ),
            'i' => kv!("input-file", 'i'),
            'r' | 'm' | 'p' | 'k' | 'K' | 'l' | 'A' | 'R' | 'D' | 'I' | 'X' | 'H' | 'L' | 'F'
            | 'B' | 'E' => Map::Reject(format!(
                "-{ch}: hydra downloads named objects and does not crawl or rewrite HTML"
            )),
            'x' => Map::Inert("directory forcing is not applicable"),
            other => Map::Reject(format!("-{other}: not a wget option hydra recognises")),
        },
        Personality::Curl => match ch {
            'o' => {
                let v = val!('o');
                if v == "-" {
                    bare!("stdout")
                } else {
                    Map::Emit(vec!["--output".into(), v])
                }
            }
            'O' => bare!("remote-name"),
            'C' => {
                let v = val!('C');
                if v == "-" {
                    bare!("continue")
                } else {
                    Map::Emit(vec!["--continue".into(), "--start-pos".into(), v])
                }
            }
            'H' => kv!("header", 'H'),
            'A' => kv!("user-agent", 'A'),
            'e' => Map::Emit(vec!["--header".into(), format!("Referer: {}", val!('e'))]),
            's' => bare!("quiet"),
            'S' => bare!("show-error"),
            'f' => bare!("fail"),
            'L' => bare!("location"),
            'k' => bare!("insecure"),
            'I' => bare!("spider"),
            'J' => bare!("content-disposition"),
            'R' => bare!("remote-time"),
            'Z' => bare!("parallel"),
            'r' => kv!("range", 'r'),
            'm' => kv!("timeout", 'm'),
            'x' => kv!("proxy", 'x'),
            'w' => {
                let _ = val!('w');
                Map::Reject("-w/--write-out is not implemented; use --json".into())
            }
            '#' => bare!("show-progress"),
            'X' => match val!('X').to_ascii_uppercase().as_str() {
                "GET" => Map::Emit(vec![]),
                "HEAD" => bare!("spider"),
                other => Map::Reject(format!(
                    "-X {other}: hydra is a downloader; it only issues GET and HEAD"
                )),
            },
            'd' | 'F' | 'T' | 'G' => Map::Reject(format!(
                "-{ch}: hydra is a downloader; it only issues GET and HEAD"
            )),
            'u' => kv!("user", 'u'),
            'U' | 'n' | 'E' => {
                let _ = take(matches!(ch, 'U' | 'E'));
                Map::Reject(format!(
                    "-{ch}: only HTTP Basic is implemented (-u user:pass); use --proxy \
                     http://user:pass@host:port for a proxy login"
                ))
            }
            'b' => kv!("cookie", 'b'),
            'c' => kv!("cookie-jar", 'c'),
            'j' => bare!("junk-session-cookies"),
            'q' => Map::Inert("there is no .curlrc to disable"),
            'N' => Map::Inert("output is not buffered in a way this would change"),
            other => Map::Reject(format!("-{other}: not a curl option hydra recognises")),
        },
        // Native short flags are the parser's own; pass through untouched.
        Personality::Native => Map::Emit(vec![format!("-{ch}")]),
    }
}

/// Short flags whose meaning is identical in wget, curl, and native mode.
///
/// Documented so the help text can state the safe subset, and asserted by a test
/// so the claim cannot drift away from `map_short`.
#[cfg(test)]
pub fn universally_safe_shorts() -> HashSet<char> {
    ['v', 'V', 'h', '4', '6'].into_iter().collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    // Needed by the dialect-coverage tests below, which assert that translated
    // arguments still go through the real argument parser.
    use clap::Parser as _;

    fn s(v: &[&str]) -> Vec<String> {
        v.iter().map(|x| x.to_string()).collect()
    }
    fn canon(p: Personality, args: &[&str]) -> Vec<String> {
        canonicalize(p, &s(args)).expect("should canonicalize").0
    }

    #[test]
    fn personality_comes_from_the_invoked_name() {
        assert_eq!(detect("/usr/local/bin/hydra", &[]), Ok(Personality::Native));
        assert_eq!(detect("/usr/local/bin/wget", &[]), Ok(Personality::Wget));
        assert_eq!(detect("hydra-wget", &[]), Ok(Personality::Wget));
        assert_eq!(detect("/opt/bin/curl", &[]), Ok(Personality::Curl));
        assert_eq!(detect("hydra-curl.exe", &[]), Ok(Personality::Curl));
    }

    /// A repeated boolean flag is what the real tools do, so it cannot be an
    /// error here. This came from a wrapper that prepends `-s` to a command the
    /// user already wrote `-sS` in: clap rejected the second `--quiet` and the
    /// download never started, which reads as "the curl dialect is broken".
    #[test]
    fn a_repeated_boolean_flag_is_accepted_not_rejected() {
        let (out, _) = canonicalize(Personality::Curl, &s(&["-s", "-sS", "http://x/f"])).unwrap();
        assert_eq!(
            out.iter().filter(|t| *t == "--quiet").count(),
            1,
            "three -s occurrences must collapse to one --quiet: {out:?}"
        );
        assert!(out.contains(&"--show-error".to_string()));
        assert!(out.contains(&"http://x/f".to_string()));

        let (w, _) = canonicalize(Personality::Wget, &s(&["-q", "-q", "-c", "-c"])).unwrap();
        assert_eq!(w.iter().filter(|t| *t == "--quiet").count(), 1);
        assert_eq!(w.iter().filter(|t| *t == "--continue").count(), 1);
    }

    /// `--verbose` counts occurrences, so collapsing it would silently downgrade
    /// `-vv` to `-v`. Flags that carry a value accumulate and must not collapse
    /// either.
    #[test]
    fn counted_and_valued_flags_are_not_collapsed() {
        let (out, _) = canonicalize(Personality::Curl, &s(&["-v", "-v", "http://x/f"])).unwrap();
        assert_eq!(out.iter().filter(|t| *t == "--verbose").count(), 2);

        let (h, _) = canonicalize(
            Personality::Curl,
            &s(&["-H", "A: 1", "-H", "B: 2", "http://x/f"]),
        )
        .unwrap();
        assert_eq!(h.iter().filter(|t| *t == "--header").count(), 2);
    }

    #[test]
    fn explicit_compat_overrides_the_name() {
        assert_eq!(
            detect("wget", &s(&["--compat=curl"])),
            Ok(Personality::Curl)
        );
        assert_eq!(
            detect("curl", &s(&["--compat", "wget"])),
            Ok(Personality::Wget)
        );
        assert_eq!(
            detect("wget", &s(&["--compat=native"])),
            Ok(Personality::Native)
        );
        // A typo must not silently become native, where -O means something else.
        let e = detect("hydra", &s(&["--compat=culr"])).unwrap_err();
        assert!(e.contains("culr"), "{e}");
    }

    /// The headline conflict: `-O` must mean opposite things per dialect.
    #[test]
    fn dash_o_means_opposite_things_in_each_dialect() {
        // wget: -O takes a filename.
        assert_eq!(
            canon(Personality::Wget, &["-O", "out.bin", "http://x/f"]),
            s(&["--output", "out.bin", "http://x/f"])
        );
        // curl: -O takes NO argument and means "use the remote name"; -o takes the file.
        assert_eq!(
            canon(Personality::Curl, &["-O", "http://x/f"]),
            s(&["--remote-name", "http://x/f"])
        );
        assert_eq!(
            canon(Personality::Curl, &["-o", "out.bin", "http://x/f"]),
            s(&["--output", "out.bin", "http://x/f"])
        );
        // And with neither, curl writes the body to stdout.
        assert_eq!(
            canon(Personality::Curl, &["-s", "http://x/f"]),
            s(&["--quiet", "http://x/f", "--stdout"])
        );
        assert!(
            !canon(Personality::Curl, &["-I", "http://x/f"]).contains(&"--stdout".to_string()),
            "a HEAD has no body to stream"
        );
        assert!(
            !canon(Personality::Curl, &["checksum", "http://x/f"])
                .contains(&"--stdout".to_string()),
            "a subcommand is not a transfer"
        );
    }

    #[test]
    fn continue_differs_between_dialects() {
        assert_eq!(
            canon(Personality::Wget, &["-c", "http://x/f"]),
            s(&["--continue", "http://x/f"])
        );
        // curl -C - is "resume where it left off"; -C 1024 is an explicit offset.
        assert_eq!(
            canon(Personality::Curl, &["-C", "-", "http://x/f"]),
            s(&["--continue", "http://x/f", "--stdout"])
        );
        assert_eq!(
            canon(Personality::Curl, &["-C", "1024", "http://x/f"]),
            s(&[
                "--continue",
                "--start-pos",
                "1024",
                "http://x/f",
                "--stdout"
            ])
        );
    }

    #[test]
    fn quiet_and_silent_reach_the_same_canonical_flag() {
        assert_eq!(
            canon(Personality::Wget, &["-q", "http://x/f"])[0],
            "--quiet"
        );
        assert_eq!(
            canon(Personality::Curl, &["-s", "http://x/f"])[0],
            "--quiet"
        );
    }

    #[test]
    fn user_agent_is_dash_u_in_wget_and_dash_a_in_curl() {
        assert_eq!(
            canon(Personality::Wget, &["-U", "me/1.0", "http://x/f"]),
            s(&["--user-agent", "me/1.0", "http://x/f"])
        );
        assert_eq!(
            canon(Personality::Curl, &["-A", "me/1.0", "http://x/f"]),
            s(&["--user-agent", "me/1.0", "http://x/f", "--stdout"])
        );
        // And curl's -U is proxy-user, which must NOT become a user agent.
        let e = canonicalize(Personality::Curl, &s(&["-U", "bob:pw", "http://x/f"])).unwrap_err();
        assert!(e.contains("Basic"), "curl -U must not be read as a UA: {e}");
    }

    /// Every dialect funnels headers through the one validating parser.
    ///
    /// `canonicalize` renames a dialect's header flag to a canonical `--header`
    /// and hands the result to `Cli::try_parse_from`, so that parser is the single
    /// choke point for every personality. This test exists to keep it that way: a
    /// future translation that constructed a header without going through it could
    /// put a colon-less field line back on the wire under a compat personality
    /// while native mode stayed clean — which is the original hang.
    ///
    /// Two properties, one per shape: a bare name is normalized identically in
    /// every dialect, and an injection attempt is refused in every dialect.
    ///
    /// The spelling differs by dialect and that is the point of the compat layer:
    /// `-H` is a header in native and curl mode, but in wget it means span-hosts
    /// (a crawl flag), so a wget user writes `--header`.
    #[test]
    fn every_dialect_normalizes_and_refuses_identically() {
        for (dialect, flag) in [
            (Personality::Native, "-H"),
            (Personality::Curl, "-H"),
            (Personality::Wget, "--header"),
        ] {
            // A bare name is a QUERY in every dialect, and is never sent.
            if let Ok((canon, _)) = canonicalize(dialect, &s(&[flag, "Age", "http://x/f"])) {
                let mut full = vec!["hydra".to_string()];
                full.extend(canon);
                let parsed = crate::cli::Cli::parse_with_queries(full)
                    .unwrap_or_else(|e| panic!("bare name under {dialect:?} must parse: {e}"));
                assert_eq!(
                    parsed.header_queries,
                    vec!["Age".to_string()],
                    "a bare name must be a response-header query under {dialect:?}"
                );
                assert!(
                    parsed.headers.is_empty(),
                    "a query must not be sent as a request header under {dialect:?}"
                );
            }

            // Injection is still refused everywhere.
            if let Ok((canon, _)) =
                canonicalize(dialect, &s(&[flag, "X-A: 1\r\nX-Evil: 2", "http://x/f"]))
            {
                let mut full = vec!["hydra".to_string()];
                full.extend(canon);
                assert!(
                    crate::cli::Cli::parse_with_queries(full).is_err(),
                    "CRLF injection must be refused under {dialect:?}"
                );
            }
        }
    }

    /// The referer translation builds a header value itself, so it must build a
    /// well-formed one.
    #[test]
    fn the_referer_translation_produces_a_parseable_header() {
        let canon = canonicalize(Personality::Curl, &s(&["-e", "http://ref/", "http://x/f"]))
            .expect("referer translates");
        let mut full = vec!["hydra".to_string()];
        full.extend(canon.0);
        let parsed = crate::cli::Cli::try_parse_from(&full)
            .expect("a translated Referer must satisfy the header grammar");
        assert_eq!(parsed.headers, vec!["Referer: http://ref/".to_string()]);
    }

    #[test]
    fn curl_and_wget_credentials_reach_the_basic_auth_flags() {
        assert_eq!(
            canon(Personality::Curl, &["-u", "a:b", "-o", "f", "http://x/f"]),
            s(&["--user", "a:b", "--output", "f", "http://x/f"])
        );
        assert_eq!(
            canon(
                Personality::Wget,
                &["--http-user=a", "--http-password=b", "http://x/f"]
            ),
            s(&["--user", "a", "--password", "b", "http://x/f"])
        );
        let e = canonicalize(Personality::Curl, &s(&["--ntlm", "http://x/f"])).unwrap_err();
        assert!(e.contains("Basic"), "{e}");
    }

    #[test]
    fn wget_dash_i_is_an_input_file() {
        assert_eq!(
            canon(Personality::Wget, &["-i", "urls.txt"]),
            s(&["--input-file", "urls.txt"])
        );
    }

    #[test]
    fn curl_json_is_a_request_body_and_is_refused_with_its_value_consumed() {
        let e = canonicalize(Personality::Curl, &s(&["--json", "{}", "http://x/f"])).unwrap_err();
        assert!(e.contains("request body"), "{e}");
        // wget has no --json, so there it is hydra's own flag.
        assert_eq!(
            canon(Personality::Wget, &["--json", "http://x/f"]),
            s(&["--json", "http://x/f"])
        );
    }

    #[test]
    fn noproxy_consumes_its_host_list() {
        assert_eq!(
            canon(
                Personality::Curl,
                &["--noproxy", "*", "-o", "f", "http://x/f"]
            ),
            s(&["--no-proxy", "--output", "f", "http://x/f"])
        );
        let e = canonicalize(
            Personality::Curl,
            &s(&["--noproxy", "a.test,b.test", "http://x/f"]),
        )
        .unwrap_err();
        assert!(e.contains("no_proxy"), "{e}");
        assert_eq!(
            canon(Personality::Wget, &["--no-proxy", "http://x/f"]),
            s(&["--no-proxy", "http://x/f"])
        );
    }

    #[test]
    fn a_dash_output_means_stdout_in_the_long_form_too() {
        assert_eq!(
            canon(Personality::Curl, &["--output", "-", "http://x/f"]),
            s(&["--stdout", "http://x/f"])
        );
        assert_eq!(
            canon(Personality::Wget, &["--output=-", "http://x/f"]),
            s(&["--stdout", "http://x/f"])
        );
    }

    #[test]
    fn get_and_head_are_the_only_methods_and_others_are_refused() {
        assert_eq!(
            canon(Personality::Curl, &["-X", "GET", "-o", "f", "http://x/f"]),
            s(&["--output", "f", "http://x/f"])
        );
        assert_eq!(
            canon(Personality::Curl, &["--request", "head", "http://x/f"]),
            s(&["--spider", "http://x/f"])
        );
        assert!(canonicalize(Personality::Curl, &s(&["-X", "POST", "http://x/f"])).is_err());
    }

    #[test]
    fn a_max_filesize_with_a_suffix_survives_the_curl_dialect_and_parses() {
        let (out, _) = canonicalize(
            Personality::Curl,
            &s(&["--max-filesize", "10M", "http://x/f"]),
        )
        .unwrap();
        let full: Vec<String> = std::iter::once("hydra".to_string())
            .chain(out.iter().cloned())
            .collect();
        let parsed = crate::cli::Cli::try_parse_from(&full).expect("10M is a size");
        assert_eq!(parsed.max_filesize, Some(10 << 20));
    }

    #[test]
    fn tries_zero_means_forever_in_wget_and_once_in_curl() {
        assert_eq!(
            canon(Personality::Wget, &["-t", "0", "http://x/f"]),
            s(&["--tries", &u32::MAX.to_string(), "http://x/f"])
        );
        assert_eq!(
            canon(Personality::Wget, &["--tries=3", "http://x/f"]),
            s(&["--tries", "3", "http://x/f"])
        );
        assert_eq!(
            canon(
                Personality::Curl,
                &["--retry", "0", "-o", "f", "http://x/f"]
            ),
            s(&["--tries", "1", "--output", "f", "http://x/f"])
        );
        assert!(canonicalize(Personality::Wget, &s(&["--waitretry", "2", "http://x/f"])).is_err());
    }

    #[test]
    fn headers_and_referer_translate() {
        assert_eq!(
            canon(
                Personality::Curl,
                &["-H", "X-A: 1", "-H", "X-B: 2", "-O", "http://x/f"]
            ),
            s(&[
                "--header",
                "X-A: 1",
                "--header",
                "X-B: 2",
                "--remote-name",
                "http://x/f"
            ])
        );
        // Both tools spell referer differently but it is just a header.
        assert_eq!(
            canon(
                Personality::Curl,
                &["-e", "http://ref/", "-O", "http://x/f"]
            ),
            s(&[
                "--header",
                "Referer: http://ref/",
                "--remote-name",
                "http://x/f"
            ])
        );
        assert_eq!(
            canon(Personality::Wget, &["--referer=http://ref/", "http://x/f"]),
            s(&["--header", "Referer: http://ref/", "http://x/f"])
        );
    }

    #[test]
    fn clustered_shorts_and_glued_values_both_work() {
        // wget -cq is two boolean flags.
        assert_eq!(
            canon(Personality::Wget, &["-cq", "http://x/f"]),
            s(&["--continue", "--quiet", "http://x/f"])
        );
        // A glued value: -t3 is --tries 3.
        assert_eq!(
            canon(Personality::Wget, &["-t3", "http://x/f"]),
            s(&["--tries", "3", "http://x/f"])
        );
        // Boolean then valued, glued: -qt5.
        assert_eq!(
            canon(Personality::Wget, &["-qt5", "http://x/f"]),
            s(&["--quiet", "--tries", "5", "http://x/f"])
        );
    }

    #[test]
    fn stdout_forms_are_recognised() {
        assert_eq!(
            canon(Personality::Wget, &["-O-", "http://x/f"])[0],
            "--stdout"
        );
        assert_eq!(
            canon(Personality::Curl, &["-o", "-", "http://x/f"])[0],
            "--stdout"
        );
    }

    #[test]
    fn agreed_long_flags_pass_through_in_any_dialect() {
        for p in [Personality::Wget, Personality::Curl, Personality::Native] {
            assert_eq!(
                canon(p, &["--limit-rate", "2M", "-O", "http://x/f"])[..2],
                s(&["--limit-rate", "2M"]),
                "dialect {:?}",
                p
            );
        }
    }

    /// Silently ignoring a flag that changes behaviour is the failure mode this
    /// layer exists to prevent.
    #[test]
    fn unsupported_behaviour_is_refused_not_ignored() {
        for (p, args, expect) in [
            (Personality::Wget, vec!["-r", "http://x/"], "crawl"),
            (Personality::Wget, vec!["--recursive", "http://x/"], "crawl"),
            (Personality::Curl, vec!["-d", "a=b", "http://x/"], "GET"),
            (Personality::Curl, vec!["--data", "a=b", "http://x/"], "GET"),
            (Personality::Wget, vec!["--post-data=x", "http://x/"], "GET"),
            (
                Personality::Curl,
                vec!["--compressed", "http://x/"],
                "byte ranges",
            ),
        ] {
            let e = canonicalize(p, &s(&args)).unwrap_err();
            assert!(
                e.contains(expect),
                "{:?} {:?} should be refused mentioning {expect:?}, got: {e}",
                p,
                args
            );
        }
    }

    /// The six cookie flags used to be six `Map::Reject` arms, which is what
    /// made `wget --load-cookies cookies.txt` fail at the argument parser
    /// before anything was attempted.
    #[test]
    fn the_cookie_flags_translate_in_both_dialects() {
        for (p, args, want) in [
            (
                Personality::Curl,
                vec!["-b", "k=v", "http://x/"],
                vec!["--cookie", "k=v"],
            ),
            (
                Personality::Curl,
                vec!["-c", "jar.txt", "http://x/"],
                vec!["--cookie-jar", "jar.txt"],
            ),
            (
                Personality::Curl,
                vec!["-j", "http://x/"],
                vec!["--junk-session-cookies"],
            ),
            (
                Personality::Wget,
                vec!["--load-cookies", "c.txt", "http://x/"],
                vec!["--load-cookies", "c.txt"],
            ),
            (
                Personality::Wget,
                vec![
                    "--save-cookies",
                    "c.txt",
                    "--keep-session-cookies",
                    "http://x/",
                ],
                vec!["--save-cookies", "c.txt", "--keep-session-cookies"],
            ),
        ] {
            let (out, _) = canonicalize(p, &s(&args)).expect("cookies are supported now");
            for w in &want {
                assert!(
                    out.iter().any(|a| a == w),
                    "{args:?} -> {out:?}, want {w:?}"
                );
            }
            let full: Vec<String> = std::iter::once("hydra".to_string())
                .chain(out.iter().cloned())
                .collect();
            crate::cli::Cli::try_parse_from(&full)
                .unwrap_or_else(|e| panic!("{out:?} is not parseable: {e}"));
        }
    }

    #[test]
    fn compression_refusal_explains_the_real_reason() {
        let e = canonicalize(Personality::Curl, &s(&["--compressed", "http://x/f"])).unwrap_err();
        assert!(
            e.contains("COMPRESSED stream"),
            "the refusal must explain why ranges and content-encoding do not mix: {e}"
        );
    }

    #[test]
    fn inert_flags_are_accepted_with_a_note() {
        let (out, notes) =
            canonicalize(Personality::Wget, &s(&["--no-dns-cache", "http://x/f"])).unwrap();
        assert_eq!(out, s(&["http://x/f"]));
        assert_eq!(notes.len(), 1);
        assert!(notes[0].contains("no effect"));
    }

    #[test]
    fn valued_inert_flags_consume_their_value() {
        let (out, _) =
            canonicalize(Personality::Wget, &s(&["--dns-timeout", "5", "http://x/f"])).unwrap();
        assert_eq!(
            out,
            s(&["http://x/f"]),
            "the value must not leak through as a URL"
        );
    }

    #[test]
    fn a_missing_value_is_an_error() {
        assert!(canonicalize(Personality::Wget, &s(&["-O"])).is_err());
        assert!(canonicalize(Personality::Curl, &s(&["-H"])).is_err());
    }

    #[test]
    fn double_dash_stops_translation() {
        let out = canon(Personality::Wget, &["-c", "--", "-O", "http://x/f"]);
        assert_eq!(out, s(&["--continue", "--", "-O", "http://x/f"]));
    }

    #[test]
    fn safe_short_set_matches_the_implementation() {
        // Every flag claimed universally safe must translate identically in all
        // three dialects, or the help text is lying.
        for ch in universally_safe_shorts() {
            let w = canon(Personality::Wget, &[&format!("-{ch}")]);
            let c = canon(Personality::Curl, &[&format!("-{ch}")]);
            assert_eq!(w, c, "-{ch} differs between dialects");
        }
    }

    /// Native long options must reach the parser untouched.
    ///
    /// Regression test: this layer used to translate in native mode too, so a
    /// native-only flag absent from the table (`--demo-frame`) was rejected here
    /// with "not recognised in native mode" instead of reaching clap, which
    /// actually defines the namespace.
    #[test]
    fn native_long_options_pass_through_untranslated() {
        for flag in ["--demo-frame", "--sort-by-type", "--queue-file", "--split"] {
            assert_eq!(
                canon(Personality::Native, &[flag, "http://x/f"]),
                s(&[flag, "http://x/f"]),
                "{flag} must reach the parser unchanged"
            );
        }
    }

    /// In a foreign dialect, connection flag spellings are translated to the canonical one.
    #[test]
    fn foreign_connection_flags_translate_in_dialects() {
        assert_eq!(
            canon(Personality::Wget, &["--split", "4", "http://x/f"]),
            s(&["--max-connection-per-server", "4", "http://x/f"])
        );
    }
}
