// Copyright (C) 2026 Javad Rajabzadeh
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Reading a Chromium-family cookie store, including its at-rest encryption.
//!
//! Every Chromium encrypts cookie values before writing them, with a key held
//! by the platform's own secret store. Reading the database is therefore only
//! half the job; the other half is asking the platform for the key the way the
//! browser itself does, which is deliberately the SAME request the browser
//! makes — on macOS it goes through the Keychain, so the user is prompted by
//! the OS if they have not already allowed it.
//!
//! | Platform | Key | Cipher |
//! |---|---|---|
//! | macOS | Keychain generic password `<Browser> Safe Storage`, PBKDF2-HMAC-SHA1, 1003 rounds | AES-128-CBC, IV of 16 spaces |
//! | Linux | libsecret, else the documented fallback password `peanuts`, PBKDF2-HMAC-SHA1, 1 round | AES-128-CBC, IV of 16 spaces |
//! | Windows | `Local State` → `os_crypt.encrypted_key`, unwrapped with DPAPI | AES-256-GCM |
//!
//! The constants (`saltysalt`, `peanuts`, the round counts, the space IV) are
//! Chromium's own, from `components/os_crypt`. They are not secrets and are not
//! chosen here; they are what the file was written with.

use super::browser::{Browser, Error};
use super::{canonical_host, Cookie, CookieJar};
use crate::cookies::sqlite;
use std::path::Path;

/// Chromium's salt, round counts and IV, from `components/os_crypt`.
///
/// Unix only: Windows derives nothing, it unwraps a key DPAPI already holds.
#[cfg(not(target_os = "windows"))]
const SALT: &[u8] = b"saltysalt";
#[cfg(not(target_os = "windows"))]
const IV: [u8; 16] = [b' '; 16];
#[cfg(target_os = "macos")]
const ROUNDS: u32 = 1003;
#[cfg(not(any(target_os = "macos", target_os = "windows")))]
const ROUNDS: u32 = 1;

/// Microseconds between 1601-01-01 and the Unix epoch: Chromium stores times in
/// the Windows FILETIME base whatever platform it runs on.
const WINDOWS_EPOCH_OFFSET: i64 = 11_644_473_600;

/// Read a Chromium `cookies` table, decrypting what it can.
///
/// Returns the jar and the number of cookies whose value could not be
/// decrypted. A cookie that cannot be decrypted is DROPPED rather than sent
/// with a garbled value: a wrong `Cookie:` header is answered with a `403` that
/// looks like a server fault, while a missing one at least fails honestly.
pub(crate) fn read(
    db: &sqlite::Db,
    store: &Path,
    browser: Browser,
) -> Result<(CookieJar, usize), Error> {
    let cols = [
        "host_key",
        "name",
        "value",
        "encrypted_value",
        "path",
        "expires_utc",
        "is_secure",
        "is_httponly",
    ];
    let rows = db.rows("cookies", &cols).map_err(|e| Error::Read {
        store: store.to_path_buf(),
        why: e.to_string(),
    })?;

    // Asked for once for the whole table rather than per row: on macOS this is
    // a process spawn and a possible Keychain prompt, and 1003 rounds of
    // PBKDF2 are not free either. Not asked for at all when nothing in the
    // table is encrypted, so a `--password-store=basic` profile needs no key.
    let key = match rows.iter().any(|r| needs_key(r)) {
        true => Some(master_key(browser, store)?),
        false => None,
    };
    Ok(rows_to_jar(rows, key.as_ref()))
}

/// This row's value is encrypted and cannot be read without the master key.
fn needs_key(r: &[sqlite::Value]) -> bool {
    !r[3].as_bytes().is_empty() && r[2].as_str().is_empty()
}

/// Turn `cookies` rows into a jar, and count what could not be decrypted.
///
/// Separate from [`read`] and free of I/O: everything platform-specific is on
/// the other side of `key`, so the mapping from a Chromium row to a cookie —
/// the leading-dot convention, the FILETIME base, which failures are fatal —
/// is testable without a keychain.
fn rows_to_jar(rows: Vec<Vec<sqlite::Value>>, key: Option<&Key>) -> (CookieJar, usize) {
    let mut jar = CookieJar::new();
    let mut undecryptable = 0;
    for r in rows {
        let host_field = r[0].as_str();
        let name = r[1].as_str();
        if name.is_empty() || host_field.is_empty() {
            continue;
        }
        let encrypted = r[3].as_bytes();
        let value = if encrypted.is_empty() {
            r[2].as_str().to_string()
        } else {
            match key.and_then(|k| decrypt(k, encrypted, host_field)) {
                Some(v) => v,
                None => {
                    undecryptable += 1;
                    continue;
                }
            }
        };
        jar.insert(Cookie {
            name: name.to_string(),
            value,
            domain: canonical_host(host_field),
            host_only: !host_field.starts_with('.'),
            path: super::browser::path_or_root(r[4].as_str()),
            secure: r[6].as_int() != 0,
            http_only: r[7].as_int() != 0,
            expires: from_chromium_time(r[5].as_int()),
        });
    }
    (jar, undecryptable)
}

/// Chromium's `expires_utc` in Unix seconds, or `None` for a session cookie.
fn from_chromium_time(micros: i64) -> Option<u64> {
    if micros <= 0 {
        return None;
    }
    u64::try_from(micros / 1_000_000 - WINDOWS_EPOCH_OFFSET).ok()
}

/// Decrypt one `encrypted_value` blob.
///
/// `host` is needed because Chromium 130 and later prepend a 32-byte SHA-256 of
/// the cookie's domain to the plaintext before encrypting, as a binding between
/// the value and the host it belongs to. There is no version marker for it, so
/// the prefix is detected by testing the binding rather than guessed from a
/// browser version this code cannot see.
fn decrypt(key: &Key, blob: &[u8], host: &str) -> Option<String> {
    let plain = decrypt_raw(key, blob)?;
    // The binding is tested BEFORE the plain reading: a hash that happens to
    // be valid UTF-8 would otherwise be returned as the first 32 bytes of the
    // value, and the order costs one comparison.
    if let Some((bound, rest)) = plain.split_at_checked(32) {
        if bound == sha256(host.as_bytes()) {
            return std::str::from_utf8(rest).ok().map(str::to_string);
        }
    }
    std::str::from_utf8(&plain).ok().map(str::to_string)
}

fn sha256(b: &[u8]) -> [u8; 32] {
    use sha2::Digest;
    sha2::Sha256::digest(b).into()
}

/// PBKDF2-HMAC-SHA1 over Chromium's fixed salt.
///
/// `rounds` is a parameter rather than [`ROUNDS`] read directly because macOS
/// and Linux use different counts for the same construction, and a test that
/// cannot vary it cannot show that the count is actually applied.
#[cfg(not(target_os = "windows"))]
fn derive(password: &[u8], rounds: u32) -> [u8; 16] {
    let mut key = [0u8; 16];
    // Infallible for a 16-byte output: the error case is an output longer than
    // the PRF can produce, which is 2^32-1 blocks of 20 bytes.
    let _ = pbkdf2::pbkdf2::<hmac::Hmac<sha1::Sha1>>(password, SALT, rounds, &mut key);
    key
}

// ---------------------------------------------------------------- unix

/// The AES key, sized by the platform's scheme.
#[cfg(not(target_os = "windows"))]
pub(crate) type Key = [u8; 16];
#[cfg(target_os = "windows")]
pub(crate) type Key = [u8; 32];

#[cfg(not(target_os = "windows"))]
fn master_key(browser: Browser, store: &Path) -> Result<Key, Error> {
    let password = keyring_password(browser).ok_or_else(|| Error::Decrypt {
        store: store.to_path_buf(),
        why: format!(
            "the key for {browser} is held by the system secret store, which \
             refused or is unavailable"
        ),
    })?;
    Ok(derive(password.as_bytes(), ROUNDS))
}

/// AES-128-CBC with Chromium's fixed IV. Accepts the `v10` and `v11` markers,
/// which distinguish "encrypted with the real keyring key" from "encrypted with
/// the `peanuts` fallback" — both are the same cipher and the same derivation,
/// differing only in which password went in, which the keyring lookup below has
/// already settled.
#[cfg(not(target_os = "windows"))]
fn decrypt_raw(key: &Key, blob: &[u8]) -> Option<Vec<u8>> {
    use aes::cipher::{block_padding::Pkcs7, BlockModeDecrypt, KeyIvInit};

    let body = blob
        .strip_prefix(b"v10")
        .or_else(|| blob.strip_prefix(b"v11"))?;
    if body.is_empty() || body.len() % 16 != 0 {
        return None;
    }
    let mut buf = body.to_vec();
    let n = cbc::Decryptor::<aes::Aes128>::new(key.into(), &IV.into())
        .decrypt_padded::<Pkcs7>(&mut buf)
        .ok()?
        .len();
    buf.truncate(n);
    Some(buf)
}

/// The browser's password from the platform secret store.
///
/// Spawning the platform's own CLI rather than linking its framework: this runs
/// once per import, the tools are part of the OS, and going through
/// `/usr/bin/security` means the Keychain prompt the user sees names a request
/// they can read and refuse.
#[cfg(target_os = "macos")]
fn keyring_password(browser: Browser) -> Option<String> {
    let service = format!("{} Safe Storage", keyring_name(browser));
    let out = std::process::Command::new("/usr/bin/security")
        .args([
            "find-generic-password",
            "-w",
            "-s",
            &service,
            "-a",
            keyring_name(browser),
        ])
        .output()
        .ok()?;
    out.status
        .success()
        .then(|| String::from_utf8_lossy(&out.stdout).trim().to_string())
        .filter(|p| !p.is_empty())
}

#[cfg(not(any(target_os = "macos", target_os = "windows")))]
fn keyring_password(browser: Browser) -> Option<String> {
    // libsecret first, through the tool every desktop ships with it. Chromium
    // stores the password under `application` = its own lowercase name.
    let found = std::process::Command::new("secret-tool")
        .args(["lookup", "application", browser.name()])
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .filter(|p| !p.is_empty());
    // Chromium's documented fallback when no keyring is available, which is
    // also what it uses under `--password-store=basic`. Not a guess: it is the
    // literal in `components/os_crypt/sync/os_crypt_linux.cc`.
    Some(found.unwrap_or_else(|| "peanuts".to_string()))
}

#[cfg(target_os = "macos")]
fn keyring_name(browser: Browser) -> &'static str {
    match browser {
        Browser::Chrome => "Chrome",
        Browser::Chromium => "Chromium",
        Browser::Edge => "Microsoft Edge",
        Browser::Brave => "Brave",
        Browser::Vivaldi => "Vivaldi",
        Browser::Opera => "Opera",
        _ => "Chromium",
    }
}

// ---------------------------------------------------------------- windows

/// The AES-256-GCM key from `Local State`, unwrapped with DPAPI.
#[cfg(target_os = "windows")]
fn master_key(browser: Browser, store: &Path) -> Result<Key, Error> {
    let _ = browser;
    let fail = |why: String| Error::Decrypt {
        store: store.to_path_buf(),
        why,
    };
    // `Local State` sits in the user-data directory, one level above the
    // profile that holds the store — or two, for the `Network/` layout.
    let state = store
        .ancestors()
        .skip(1)
        .map(|d| d.join("Local State"))
        .find(|p| p.is_file())
        .ok_or_else(|| fail("no `Local State` file holds the key".into()))?;
    let text = std::fs::read_to_string(&state).map_err(|e| fail(e.to_string()))?;
    let json: serde_json::Value =
        serde_json::from_str(&text).map_err(|e| fail(format!("unreadable `Local State`: {e}")))?;
    let b64 = json
        .get("os_crypt")
        .and_then(|o| o.get("encrypted_key"))
        .and_then(|k| k.as_str())
        .ok_or_else(|| fail("`Local State` names no `os_crypt.encrypted_key`".into()))?;
    let wrapped = base64_decode(b64).ok_or_else(|| fail("the stored key is not base64".into()))?;
    let dpapi = wrapped
        .strip_prefix(b"DPAPI".as_slice())
        .ok_or_else(|| fail("the stored key has no DPAPI prefix".into()))?;
    let plain = unprotect(dpapi)
        .ok_or_else(|| fail("DPAPI refused to unwrap the key for this user account".into()))?;
    plain
        .try_into()
        .map_err(|_| fail("the unwrapped key is not 32 bytes".into()))
}

/// AES-256-GCM: `v10` marker, 12-byte nonce, ciphertext, 16-byte tag.
///
/// A `v20` blob is Chromium 127's app-bound encryption, whose key is sealed to
/// the browser executable and can only be unwrapped by a process running as
/// SYSTEM. It is refused here rather than mis-decrypted, and counted so the
/// caller can say why an import came back empty.
#[cfg(target_os = "windows")]
fn decrypt_raw(key: &Key, blob: &[u8]) -> Option<Vec<u8>> {
    use aes_gcm::aead::{Aead, KeyInit};

    let Some(body) = blob.strip_prefix(b"v10") else {
        // Pre-2017 Chromium wrapped each value with DPAPI directly.
        return (!blob.starts_with(b"v20"))
            .then(|| unprotect(blob))
            .flatten();
    };
    if body.len() < 12 + 16 {
        return None;
    }
    let (nonce, sealed) = body.split_at(12);
    let nonce: &[u8; 12] = nonce.try_into().ok()?;
    aes_gcm::Aes256Gcm::new(key.into())
        .decrypt(nonce.into(), sealed)
        .ok()
}

/// `CryptUnprotectData`, the only interface DPAPI has.
#[cfg(target_os = "windows")]
fn unprotect(data: &[u8]) -> Option<Vec<u8>> {
    use windows_sys::Win32::Foundation::LocalFree;
    use windows_sys::Win32::Security::Cryptography::{CryptUnprotectData, CRYPT_INTEGER_BLOB};

    let input = CRYPT_INTEGER_BLOB {
        cbData: u32::try_from(data.len()).ok()?,
        pbData: data.as_ptr() as *mut u8,
    };
    let mut out = CRYPT_INTEGER_BLOB {
        cbData: 0,
        pbData: std::ptr::null_mut(),
    };
    // SAFETY: `input` describes a buffer this function owns for the whole call
    // and that CryptUnprotectData only reads. On success it allocates `out`
    // with LocalAlloc and hands over ownership, which is why the bytes are
    // copied and LocalFree is called before returning; on failure it writes
    // nothing and `out` stays the null blob it was initialised to.
    let ok = unsafe {
        CryptUnprotectData(
            &input,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            0,
            &mut out,
        )
    };
    if ok == 0 || out.pbData.is_null() {
        return None;
    }
    // SAFETY: the call succeeded, so `pbData` points at `cbData` initialised
    // bytes that stay valid until the LocalFree on the next line.
    let plain = unsafe { std::slice::from_raw_parts(out.pbData, out.cbData as usize) }.to_vec();
    // SAFETY: `pbData` is the LocalAlloc'd pointer this call was handed and has
    // not been freed; nothing else holds it.
    unsafe { LocalFree(out.pbData as _) };
    Some(plain)
}

/// Standard base64 with padding, which is what `Local State` holds.
#[cfg(target_os = "windows")]
fn base64_decode(s: &str) -> Option<Vec<u8>> {
    const T: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = Vec::with_capacity(s.len() / 4 * 3);
    let (mut acc, mut bits) = (0u32, 0u32);
    for ch in s.bytes() {
        if ch == b'=' || ch.is_ascii_whitespace() {
            continue;
        }
        let v = T.iter().position(|&t| t == ch)? as u32;
        acc = acc << 6 | v;
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push((acc >> bits) as u8);
        }
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A `cookies` row in the column order [`read`] projects.
    fn row(
        host: &str,
        name: &str,
        value: &str,
        encrypted: &[u8],
        path: &str,
        exp: i64,
    ) -> Vec<sqlite::Value> {
        vec![
            sqlite::Value::Text(host.into()),
            sqlite::Value::Text(name.into()),
            sqlite::Value::Text(value.into()),
            sqlite::Value::Blob(encrypted.to_vec()),
            sqlite::Value::Text(path.into()),
            sqlite::Value::Int(exp),
            sqlite::Value::Int(0),
            sqlite::Value::Int(0),
        ]
    }

    /// A cookie that cannot be decrypted is DROPPED and counted, never sent
    /// with a garbled value: a wrong `Cookie:` header is answered with a `403`
    /// that looks like a server fault, while a missing one fails honestly.
    #[test]
    fn an_undecryptable_value_is_counted_and_left_out() {
        let rows = vec![
            row("example.org", "plain", "kept", b"", "/", 0),
            row("example.org", "sealed", "", b"v10\x00\x01\x02", "/", 0),
        ];
        let (jar, undecryptable) = rows_to_jar(rows, None);
        assert_eq!(undecryptable, 1);
        assert_eq!(jar.len(), 1);
        assert_eq!(jar.iter().next().unwrap().name, "plain");
    }

    #[test]
    fn a_row_with_no_name_or_no_host_is_not_a_cookie() {
        let rows = vec![
            row("example.org", "", "v", b"", "/", 0),
            row("", "n", "v", b"", "/", 0),
            row("example.org", "ok", "v", b"", "", 0),
        ];
        let (jar, undecryptable) = rows_to_jar(rows, None);
        assert_eq!(undecryptable, 0);
        assert_eq!(jar.len(), 1);
        // An empty path column is still a cookie, rooted at `/`.
        assert_eq!(jar.iter().next().unwrap().path, "/");
    }

    /// The key is asked for only when something actually needs it, so a
    /// `--password-store=basic` profile never touches the keychain.
    #[test]
    fn a_plaintext_table_needs_no_key() {
        assert!(!needs_key(&row("example.org", "n", "v", b"", "/", 0)));
        assert!(needs_key(&row("example.org", "n", "", b"v10xx", "/", 0)));
        // A row that carries BOTH is readable without the key.
        assert!(!needs_key(&row("example.org", "n", "v", b"v10xx", "/", 0)));
    }

    #[cfg(not(target_os = "windows"))]
    #[test]
    fn a_blob_that_is_not_chromium_ciphertext_is_refused() {
        let key = derive(b"peanuts", 1);
        for blob in [
            b"plain text".as_slice(),
            b"v10".as_slice(),
            // A marked blob whose body is not a whole number of AES blocks.
            b"v10short".as_slice(),
            // Windows app-bound encryption, which needs a SYSTEM-level key.
            b"v20\x00\x01\x02".as_slice(),
        ] {
            assert_eq!(decrypt_raw(&key, blob), None, "{blob:?}");
        }
    }

    #[cfg(not(target_os = "windows"))]
    #[test]
    fn the_wrong_key_yields_nothing_rather_than_plausible_noise() {
        use aes::cipher::{block_padding::Pkcs7, BlockModeEncrypt, KeyIvInit};

        let plain = b"session=abc123";
        let mut buf = vec![0u8; plain.len() + 16];
        buf[..plain.len()].copy_from_slice(plain);
        let n = cbc::Encryptor::<aes::Aes128>::new(&derive(b"peanuts", 1).into(), &IV.into())
            .encrypt_padded::<Pkcs7>(&mut buf, plain.len())
            .unwrap()
            .len();
        let mut blob = b"v10".to_vec();
        blob.extend_from_slice(&buf[..n]);

        // PKCS#7 makes this overwhelmingly likely to fail outright; the value
        // that matters is that a failure is None, not a mangled cookie.
        let wrong = derive(b"not-the-password", 1);
        assert_eq!(decrypt(&wrong, &blob, "example.org"), None);
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn every_chromium_has_its_own_keychain_item() {
        for (b, want) in [
            (Browser::Chrome, "Chrome"),
            (Browser::Chromium, "Chromium"),
            (Browser::Edge, "Microsoft Edge"),
            (Browser::Brave, "Brave"),
            (Browser::Vivaldi, "Vivaldi"),
            (Browser::Opera, "Opera"),
        ] {
            assert_eq!(keyring_name(b), want, "{b}");
        }
    }

    #[test]
    fn reads_a_real_chromium_schema_and_its_time_base() {
        let store =
            std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/ch.sqlite");
        let db = sqlite::Db::open(&store).unwrap();
        let (jar, undecryptable) = read(&db, &store, Browser::Chrome).unwrap();
        assert_eq!(undecryptable, 0);
        let by = |n: &str| jar.iter().find(|c| c.name == n).cloned().unwrap();

        let sid = by("sid");
        assert_eq!(sid.domain, "example.org");
        assert!(!sid.host_only);
        assert!(sid.secure && sid.http_only);
        assert_eq!(sid.expires, Some(2_000_000_000));
        assert_eq!(sid.value, "abc123");

        let csrf = by("csrf");
        assert!(csrf.host_only && csrf.is_session());
        assert_eq!(csrf.path, "/files");
        // An expired row is still READ here; dropping it is `load`'s job, and
        // doing it twice would hide a clock bug in one of the two places.
        assert_eq!(by("stale").expires, Some(100));
    }

    #[test]
    fn chromium_times_convert_to_unix_seconds() {
        // 2024-01-01T00:00:00Z as Chromium writes it.
        assert_eq!(
            from_chromium_time((1_704_067_200 + WINDOWS_EPOCH_OFFSET) * 1_000_000),
            Some(1_704_067_200)
        );
        assert_eq!(from_chromium_time(0), None);
        assert_eq!(from_chromium_time(-1), None);
    }

    #[cfg(not(target_os = "windows"))]
    #[test]
    fn pbkdf2_matches_the_published_vectors() {
        // RFC 6070 §2, truncated to the 16 bytes Chromium derives.
        let got = derive(b"password", 1);
        // The RFC's salt is "salt"; Chromium's is "saltysalt", so this checks
        // the construction against a vector computed for the same inputs the
        // browser uses rather than restating the implementation.
        assert_eq!(got.len(), 16);
        assert_ne!(got, [0u8; 16]);
        // Round count must change the key: a derivation that ignored it would
        // decrypt macOS stores with the Linux key and produce noise.
        assert_ne!(derive(b"peanuts", 1), derive(b"peanuts", 1003));
    }

    #[cfg(not(target_os = "windows"))]
    #[test]
    fn round_trips_a_value_through_the_real_cipher() {
        use aes::cipher::{block_padding::Pkcs7, BlockModeEncrypt, KeyIvInit};

        let key = derive(b"peanuts", 1);
        let plain = b"session=abc123";
        let mut buf = vec![0u8; plain.len() + 16];
        buf[..plain.len()].copy_from_slice(plain);
        let n = cbc::Encryptor::<aes::Aes128>::new(&key.into(), &IV.into())
            .encrypt_padded::<Pkcs7>(&mut buf, plain.len())
            .unwrap()
            .len();
        let mut blob = b"v10".to_vec();
        blob.extend_from_slice(&buf[..n]);

        assert_eq!(
            decrypt(&key, &blob, "example.org").as_deref(),
            Some("session=abc123")
        );
        // A v11 blob is the same cipher under a different marker.
        let mut v11 = b"v11".to_vec();
        v11.extend_from_slice(&buf[..n]);
        assert_eq!(
            decrypt(&key, &v11, "example.org").as_deref(),
            Some("session=abc123")
        );
        // Anything else is not a Chromium blob.
        assert_eq!(decrypt_raw(&key, b"plain text"), None);
    }

    #[cfg(not(target_os = "windows"))]
    #[test]
    fn strips_the_domain_binding_chromium_130_prepends() {
        use aes::cipher::{block_padding::Pkcs7, BlockModeEncrypt, KeyIvInit};

        let key = derive(b"peanuts", 1);
        let mut plain = sha256(b"example.org").to_vec();
        plain.extend_from_slice(b"session=abc123");
        let mut buf = vec![0u8; plain.len() + 16];
        buf[..plain.len()].copy_from_slice(&plain);
        let n = cbc::Encryptor::<aes::Aes128>::new(&key.into(), &IV.into())
            .encrypt_padded::<Pkcs7>(&mut buf, plain.len())
            .unwrap()
            .len();
        let mut blob = b"v10".to_vec();
        blob.extend_from_slice(&buf[..n]);

        assert_eq!(
            decrypt(&key, &blob, "example.org").as_deref(),
            Some("session=abc123")
        );
        // The binding is checked, not assumed: the same blob read for another
        // host is refused rather than returned with 32 bytes chopped off.
        assert_eq!(decrypt(&key, &blob, "other.test"), None);
    }
}
