// Copyright (C) 2026 Javad Rajabzadeh
// SPDX-License-Identifier: GPL-3.0-or-later

//! What the cookie flags asked for, resolved into a jar.
//!
//! Six flags feed one [`CookieJar`]: a literal string, a jar file in either
//! direction, a browser's own store, and two switches over what a session
//! cookie means. This module is where they are read, combined and written back;
//! [`hya_net::cookies`] is where a cookie means anything.
//!
//! # Nothing here ever logs a value
//!
//! A cookie is a bearer credential. `-v` output, the jar's own notices and
//! every error below name hosts, counts and file paths, and never a name=value
//! pair — a session printed into a terminal scrollback or a CI log is a session
//! given away. The one place a value reaches disk is a jar file the user named,
//! and that is written `0600`.

use std::path::{Path, PathBuf};

use hya_net::cookies::{browser, netscape, CookieJar};

/// The cookie flags, validated but not yet acted on.
///
/// Built once per run; [`CookieSpec::open`] is what turns it into a jar, and it
/// takes the host because a browser import is scoped to one.
#[derive(Clone, Debug)]
pub struct CookieSpec {
    /// `--cookie`: a literal `name=value` string.
    literal: Option<String>,
    /// Files to read, in the order they were named.
    load: Vec<PathBuf>,
    /// `--cookie-jar` / `--save-cookies`: where the jar is written at the end.
    save: Option<PathBuf>,
    browser: Option<browser::Source>,
    keep_session: bool,
    junk_session: bool,
}

impl CookieSpec {
    /// The spec this run's flags describe, or `None` when none were given.
    ///
    /// `None` is what keeps the no-flag behaviour byte-identical: nothing is
    /// read, no header is added, and no jar is written.
    ///
    /// # Errors
    ///
    /// A `--cookies-from-browser` value naming a browser this does not know.
    /// Everything else is a file that may legitimately not exist yet —
    /// `--cookie-jar` on a first run is the normal case — and is reported when
    /// it is read, not here.
    pub fn from_cli(args: &crate::cli::Cli) -> Result<Option<Self>, String> {
        let browser = args
            .cookies_from_browser
            .as_deref()
            .map(str::parse::<browser::Source>)
            .transpose()
            .map_err(|e| format!("--cookies-from-browser: {e}"))?;

        // curl's rule for `-b`: a string with an `=` in it is cookies, anything
        // else is the name of a file to read them from.
        let (literal, from_b) = match args.cookie.as_deref() {
            Some(v) if v.contains('=') => (Some(v.to_string()), None),
            Some(v) => (None, Some(PathBuf::from(v))),
            None => (None, None),
        };

        let load: Vec<PathBuf> = [from_b, args.cookie_jar.clone(), args.load_cookies.clone()]
            .into_iter()
            .flatten()
            .collect();
        let save = args
            .save_cookies
            .clone()
            .or_else(|| args.cookie_jar.clone());

        let nothing = literal.is_none() && load.is_empty() && save.is_none() && browser.is_none();
        if nothing {
            // The two session switches on their own describe how to treat a jar
            // that was never asked for. Saying so beats silently doing nothing.
            if args.keep_session_cookies || args.junk_session_cookies {
                return Err(
                    "--keep-session-cookies and --junk-session-cookies describe a \
                            cookie jar; name one with --cookie-jar, --load-cookies or \
                            --save-cookies"
                        .into(),
                );
            }
            return Ok(None);
        }
        Ok(Some(CookieSpec {
            literal,
            load,
            save,
            browser,
            keep_session: args.keep_session_cookies,
            junk_session: args.junk_session_cookies,
        }))
    }

    /// The jar to start `host` with, and the lines the user should be told.
    ///
    /// Sources are applied in widening order of authority: a file first, then
    /// the browser, then the literal string, so a `--cookie` typed on the
    /// command line wins over the same name read from anywhere else.
    ///
    /// # Blocking
    ///
    /// Reads files and may run the platform's secret-store helper. Call it
    /// before the transfer starts, off the executor.
    pub fn open(&self, host: &str, now: u64) -> Result<(CookieJar, Vec<String>), String> {
        let mut jar = CookieJar::new();
        let mut notes = Vec::new();

        for path in &self.load {
            match std::fs::read_to_string(path) {
                Ok(text) => {
                    let (from_file, skipped) = netscape::parse(&text);
                    notes.push(format!(
                        "read {} cookies from {}{}",
                        from_file.len(),
                        path.display(),
                        if skipped > 0 {
                            format!(" ({skipped} unreadable lines skipped)")
                        } else {
                            String::new()
                        }
                    ));
                    jar.extend(from_file);
                }
                // A jar named for writing need not exist yet, and a first run
                // is the normal way that happens.
                Err(e) if e.kind() == std::io::ErrorKind::NotFound && self.writes(path) => {}
                Err(e) => return Err(format!("{}: {e}", path.display())),
            }
        }
        if self.junk_session {
            jar.purge(now, true);
        }

        if let Some(src) = &self.browser {
            let import = browser::load(src, host, now).map_err(|e| e.to_string())?;
            // The consent line, and the reason it names the file rather than
            // the browser: "reading your cookies" is what malware would also
            // say, and the path is what a user can check.
            notes.push(format!(
                "reading {} cookies for {host} from {}",
                src.browser,
                import.store.display()
            ));
            if import.undecryptable > 0 {
                notes.push(format!(
                    "{} cookie(s) for {host} could not be decrypted and were skipped",
                    import.undecryptable
                ));
            }
            jar.extend(import.jar);
        }

        if let Some(s) = &self.literal {
            jar.add_pairs(s, host);
        }

        jar.purge(now, false);
        Ok((jar, notes))
    }

    /// `path` is also the file this run writes back to.
    fn writes(&self, path: &Path) -> bool {
        self.save.as_deref() == Some(path)
    }

    /// Write the jar to the file the flags named.
    ///
    /// Owner-only permissions, set before the content is written rather than
    /// after: a jar created `0644` and chmod'ed a moment later is readable by
    /// every other user on the box for that moment, and this file holds live
    /// sessions.
    ///
    /// # Errors
    ///
    /// Anything that stops the file being written, named with the path — a jar
    /// the user asked to keep and silently did not get is worse than a failed
    /// download, because they will only find out at the next login.
    pub fn save(&self, jar: &CookieJar, now: u64) -> Result<Option<String>, String> {
        let Some(path) = &self.save else {
            return Ok(None);
        };
        let body = netscape::render(jar, self.keep_session, now);
        write_private(path, &body).map_err(|e| format!("{}: {e}", path.display()))?;
        let n = body
            .lines()
            .filter(|l| !l.starts_with('#') && !l.is_empty())
            .count();
        Ok(Some(format!("wrote {n} cookies to {}", path.display())))
    }
}

/// Create or replace `path` with `body`, readable only by its owner.
#[cfg(unix)]
fn write_private(path: &Path, body: &str) -> std::io::Result<()> {
    use std::io::Write;
    use std::os::unix::fs::OpenOptionsExt;

    let mut f = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(path)?;
    f.write_all(body.as_bytes())
}

/// Windows has no mode bits to set at create time; the file inherits the
/// directory's ACL, which for a user profile directory is already owner-only.
#[cfg(not(unix))]
fn write_private(path: &Path, body: &str) -> std::io::Result<()> {
    std::fs::write(path, body)
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::Parser as _;

    fn cli(argv: &[&str]) -> crate::cli::Cli {
        let mut v = vec!["hydra"];
        v.extend_from_slice(argv);
        v.push("http://example.org/f");
        crate::cli::Cli::parse_from(v)
    }

    fn spec(argv: &[&str]) -> Option<CookieSpec> {
        CookieSpec::from_cli(&cli(argv)).unwrap()
    }

    #[test]
    fn no_cookie_flag_means_no_jar_at_all() {
        assert!(spec(&[]).is_none());
    }

    #[test]
    fn a_session_switch_without_a_jar_is_refused_rather_than_ignored() {
        for flag in ["--keep-session-cookies", "--junk-session-cookies"] {
            let e = CookieSpec::from_cli(&cli(&[flag])).unwrap_err();
            assert!(e.contains("--cookie-jar"), "{e}");
        }
    }

    #[test]
    fn dash_b_is_a_string_when_it_has_an_equals_and_a_file_otherwise() {
        let s = spec(&["--cookie", "a=1; b=2"]).unwrap();
        assert_eq!(s.literal.as_deref(), Some("a=1; b=2"));
        assert!(s.load.is_empty());

        let s = spec(&["--cookie", "jar.txt"]).unwrap();
        assert!(s.literal.is_none());
        assert_eq!(s.load, [PathBuf::from("jar.txt")]);
    }

    #[test]
    fn cookie_jar_reads_and_writes_the_same_file() {
        let s = spec(&["--cookie-jar", "j.txt"]).unwrap();
        assert_eq!(s.load, [PathBuf::from("j.txt")]);
        assert_eq!(s.save.as_deref(), Some(Path::new("j.txt")));
    }

    #[test]
    fn load_and_save_cookies_name_one_direction_each() {
        let s = spec(&["--load-cookies", "in.txt"]).unwrap();
        assert_eq!(s.load, [PathBuf::from("in.txt")]);
        assert!(s.save.is_none());

        let s = spec(&["--save-cookies", "out.txt"]).unwrap();
        assert!(s.load.is_empty());
        assert_eq!(s.save.as_deref(), Some(Path::new("out.txt")));
    }

    #[test]
    fn an_unknown_browser_is_refused_at_argument_time() {
        let e = CookieSpec::from_cli(&cli(&["--cookies-from-browser", "netscape"])).unwrap_err();
        assert!(e.contains("--cookies-from-browser"), "{e}");
        assert!(e.contains("firefox"), "{e}");
    }

    #[test]
    fn a_missing_jar_file_is_an_error_to_read_but_not_to_create() {
        let dir = std::env::temp_dir().join(format!("hydra-cookie-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let missing = dir.join("never-written.txt");

        let s = spec(&["--load-cookies", missing.to_str().unwrap()]).unwrap();
        assert!(s.open("example.org", 0).is_err(), "read-only, must exist");

        let s = spec(&["--cookie-jar", missing.to_str().unwrap()]).unwrap();
        let (jar, notes) = s.open("example.org", 0).unwrap();
        assert!(jar.is_empty() && notes.is_empty());
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn a_literal_cookie_wins_over_the_same_name_in_a_file() {
        let dir = std::env::temp_dir().join(format!("hydra-cookie-win-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("jar.txt");
        std::fs::write(&path, "example.org\tFALSE\t/\tFALSE\t0\tsid\tfrom-file\n").unwrap();

        let s = spec(&[
            "--load-cookies",
            path.to_str().unwrap(),
            "--cookie",
            "sid=typed",
        ])
        .unwrap();
        let (jar, notes) = s.open("example.org", 0).unwrap();
        assert_eq!(
            jar.header_value("example.org", "/", false, 0).as_deref(),
            Some("sid=typed")
        );
        assert!(notes[0].contains("read 1 cookies"), "{notes:?}");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn junk_session_cookies_drops_what_the_file_held() {
        let dir = std::env::temp_dir().join(format!("hydra-cookie-junk-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("jar.txt");
        std::fs::write(
            &path,
            "example.org\tFALSE\t/\tFALSE\t0\tsession\ta\n\
             example.org\tFALSE\t/\tFALSE\t2000000000\tkept\tb\n",
        )
        .unwrap();

        let argv = [
            "--load-cookies",
            path.to_str().unwrap(),
            "--junk-session-cookies",
        ];
        let (jar, _) = spec(&argv).unwrap().open("example.org", 0).unwrap();
        assert_eq!(
            jar.header_value("example.org", "/", false, 0).as_deref(),
            Some("kept=b")
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn a_saved_jar_is_owner_only_and_keeps_sessions_only_when_asked() {
        let dir = std::env::temp_dir().join(format!("hydra-cookie-save-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("out.txt");
        let mut jar = CookieJar::new();
        jar.add_pairs("sid=abc", "example.org");

        let s = spec(&["--save-cookies", path.to_str().unwrap()]).unwrap();
        let note = s.save(&jar, 0).unwrap().unwrap();
        assert!(note.contains("wrote 0 cookies"), "{note}");
        assert!(!std::fs::read_to_string(&path).unwrap().contains("sid"));

        let argv = [
            "--save-cookies",
            path.to_str().unwrap(),
            "--keep-session-cookies",
        ];
        let s = spec(&argv).unwrap();
        assert!(s
            .save(&jar, 0)
            .unwrap()
            .unwrap()
            .contains("wrote 1 cookies"));
        assert!(std::fs::read_to_string(&path).unwrap().contains("sid"));

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&path).unwrap().permissions().mode();
            assert_eq!(mode & 0o777, 0o600, "a jar holds live sessions");
        }
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn nothing_is_written_when_no_flag_named_a_destination() {
        let s = spec(&["--load-cookies", "in.txt"]).unwrap();
        assert_eq!(s.save(&CookieJar::new(), 0).unwrap(), None);
    }
}
