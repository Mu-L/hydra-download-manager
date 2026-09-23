// Copyright (C) 2026 Javad Rajabzadeh
// SPDX-License-Identifier: MIT OR Apache-2.0

//! URL parsing and normalization for supported schemes (`http`, `https`, `ftp`).

use hya_net::url::percent_decode;

/// Parsed URL components required by engine network transports.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Url {
    /// Lowercase scheme (`http`, `https`, or `ftp`).
    pub scheme: String,
    pub host: String,
    pub port: u16,
    /// Request path and query starting with `/`.
    pub path: String,
    /// Extracted userinfo credentials.
    pub user: Option<String>,
    pub pass: Option<String>,
}

impl Url {
    /// Returns true if this URL uses TLS.
    pub(crate) fn tls(&self) -> bool {
        self.scheme == "https"
    }

    /// Returns true if this URL uses FTP.
    pub(crate) fn is_ftp(&self) -> bool {
        self.scheme == "ftp"
    }

    /// Returns `host` or `host:port` authority string.
    pub(crate) fn authority(&self) -> String {
        let default = if self.tls() { 443 } else { 80 };
        if self.port == default {
            self.host.clone()
        } else {
            format!("{}:{}", self.host, self.port)
        }
    }

    /// Returns URL string with user credentials removed.
    pub(crate) fn redacted(&self) -> String {
        let default = match self.scheme.as_str() {
            "https" => 443,
            "ftp" => 21,
            _ => 80,
        };
        if self.port == default {
            format!("{}://{}{}", self.scheme, self.host, self.path)
        } else {
            format!("{}://{}:{}{}", self.scheme, self.host, self.port, self.path)
        }
    }

    /// Extracts suggested filename from the last path segment.
    ///
    /// Decoding admits characters the encoded segment could not contain, so
    /// the result is reduced to its own basename afterwards on BOTH separators
    /// whatever the host OS, and cut at a NUL — `%2F`, `%5C` and `%00` must
    /// not let a URL name a directory, or a different file, than it says.
    pub(crate) fn file_name(&self) -> Option<String> {
        let path = self.path.split(['?', '#']).next().unwrap_or("");
        let seg = path.rsplit('/').find(|s| !s.is_empty()).unwrap_or("");
        let decoded = percent_decode(seg);
        let base = decoded
            .rsplit(['/', '\\'])
            .next()
            .unwrap_or("")
            .split('\0')
            .next()
            .unwrap_or("")
            .trim();
        if base.is_empty() || base == "." || base == ".." {
            None
        } else {
            Some(base.to_string())
        }
    }

    /// Parses a URL string into components.
    pub(crate) fn parse(raw: &str) -> Result<Url, String> {
        let raw = raw.trim();
        if raw.is_empty() {
            return Err("empty URL".into());
        }
        if raw.chars().any(|c| c.is_control()) {
            return Err("URL contains control characters".into());
        }
        let (scheme, rest) = raw
            .split_once("://")
            .ok_or_else(|| format!("{raw:?} has no scheme (want http, https or ftp)"))?;
        let scheme = scheme.to_ascii_lowercase();
        if !matches!(scheme.as_str(), "http" | "https" | "ftp") {
            return Err(format!(
                "unsupported scheme {scheme:?} (supported: {})",
                hya_net::scheme::supported().join(", ")
            ));
        }
        let rest = rest.split('#').next().unwrap_or(rest);
        // The authority ends at the first `/` or `?`: `http://h?x=1` has an
        // empty path and a query, not a host called `h?x=1`.
        let (authority, path) = match rest.find(['/', '?']) {
            Some(i) if rest.as_bytes()[i] == b'?' => (&rest[..i], format!("/{}", &rest[i..])),
            Some(i) => (&rest[..i], rest[i..].to_string()),
            None => (rest, "/".to_string()),
        };
        let (userinfo, hostport) = match authority.rsplit_once('@') {
            Some((u, h)) => (Some(u), h),
            None => (None, authority),
        };
        let (user, pass) = match userinfo {
            Some(u) => match u.split_once(':') {
                Some((a, b)) => (Some(percent_decode(a)), Some(percent_decode(b))),
                None => (Some(percent_decode(u)), None),
            },
            None => (None, None),
        };
        let default_port = match scheme.as_str() {
            "https" => 443,
            "ftp" => 21,
            _ => 80,
        };
        let (host, port) = if let Some(after) = hostport.strip_prefix('[') {
            let (h, tail) = after
                .split_once(']')
                .ok_or_else(|| format!("unterminated IPv6 literal in {raw:?}"))?;
            let p = match tail.strip_prefix(':') {
                Some(p) => p
                    .parse::<u16>()
                    .map_err(|_| format!("bad port in {raw:?}"))?,
                None => default_port,
            };
            (h.to_string(), p)
        } else {
            match hostport.rsplit_once(':') {
                Some((h, p)) if !p.is_empty() && p.chars().all(|c| c.is_ascii_digit()) => (
                    h.to_string(),
                    p.parse::<u16>()
                        .map_err(|_| format!("bad port in {raw:?}"))?,
                ),
                _ => (hostport.to_string(), default_port),
            }
        };
        if host.is_empty() {
            return Err(format!("{raw:?} has no host"));
        }
        Ok(Url {
            scheme,
            host,
            port,
            path,
            user,
            pass,
        })
    }

    /// Resolves a relative or absolute redirect target against this URL.
    pub(crate) fn join(&self, location: &str) -> Result<Url, String> {
        let loc = location.trim();
        if loc.is_empty() {
            return Err("empty Location header".into());
        }
        if has_scheme(loc) {
            return Url::parse(loc);
        }
        let host = if self.host.contains(':') {
            format!("[{}]", self.host)
        } else {
            self.host.clone()
        };
        // Protocol-relative: a new authority under the same scheme.
        if let Some(rest) = loc.strip_prefix("//") {
            return Url::parse(&format!("{}://{rest}", self.scheme));
        }
        let base = format!("{}://{host}:{}", self.scheme, self.port);
        if let Some(rest) = loc.strip_prefix('/') {
            return Url::parse(&format!("{base}/{rest}"));
        }
        // Relative to the directory of the PATH, never of the query: a `/`
        // inside `?redirect=/a/b` is not a directory.
        let path = self.path.split(['?', '#']).next().unwrap_or("/");
        if let Some(query) = loc.strip_prefix('?') {
            return Url::parse(&format!("{base}{path}?{query}"));
        }
        let dir = path.rsplit_once('/').map(|(d, _)| d).unwrap_or("");
        Url::parse(&format!("{base}{dir}/{loc}"))
    }
}

/// Whether `s` starts with a URI scheme (`scheme:`), as RFC 3986 spells one.
///
/// A bare `contains("://")` would read `next?u=http://x` as absolute.
fn has_scheme(s: &str) -> bool {
    let Some((scheme, _)) = s.split_once(':') else {
        return false;
    };
    let mut chars = scheme.chars();
    chars.next().is_some_and(|c| c.is_ascii_alphabetic())
        && chars.all(|c| c.is_ascii_alphanumeric() || matches!(c, '+' | '-' | '.'))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_and_authority() {
        let u = Url::parse("https://example.com/a/b.iso?x=1").unwrap();
        assert_eq!(u.port, 443);
        assert_eq!(u.authority(), "example.com");
        assert_eq!(u.path, "/a/b.iso?x=1");
        assert_eq!(u.file_name().as_deref(), Some("b.iso"));

        let u = Url::parse("http://example.com:8080/x").unwrap();
        assert_eq!(u.authority(), "example.com:8080");
    }

    #[test]
    fn ipv6_literal_keeps_its_colons() {
        let u = Url::parse("http://[2001:db8::1]:8080/f").unwrap();
        assert_eq!(u.host, "2001:db8::1");
        assert_eq!(u.port, 8080);
    }

    #[test]
    fn a_redacted_url_carries_no_credentials() {
        let u = Url::parse("ftp://bob:p%40ss@files.example/pub/x.tar").unwrap();
        let r = u.redacted();
        assert_eq!(r, "ftp://files.example/pub/x.tar");
        assert!(!r.contains("bob") && !r.contains("ss"), "{r}");
        // A non-default port is load-bearing and stays.
        let u = Url::parse("http://a:b@h.example:8080/x").unwrap();
        assert_eq!(u.redacted(), "http://h.example:8080/x");
    }

    #[test]
    fn ftp_userinfo_is_extracted_and_decoded() {
        let u = Url::parse("ftp://bob:p%40ss@files.example/pub/x.tar").unwrap();
        assert!(u.is_ftp());
        assert_eq!(u.port, 21);
        assert_eq!(u.user.as_deref(), Some("bob"));
        assert_eq!(u.pass.as_deref(), Some("p@ss"));
    }

    #[test]
    fn hostile_inputs_are_refused_rather_than_normalised() {
        for bad in [
            "",
            "example.com/x",
            "gopher://example.com/x",
            "https:///nohost",
            "https://exa\r\nmple.com/x",
        ] {
            assert!(Url::parse(bad).is_err(), "{bad:?} must not parse");
        }
    }

    #[test]
    fn a_server_supplied_name_cannot_escape_a_directory() {
        let u = Url::parse("https://example.com/a/%2e%2e%2f%2e%2e%2fetc%2fpasswd").unwrap();
        assert_eq!(u.file_name().as_deref(), Some("passwd"));
        let u = Url::parse("https://example.com/").unwrap();
        assert_eq!(u.file_name(), None);
    }

    #[test]
    fn redirects_resolve_in_all_three_forms() {
        let base = Url::parse("https://a.example/dir/file").unwrap();
        assert_eq!(base.join("https://b.example/x").unwrap().host, "b.example");
        assert_eq!(base.join("/root").unwrap().path, "/root");
        assert_eq!(base.join("sib").unwrap().path, "/dir/sib");
    }

    #[test]
    fn a_protocol_relative_location_changes_the_host_and_keeps_the_scheme() {
        let base = Url::parse("https://a.example:8443/dir/file").unwrap();
        let next = base.join("//cdn.example/x/y.bin").unwrap();
        assert_eq!(next.scheme, "https");
        assert_eq!(next.host, "cdn.example");
        assert_eq!(next.port, 443);
        assert_eq!(next.path, "/x/y.bin");
    }

    #[test]
    fn a_relative_location_ignores_the_query_of_the_base() {
        let base = Url::parse("https://a.example/dir/file?next=/evil/path").unwrap();
        assert_eq!(base.join("sib").unwrap().path, "/dir/sib");
        assert_eq!(base.join("?page=2").unwrap().path, "/dir/file?page=2");
        // A query value that looks like a URL does not make the location absolute.
        let next = base.join("go?u=http://other.example/z").unwrap();
        assert_eq!(next.host, "a.example");
        assert_eq!(next.path, "/dir/go?u=http://other.example/z");
        // An IPv6 base keeps its brackets when the path is joined.
        let v6 = Url::parse("http://[::1]:8080/d/f").unwrap();
        assert_eq!(v6.join("g").unwrap().host, "::1");
    }

    #[test]
    fn a_query_without_a_path_is_not_part_of_the_host() {
        let u = Url::parse("http://h.example?x=1").unwrap();
        assert_eq!(u.host, "h.example");
        assert_eq!(u.path, "/?x=1");
    }

    #[test]
    fn a_decoded_segment_cannot_smuggle_a_separator_or_a_nul() {
        let u = Url::parse("https://example.com/a/dir%5Cevil%00.txt").unwrap();
        assert_eq!(u.file_name().as_deref(), Some("evil"));
        let u = Url::parse("https://example.com/a/x%2Fy.bin?q=/z").unwrap();
        assert_eq!(u.file_name().as_deref(), Some("y.bin"));
        // A trailing slash names the directory before it, as the CLI does.
        let u = Url::parse("https://example.com/pub/dist/").unwrap();
        assert_eq!(u.file_name().as_deref(), Some("dist"));
    }
}
