//! URL handling and the resume sidecar.
//!
//! No URL crate: the transport speaks plaintext HTTP/1.1 and needs exactly
//! scheme, authority, and path. Parsing that by hand is a dozen lines and avoids
//! a dependency that would imply support for schemes this client cannot honour.

use hya_net::url::percent_decode;
use hya_net::Target;
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

/// A parsed download URL.
#[derive(Clone, PartialEq, Eq)]
pub struct Url {
    pub scheme: String,
    pub host: String,
    pub port: u16,
    pub path: String,
    /// Userinfo from the URL, kept only because FTP's authentication IS the URL
    /// (`ftp://user:pass@host/path`), unlike HTTP where credentials are a header.
    ///
    /// Retained for every scheme so the parse is one code path, but only ever *used* by
    /// FTP: `to_target` and `proxy_authority` continue to exclude it, so an HTTP request
    /// line and a proxy CONNECT can never carry it. Excluded from `Debug` for the same
    /// reason the Endpoint type is.
    pub user: Option<String>,
    pub pass: Option<String>,
}

impl std::fmt::Debug for Url {
    /// Never print credentials: a derive would leak them into every log line and error.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Url")
            .field("scheme", &self.scheme)
            .field("host", &self.host)
            .field("port", &self.port)
            .field("path", &self.path)
            .field("user", &self.user.as_ref().map(|_| "<set>"))
            .field("pass", &self.pass.as_ref().map(|_| "<redacted>"))
            .finish()
    }
}

impl std::fmt::Display for Url {
    /// The absolute URL, WITHOUT userinfo.
    ///
    /// Credentials are excluded for the same reason `Debug` excludes them: this
    /// is what gets printed, logged, and written into a resume sidecar, and a
    /// password in any of those outlives the command that typed it. The default
    /// port is elided so the round trip through `parse` is stable.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let default = match self.scheme.as_str() {
            "https" => 443,
            "ftp" => 21,
            _ => 80,
        };
        write!(f, "{}://{}", self.scheme, self.host_for_authority())?;
        if self.port != default {
            write!(f, ":{}", self.port)?;
        }
        write!(f, "{}", self.path)
    }
}

impl Url {
    pub fn parse(s: &str) -> Option<Self> {
        let (scheme, rest) = s.split_once("://")?;
        let scheme = scheme.to_ascii_lowercase();
        if !hya_net::scheme::supported().contains(&scheme.as_str()) {
            return None;
        }
        let (auth, path) = match rest.find('/') {
            Some(i) => (&rest[..i], rest[i..].to_string()),
            None => (rest, "/".to_string()),
        };
        // Split userinfo from the host. `rsplit_once` on '@' is deliberate: a password may
        // legitimately contain '@', and the LAST one separates credentials from the host.
        let (userinfo, hostport) = match auth.rsplit_once('@') {
            Some((ui, hp)) => (Some(ui), hp),
            None => (None, auth),
        };
        let (user, pass) = match userinfo {
            Some(ui) => match ui.split_once(':') {
                Some((u, p)) => (Some(percent_decode(u)), Some(percent_decode(p))),
                None => (Some(percent_decode(ui)), None),
            },
            None => (None, None),
        };
        let default_port = match scheme.as_str() {
            "https" => 443,
            "ftp" => 21,
            _ => 80,
        };
        // A bracketed IPv6 literal carries colons of its own, so the port is
        // whatever follows the closing bracket. The brackets are URL syntax,
        // not part of the address the resolver wants.
        let (host, port) = if let Some(rest) = hostport.strip_prefix('[') {
            let (h, after) = rest.split_once(']')?;
            let port = match after.strip_prefix(':') {
                Some(p) => p.parse().ok()?,
                None if after.is_empty() => default_port,
                None => return None,
            };
            (h.to_string(), port)
        } else {
            match hostport.rsplit_once(':') {
                Some((h, p)) => (h.to_string(), p.parse().ok()?),
                None => (hostport.to_string(), default_port),
            }
        };
        if host.is_empty() {
            return None;
        }
        Some(Self {
            scheme,
            host,
            port,
            path,
            user,
            pass,
        })
    }

    /// Is this an FTP URL?
    pub fn is_ftp(&self) -> bool {
        self.scheme == "ftp"
    }

    /// Build a protocol-neutral endpoint for the scheme layer.
    pub fn to_endpoint(&self, proxy: Option<(&str, u16)>) -> hya_net::scheme::Endpoint {
        let mut e = hya_net::scheme::Endpoint::new(&self.host, self.port, &self.path);
        e.tls = self.scheme == "https";
        e.origin = proxy.map(|(h, p)| (h.to_string(), p));
        // Credentials reach the endpoint only for FTP. Attaching them for HTTP would put
        // them one careless format! away from a request line or a log.
        if self.is_ftp() {
            e.user = self.user.clone();
            e.pass = self.pass.clone();
        }
        e
    }

    /// Resolve a redirect `Location` against this URL.
    ///
    /// Handles the three forms servers actually send: an absolute URL, a
    /// scheme-relative `//host/path`, and a path-relative `/path`. Needed because a
    /// redirect commonly crosses hosts — a GitHub release asset redirects to a
    /// different domain entirely — so the new target cannot be built by patching the
    /// old one's path.
    pub fn join(&self, location: &str) -> Option<Url> {
        let loc = location.trim();
        if loc.is_empty() {
            return None;
        }
        if loc.contains("://") {
            return Url::parse(loc);
        }
        if let Some(rest) = loc.strip_prefix("//") {
            return Url::parse(&format!("{}://{}", self.scheme, rest));
        }
        if loc.starts_with('/') {
            return Url::parse(&format!(
                "{}://{}:{}{}",
                self.scheme,
                self.host_for_authority(),
                self.port,
                loc
            ));
        }
        // A relative reference resolves against the current directory.
        let base = match self.path.rfind('/') {
            Some(i) => &self.path[..=i],
            None => "/",
        };
        Url::parse(&format!(
            "{}://{}:{}{}{}",
            self.scheme,
            self.host_for_authority(),
            self.port,
            base,
            loc
        ))
    }

    /// Authority for a proxied request: ALWAYS carries the port.
    ///
    /// There is deliberately no `Host`-header variant here. RFC 9110 prefers the
    /// default port omitted in `Host`, and the transport builds that itself from
    /// the target; a second, nearly-identical accessor was dead code that only
    /// invited using the wrong one.
    ///
    /// Deliberately different from [`Self::authority`]. A `Host` header should omit
    /// the default port; a `CONNECT` request line must not, and proxies reject the
    /// portless form. Conflating the two produced a CONNECT to `x.org` with no
    /// port, which fails every proxied TLS handshake.
    pub fn proxy_authority(&self) -> String {
        format!("{}:{}", self.host_for_authority(), self.port)
    }

    /// The host as an authority component spells it: an IPv6 literal goes back
    /// in its brackets.
    fn host_for_authority(&self) -> std::borrow::Cow<'_, str> {
        if self.host.contains(':') {
            std::borrow::Cow::Owned(format!("[{}]", self.host))
        } else {
            std::borrow::Cow::Borrowed(&self.host)
        }
    }

    /// `Authorization: Basic` for userinfo in an http(s) URL.
    ///
    /// FTP sends its credentials on the control channel and never gets one.
    pub fn basic_auth_header(&self) -> Option<String> {
        if self.is_ftp() {
            return None;
        }
        let user = self.user.as_deref()?;
        Some(basic_auth_line(user, self.pass.as_deref().unwrap_or("")))
    }

    /// Filename implied by the URL path, for when `-O` is not given.
    ///
    /// The segment is percent-decoded, because a path is an encoded form: saving
    /// `Elementor%20Pro.zip` verbatim writes the escape into the name the user reads.
    /// Decoding admits characters the encoded segment could not contain, so the result
    /// is reduced to its own basename afterwards — `%2F` and `%5C` are separators and
    /// `%00` truncates the name at the OS boundary, and a URL must not get to pick a
    /// directory for a download that was never told one.
    pub fn suggested_filename(&self) -> String {
        // Query strings and fragments are not part of a filename, and they are stripped
        // BEFORE the path is split: a query may itself contain '/', and taking the last
        // segment first names the file after the tail of `?redirect=/a/b.zip`.
        let path = self.path.split(['?', '#']).next().unwrap_or("");
        let segment = path.rsplit('/').find(|s| !s.is_empty()).unwrap_or("");
        let decoded = percent_decode(segment);
        let base = decoded
            .rsplit(['/', '\\'])
            .next()
            .unwrap_or("")
            .split('\0')
            .next()
            .unwrap_or("")
            .trim();
        if base.is_empty() || base == "." || base == ".." {
            "download".to_string()
        } else {
            base.to_string()
        }
    }

    /// Build a transport target, routing through `proxy` when one is configured.
    ///
    /// Both schemes are supported. A proxied `https` target keeps `tls: true` so
    /// the connector opens a CONNECT tunnel before handshaking — the proxy has to
    /// read the request line in cleartext, and an encrypted one cannot be read.
    pub fn to_target(&self, proxy: Option<(&str, u16)>) -> Result<Target, String> {
        let tls = self.scheme == "https";
        let mut t = match proxy {
            // The proxy authority must ALWAYS carry an explicit port. `authority()`
            // omits the default one because a `Host` header should, but a CONNECT
            // request line without a port is rejected by proxies — so the two
            // spellings are deliberately different here.
            Some((ph, pp)) => Target::via_proxy(ph, pp, &self.proxy_authority(), &self.path),
            None if tls => Target::direct_tls(&self.host, self.port, &self.path),
            None => Target::direct(&self.host, self.port, &self.path),
        };
        t.tls = tls;
        Ok(t)
    }
}

/// `Authorization: Basic` / `Proxy-Authorization: Basic` field line.
pub fn basic_auth_line(user: &str, pass: &str) -> String {
    format!("Authorization: Basic {}", hya_net::basic_auth(user, pass))
}

/// How a run chooses its proxy: `--no-proxy` beats `--proxy` beats the environment.
#[derive(Clone, Debug, Default)]
pub struct ProxyPolicy {
    pub explicit: Option<String>,
    pub disabled: bool,
}

impl ProxyPolicy {
    pub fn new(explicit: Option<&str>, disabled: bool) -> Self {
        Self {
            explicit: explicit.map(str::to_string),
            disabled,
        }
    }

    /// The proxy a request to `u` goes through, if any.
    pub fn for_url(&self, u: &Url) -> Result<Option<hya_net::Proxy>, String> {
        if self.disabled {
            return Ok(None);
        }
        match &self.explicit {
            Some(spec) => hya_net::Proxy::parse(spec)
                .map(Some)
                .map_err(|e| format!("--proxy: {e}")),
            None => proxy_from_env(u),
        }
    }

    /// The HTTP proxy route for `u`, as `Url::to_target` wants it. A SOCKS proxy
    /// lives on the connector, so the target is built as if direct.
    pub fn http_route(&self, u: &Url) -> Result<Option<(String, u16)>, String> {
        Ok(self
            .for_url(u)?
            .filter(|p| !p.kind.is_socks())
            .map(|p| (p.host, p.port)))
    }

    /// The `Proxy-Authorization` line an HTTP proxy with credentials needs.
    pub fn auth_header(&self, u: &Url) -> Option<String> {
        let p = self.for_url(u).ok()??;
        if p.kind.is_socks() {
            return None;
        }
        let user = p.username?;
        Some(format!(
            "Proxy-{}",
            basic_auth_line(&user, p.password.as_deref().unwrap_or(""))
        ))
    }
}

/// Proxy for `u` from the environment, following the convention every client
/// reads: `https_proxy` for https, `http_proxy` for http, `all_proxy` as the
/// fallback, and `no_proxy` as the exemption list.
pub fn proxy_from_env(u: &Url) -> Result<Option<hya_net::Proxy>, String> {
    let var = |names: &[&str]| {
        names
            .iter()
            .find_map(|n| std::env::var(n).ok().filter(|v| !v.trim().is_empty()))
    };
    let raw = match u.scheme.as_str() {
        "https" => var(&["https_proxy", "HTTPS_PROXY"]),
        "ftp" => var(&["ftp_proxy", "FTP_PROXY"]),
        _ => var(&["http_proxy", "HTTP_PROXY"]),
    }
    .or_else(|| var(&["all_proxy", "ALL_PROXY"]));
    let Some(r) = raw else { return Ok(None) };
    let proxy = hya_net::Proxy::parse(&r)
        .map_err(|e| format!("proxy from the environment ({r:?}): {e}"))?;
    Ok((!proxy.bypasses(&u.host)).then_some(proxy))
}

/// Resume state, written beside the output file as `<output>.hydra`.
///
/// Resume is only sound when the object has not changed underneath us. The
/// validator is therefore part of the sidecar and is compared before any byte is
/// reused: without it, resuming across a server-side update silently splices two
/// versions of a file together, which is the unsound case Proposition 13 names.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Sidecar {
    pub size: u64,
    /// `ETag` if the server offered one, else `Last-Modified`, else `None`.
    pub validator: Option<String>,
    /// Byte ranges already written, as `[lo, hi)` pairs.
    pub done: Vec<(u64, u64)>,
    pub url: String,
}

impl Sidecar {
    pub fn path_for(output: &Path) -> PathBuf {
        let mut s = output.as_os_str().to_os_string();
        s.push(".hydra");
        PathBuf::from(s)
    }

    pub fn load(output: &Path) -> Option<Self> {
        let raw = std::fs::read_to_string(Self::path_for(output)).ok()?;
        serde_json::from_str(&raw).ok()
    }

    pub fn save(&self, output: &Path) -> std::io::Result<()> {
        let tmp = Self::path_for(output).with_extension("hydra.tmp");
        std::fs::write(&tmp, serde_json::to_vec_pretty(self)?)?;
        // Atomic replace: a crash mid-write must not leave a sidecar that
        // describes a state the file was never in.
        std::fs::rename(tmp, Self::path_for(output))
    }

    pub fn remove(output: &Path) {
        let _ = std::fs::remove_file(Self::path_for(output));
    }

    /// Bytes already held, for progress accounting.
    pub fn bytes_done(&self) -> u64 {
        self.done.iter().map(|(a, b)| b.saturating_sub(*a)).sum()
    }

    /// Can this sidecar be resumed against a freshly-probed object?
    ///
    /// Returns `Err` with a human-readable reason when it cannot, because
    /// silently restarting a 900 MB download is worse than saying why.
    pub fn can_resume(&self, size: u64, validator: Option<&str>) -> Result<(), String> {
        if self.size != size {
            return Err(format!(
                "size changed ({} -> {}), cannot resume",
                self.size, size
            ));
        }
        match (&self.validator, validator) {
            (Some(a), Some(b)) if a != b => Err(format!(
                "validator changed ({a} -> {b}), object was modified"
            )),
            (Some(_), None) => {
                Err("server no longer offers a validator, resume is unverifiable".into())
            }
            (None, _) => Err("no validator was recorded, resume is unverifiable".into()),
            (Some(_), Some(_)) => Ok(()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_the_forms_users_actually_type() {
        let u = Url::parse("http://example.com/a/b.tar.gz").unwrap();
        assert_eq!(
            (u.host.as_str(), u.port, u.path.as_str()),
            ("example.com", 80, "/a/b.tar.gz")
        );
        assert_eq!(Url::parse("https://x.org/f").unwrap().port, 443);
        assert_eq!(Url::parse("http://x.org:8080/f").unwrap().port, 8080);
        assert_eq!(Url::parse("http://x.org").unwrap().path, "/");
        // ftp:// is supported now; a scheme this build has no fetcher for is still refused
        // rather than guessed at, so the user gets a reason instead of a wrong protocol.
        assert!(Url::parse("ftp://x.org/f").is_some());
        assert_eq!(Url::parse("gopher://x.org/f"), None);
        assert_eq!(
            Url::parse("file:///etc/passwd"),
            None,
            "unsupported schemes must be refused"
        );
        assert_eq!(Url::parse("not a url"), None);
        assert_eq!(Url::parse("http:///f"), None, "empty host must be refused");
    }

    #[test]
    fn an_ipv6_literal_parses_with_and_without_a_port() {
        let u = Url::parse("http://[::1]/f").unwrap();
        assert_eq!(
            (u.host.as_str(), u.port, u.path.as_str()),
            ("::1", 80, "/f")
        );
        let p = Url::parse("https://[2001:db8::1]:8443/x").unwrap();
        assert_eq!((p.host.as_str(), p.port), ("2001:db8::1", 8443));
        assert_eq!(p.proxy_authority(), "[2001:db8::1]:8443");
        assert_eq!(u.to_string(), "http://[::1]/f");
        assert_eq!(Url::parse("http://[::1/f"), None, "an unclosed bracket");
        assert_eq!(
            Url::parse("http://[::1]x/f"),
            None,
            "junk after the bracket"
        );
    }

    #[test]
    fn http_userinfo_becomes_a_basic_authorization_header() {
        let u = Url::parse("http://alice:secret@h/f").unwrap();
        assert_eq!(
            u.basic_auth_header().as_deref(),
            Some("Authorization: Basic YWxpY2U6c2VjcmV0")
        );
        let no_pass = Url::parse("http://alice@h/f").unwrap();
        assert_eq!(
            no_pass.basic_auth_header().as_deref(),
            Some("Authorization: Basic YWxpY2U6")
        );
        assert!(Url::parse("http://h/f")
            .unwrap()
            .basic_auth_header()
            .is_none());
        assert!(
            Url::parse("ftp://a:b@h/f")
                .unwrap()
                .basic_auth_header()
                .is_none(),
            "FTP logs in on the control channel"
        );
    }

    #[test]
    fn base64_matches_the_rfc_vectors() {
        for (input, want) in [
            ("", ""),
            ("f", "Zg=="),
            ("fo", "Zm8="),
            ("foo", "Zm9v"),
            ("foob", "Zm9vYg=="),
            ("fooba", "Zm9vYmE="),
            ("foobar", "Zm9vYmFy"),
        ] {
            assert_eq!(hya_net::base64::encode(input.as_bytes()), want);
        }
        assert_eq!(basic_auth_line("pu", "pw"), "Authorization: Basic cHU6cHc=");
    }

    /// Every test that touches the proxy environment holds this: the
    /// variables are process-wide and the harness runs tests in parallel.
    static ENV: std::sync::Mutex<()> = std::sync::Mutex::new(());

    const PROXY_VARS: [&str; 8] = [
        "http_proxy",
        "HTTP_PROXY",
        "https_proxy",
        "HTTPS_PROXY",
        "all_proxy",
        "ALL_PROXY",
        "no_proxy",
        "NO_PROXY",
    ];

    /// Run `f` with the proxy variables cleared, restoring them afterwards.
    fn with_clean_proxy_env(f: impl FnOnce()) {
        let _g = ENV.lock().unwrap_or_else(|e| e.into_inner());
        let saved: Vec<Option<String>> = PROXY_VARS.iter().map(|v| std::env::var(v).ok()).collect();
        for v in PROXY_VARS {
            std::env::remove_var(v);
        }
        f();
        for (v, old) in PROXY_VARS.iter().zip(saved) {
            match old {
                Some(o) => std::env::set_var(v, o),
                None => std::env::remove_var(v),
            }
        }
    }

    #[test]
    fn no_proxy_entries_match_hosts_and_their_subdomains() {
        with_clean_proxy_env(|| {
            let policy = ProxyPolicy::new(None, false);
            let routed = |host: &str| {
                policy
                    .for_url(&Url::parse(&format!("http://{host}/f")).unwrap())
                    .unwrap()
                    .is_some()
            };
            std::env::set_var("http_proxy", "http://p.test:3128");
            for (list, host, exempt) in [
                ("localhost,.example.org", "www.example.org", true),
                ("example.org", "example.org", true),
                ("example.org", "notexample.org", false),
                ("*", "anything.test", true),
                ("127.0.0.1:8080", "127.0.0.1", true),
                ("", "h", false),
            ] {
                std::env::set_var("no_proxy", list);
                assert_eq!(!routed(host), exempt, "no_proxy={list:?} host={host}");
            }
        });
    }

    #[test]
    fn an_explicit_proxy_carries_credentials_and_socks_stays_off_the_target() {
        let u = Url::parse("http://h/f").unwrap();
        let http = ProxyPolicy::new(Some("http://pu:pw@127.0.0.1:8094"), false);
        assert_eq!(
            http.http_route(&u).unwrap(),
            Some(("127.0.0.1".into(), 8094))
        );
        assert_eq!(
            http.auth_header(&u).as_deref(),
            Some("Proxy-Authorization: Basic cHU6cHc=")
        );
        let socks = ProxyPolicy::new(Some("socks5://127.0.0.1:1080"), false);
        assert_eq!(socks.http_route(&u).unwrap(), None);
        assert!(socks.auth_header(&u).is_none());
        assert!(socks.for_url(&u).unwrap().unwrap().kind.is_socks());
        let off = ProxyPolicy::new(Some("http://127.0.0.1:8094"), true);
        assert_eq!(off.for_url(&u).unwrap(), None, "--no-proxy wins");
        assert!(ProxyPolicy::new(Some("gopher://x"), false)
            .for_url(&u)
            .is_err());
    }

    #[test]
    fn the_environment_proxy_is_read_per_scheme_and_honours_no_proxy() {
        with_clean_proxy_env(the_environment_proxy_case);
    }

    fn the_environment_proxy_case() {
        std::env::set_var("https_proxy", "socks5://u:p@sp.test:1080");
        std::env::set_var("all_proxy", "http://ap.test:3128");
        std::env::set_var("no_proxy", "localhost,.internal");

        let policy = ProxyPolicy::new(None, false);
        let https = policy
            .for_url(&Url::parse("https://h/f").unwrap())
            .unwrap()
            .unwrap();
        assert!(https.kind.is_socks());
        assert_eq!(https.username.as_deref(), Some("u"));
        let http = policy
            .for_url(&Url::parse("http://h/f").unwrap())
            .unwrap()
            .unwrap();
        assert_eq!((http.host.as_str(), http.port), ("ap.test", 3128));
        assert_eq!(
            policy
                .for_url(&Url::parse("http://box.internal/f").unwrap())
                .unwrap(),
            None
        );
        std::env::set_var("http_proxy", "nonsense://");
        assert!(policy.for_url(&Url::parse("http://h/f").unwrap()).is_err());
    }

    #[test]
    fn redirect_locations_resolve_in_all_three_forms() {
        let base = Url::parse("https://github.com/o/r/releases/download/v1/asset").unwrap();
        // Absolute, crossing hosts — what a release asset actually returns.
        let a = base
            .join("https://release-assets.githubusercontent.com/x/y?token=abc")
            .unwrap();
        assert_eq!(a.host, "release-assets.githubusercontent.com");
        assert_eq!(a.scheme, "https");
        // Scheme-relative keeps the scheme.
        let b = base.join("//cdn.example.net/file.bin").unwrap();
        assert_eq!(
            (b.scheme.as_str(), b.host.as_str()),
            ("https", "cdn.example.net")
        );
        // Path-absolute keeps host and port.
        let c = base.join("/other/path.bin").unwrap();
        assert_eq!(c.host, "github.com");
        assert_eq!(c.path, "/other/path.bin");
        // Relative resolves against the current directory, not the root.
        let d = base.join("sibling.bin").unwrap();
        assert_eq!(d.path, "/o/r/releases/download/v1/sibling.bin");
        assert!(base.join("").is_none(), "an empty Location is not a target");
    }

    #[test]
    fn userinfo_is_stripped_not_forwarded() {
        let u = Url::parse("http://user:pass@example.com/f").unwrap();
        assert_eq!(u.host, "example.com");
        // Every authority that can reach the wire must be credential-free: the
        // CONNECT request line and the target's own host field.
        assert!(
            !u.proxy_authority().contains('@'),
            "credentials must not reach a CONNECT request line"
        );
        assert!(
            !u.to_target(None).unwrap().host.contains('@'),
            "credentials must not reach the connection target"
        );
    }

    #[test]
    fn host_header_omits_the_default_port_but_connect_never_does() {
        let u = Url::parse("http://x.org/f").unwrap();
        assert_eq!(
            u.proxy_authority(),
            "x.org:80",
            "CONNECT must carry a port; proxies reject the portless form"
        );
        let s = Url::parse("https://x.org/f").unwrap();
        assert_eq!(s.proxy_authority(), "x.org:443");
        // An explicit non-default port is preserved rather than re-derived.
        let e = Url::parse("http://x.org:8080/f").unwrap();
        assert_eq!(e.proxy_authority(), "x.org:8080");
    }

    #[test]
    fn filename_is_derived_from_the_path() {
        assert_eq!(
            Url::parse("http://x.org/a/pkg-1.2.tar.gz")
                .unwrap()
                .suggested_filename(),
            "pkg-1.2.tar.gz"
        );
        assert_eq!(
            Url::parse("http://x.org/a/f.bin?token=abc")
                .unwrap()
                .suggested_filename(),
            "f.bin"
        );
        assert_eq!(
            Url::parse("http://x.org/").unwrap().suggested_filename(),
            "download"
        );
        assert_eq!(
            Url::parse("http://x.org/dir/")
                .unwrap()
                .suggested_filename(),
            "dir"
        );
        // A query may contain '/'; the filename comes from the PATH, not from the
        // tail of a redirect parameter.
        assert_eq!(
            Url::parse("http://x.org/get.php?url=/other/decoy.zip")
                .unwrap()
                .suggested_filename(),
            "get.php"
        );
    }

    #[test]
    fn an_escaped_path_names_the_file_a_human_reads() {
        // The reported bug: a space-bearing name reached disk as `…%20….zip`.
        assert_eq!(
            Url::parse("https://x.org/dl/Elementor%20Pro%204.2.3.zip")
                .unwrap()
                .suggested_filename(),
            "Elementor Pro 4.2.3.zip"
        );
        // Non-ASCII arrives as UTF-8 octets, one escape per byte.
        assert_eq!(
            Url::parse("https://x.org/%D9%86%D9%85%D9%88%D9%86%D9%87.pdf")
                .unwrap()
                .suggested_filename(),
            "نمونه.pdf"
        );
        // A malformed escape is a literal '%', matching how credentials are decoded:
        // a name is better left verbatim than mangled.
        assert_eq!(
            Url::parse("https://x.org/100%pure.bin")
                .unwrap()
                .suggested_filename(),
            "100%pure.bin"
        );
        // The path itself keeps the encoded form — it is what goes on the wire.
        assert_eq!(
            Url::parse("https://x.org/a%20b.zip").unwrap().path,
            "/a%20b.zip"
        );
    }

    #[test]
    fn a_decoded_segment_cannot_choose_a_directory() {
        // Decoding admits characters the segment could not otherwise hold. Each of
        // these would escape the directory the user asked for, or truncate the name
        // where the OS stops reading it.
        for (url, want) in [
            ("https://x.org/%2E%2E%2F%2E%2E%2Fevil.sh", "evil.sh"),
            ("https://x.org/a%2Fb%2Fc.bin", "c.bin"),
            ("https://x.org/..%5C..%5Cevil.exe", "evil.exe"),
            ("https://x.org/ok.zip%00.exe", "ok.zip"),
            ("https://x.org/%2E", "download"),
            ("https://x.org/%2E%2E", "download"),
            ("https://x.org/%2F", "download"),
            ("https://x.org/%20%20", "download"),
        ] {
            assert_eq!(
                Url::parse(url).unwrap().suggested_filename(),
                want,
                "{url} must not name a path outside the output directory"
            );
        }
    }

    #[test]
    fn https_targets_are_marked_for_tls_direct_and_proxied() {
        let direct = Url::parse("https://x.org/f")
            .unwrap()
            .to_target(None)
            .unwrap();
        assert!(direct.tls, "https must request TLS");
        assert_eq!(direct.port, 443);
        assert_eq!(direct.tls_server_name(), "x.org");

        // Through a proxy the socket goes to the proxy, but TLS is still
        // end-to-end with the origin via CONNECT, and SNI must name the origin.
        let proxied = Url::parse("https://x.org/f")
            .unwrap()
            .to_target(Some(("proxy.local", 3128)))
            .unwrap();
        assert!(proxied.tls);
        assert_eq!(proxied.host, "proxy.local");
        assert_eq!(
            proxied.tls_server_name(),
            "x.org",
            "validating the proxy's identity instead of the origin's would be a hole"
        );
        assert_eq!(proxied.proxy_authority(), "x.org:443");

        // Plain http must NOT be marked, proxied or not.
        assert!(
            !Url::parse("http://x.org/f")
                .unwrap()
                .to_target(None)
                .unwrap()
                .tls
        );
        assert!(
            !Url::parse("http://x.org/f")
                .unwrap()
                .to_target(Some(("proxy.local", 3128)))
                .unwrap()
                .tls
        );
    }

    #[test]
    fn resume_requires_a_matching_validator() {
        let sc = Sidecar {
            size: 100,
            validator: Some("\"abc\"".into()),
            done: vec![(0, 40)],
            url: "http://x/f".into(),
        };
        assert!(sc.can_resume(100, Some("\"abc\"")).is_ok());
        assert!(
            sc.can_resume(100, Some("\"zzz\"")).is_err(),
            "a changed ETag must block resume"
        );
        assert!(
            sc.can_resume(200, Some("\"abc\"")).is_err(),
            "a changed size must block resume"
        );
        assert!(
            sc.can_resume(100, None).is_err(),
            "a vanished validator must block resume"
        );
        assert_eq!(sc.bytes_done(), 40);
    }

    #[test]
    fn resume_without_a_recorded_validator_is_refused() {
        let sc = Sidecar {
            size: 100,
            validator: None,
            done: vec![(0, 40)],
            url: "http://x/f".into(),
        };
        assert!(
            sc.can_resume(100, Some("\"abc\"")).is_err(),
            "bytes fetched without a validator cannot be proven to belong to this object"
        );
    }

    #[test]
    fn sidecar_round_trips_through_disk() {
        let dir = std::env::temp_dir().join("hydra_sc_test");
        std::fs::create_dir_all(&dir).unwrap();
        let out = dir.join("obj.bin");
        let sc = Sidecar {
            size: 12_801_696,
            validator: Some("\"c356a0-5b8a7ba768224\"".into()),
            done: vec![(0, 1024), (2048, 4096)],
            url: "http://cran.r-project.org/x".into(),
        };
        sc.save(&out).unwrap();
        let back = Sidecar::load(&out).unwrap();
        assert_eq!(back.done, sc.done);
        assert_eq!(back.validator, sc.validator);
        assert_eq!(back.bytes_done(), 1024 + 2048);
        Sidecar::remove(&out);
        assert!(Sidecar::load(&out).is_none());
    }

    #[test]
    fn ftp_urls_carry_credentials_and_default_to_port_21() {
        let u = Url::parse("ftp://alice:s3cret@ftp.example.org/pub/f.bin").unwrap();
        assert_eq!(u.scheme, "ftp");
        assert_eq!(u.port, 21, "the FTP default port, not HTTP's 80");
        assert_eq!(u.host, "ftp.example.org");
        assert_eq!(u.path, "/pub/f.bin");
        assert_eq!(u.user.as_deref(), Some("alice"));
        assert_eq!(u.pass.as_deref(), Some("s3cret"));
        assert!(u.is_ftp());
        // Anonymous form.
        let a = Url::parse("ftp://ftp.example.org/pub/f.bin").unwrap();
        assert!(a.user.is_none() && a.pass.is_none());
        // User with no password is legal; the server may answer 230 without asking.
        let p = Url::parse("ftp://alice@ftp.example.org/f").unwrap();
        assert_eq!(p.user.as_deref(), Some("alice"));
        assert!(p.pass.is_none());
    }

    #[test]
    fn a_password_containing_an_at_sign_splits_on_the_last_one() {
        // Splitting on the FIRST '@' would take "alice:p" as the credentials and
        // "w@host/f" as the host, producing a login failure that looks like a server fault.
        let u = Url::parse("ftp://alice:p@ssw0rd@ftp.example.org/f").unwrap();
        assert_eq!(u.host, "ftp.example.org");
        assert_eq!(u.pass.as_deref(), Some("p@ssw0rd"));
    }

    #[test]
    fn percent_encoded_credentials_are_decoded_before_use() {
        // A password with a reserved character must be encoded in the URL; sending the
        // encoded form would authenticate with the wrong string.
        let u = Url::parse("ftp://us%65r:p%40ss%3Aword@ftp.example.org/f").unwrap();
        assert_eq!(u.user.as_deref(), Some("user"));
        assert_eq!(u.pass.as_deref(), Some("p@ss:word"));
        // A malformed escape is left verbatim rather than dropped: silently altering a
        // password yields an auth failure that looks like a server problem.
        let bad = Url::parse("ftp://a:100%pure@ftp.example.org/f").unwrap();
        assert_eq!(bad.pass.as_deref(), Some("100%pure"));
    }

    #[test]
    fn credentials_never_appear_in_debug_or_on_the_wire() {
        let u = Url::parse("ftp://alice:s3cret@ftp.example.org/f").unwrap();
        let shown = format!("{u:?}");
        assert!(
            !shown.contains("s3cret") && !shown.contains("alice"),
            "a Debug derive would leak credentials into every log line: {shown}"
        );
        // And for HTTP the credentials must not even reach the endpoint, let alone the wire.
        let h = Url::parse("http://alice:s3cret@example.org/f").unwrap();
        let ep = h.to_endpoint(None);
        assert!(
            ep.user.is_none() && ep.pass.is_none(),
            "HTTP credentials belong in a header, not one format! away from a request line"
        );
        assert!(!h.proxy_authority().contains('@'));
    }

    #[test]
    fn an_ftp_endpoint_is_built_without_tls_and_an_https_one_with_it() {
        let f = Url::parse("ftp://ftp.example.org/f")
            .unwrap()
            .to_endpoint(None);
        assert!(!f.tls && f.port == 21);
        let s = Url::parse("https://example.org/f")
            .unwrap()
            .to_endpoint(None);
        assert!(s.tls && s.port == 443);
    }
}
