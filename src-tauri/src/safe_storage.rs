//! Decrypt Cursor's stored login tokens.
//!
//! Newer Cursor builds keep `cursorAuth/accessToken` as
//! `cursor-safe-storage:v1:<base64>`, where the base64 payload is Electron
//! safeStorage (Chromium os_crypt) output. Older builds store the token in
//! plain text, which is returned unchanged.
//!
//! The decrypted token is only ever held in memory. Nothing here writes it.
//!
//! - Windows: AES-256-GCM. The key is in `<data dir>/Local State`
//!   (`os_crypt.encrypted_key`), protected with DPAPI for the current user.
//!   Each Cursor data folder (so each Multi Cursor environment) has its own key.
//! - macOS: AES-128-CBC. The key is derived from the "Cursor Safe Storage"
//!   Keychain password; macOS asks once to allow access.
//! - Linux: AES-128-CBC. `v11` uses a password from the desktop keyring
//!   (GNOME Keyring via `secret-tool`, or KWallet via `kwallet-query`);
//!   `v10` uses Chromium's fixed fallback.
//!
//! Fetching the key can show a permission prompt (macOS Keychain, KWallet).
//! So the key is fetched at most once per app session and shared by all
//! accounts, fetches are serialized (several accounts loading at once cause
//! one prompt, not several), and a failure, such as the user clicking Deny,
//! is remembered until the user explicitly retries.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};

use base64::Engine;

const WRAPPER_PREFIX: &str = "cursor-safe-storage:";

#[derive(Debug, Clone, PartialEq)]
pub enum TokenError {
    /// The decryption key could not be obtained (prompt denied, keyring
    /// missing or locked). Remembered for the session; pass `retry_key` to
    /// try again.
    KeyUnavailable(String),
    /// The stored value could not be decrypted or parsed.
    Invalid(String),
}

impl std::fmt::Display for TokenError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            TokenError::KeyUnavailable(e) | TokenError::Invalid(e) => f.write_str(e),
        }
    }
}

/// Plain-text token, or the decrypted value of a `cursor-safe-storage:` one.
/// `data_dir` is the Cursor user-data folder the value came from.
/// `retry_key` forgets a remembered key failure and asks again.
pub fn decrypt_token(value: &str, data_dir: &Path, retry_key: bool) -> Result<String, TokenError> {
    let value = value.trim();
    let Some(rest) = value.strip_prefix(WRAPPER_PREFIX) else {
        return Ok(value.to_string());
    };
    let (version, payload) = rest
        .split_once(':')
        .ok_or_else(|| TokenError::Invalid("Malformed encrypted token".to_string()))?;
    if version != "v1" {
        return Err(TokenError::Invalid(format!(
            "Unsupported token encryption {version}"
        )));
    }
    let blob = base64::engine::general_purpose::STANDARD
        .decode(payload)
        .map_err(|e| TokenError::Invalid(format!("Malformed encrypted token: {e}")))?;
    let plain = decrypt_os_crypt(&blob, data_dir, retry_key)?;
    String::from_utf8(plain)
        .map_err(|_| TokenError::Invalid("Decrypted token is not text".to_string()))
}

enum KeyState {
    /// `recheck`: a decrypt failure may still mean the key went stale. Cleared
    /// after one refetch that didn't help, so a single bad token can't keep
    /// re-prompting; `retry_key` sets it again.
    Ready {
        key: Vec<u8>,
        recheck: bool,
    },
    Failed(String),
}

fn key_cache() -> &'static Mutex<HashMap<PathBuf, KeyState>> {
    static CACHE: OnceLock<Mutex<HashMap<PathBuf, KeyState>>> = OnceLock::new();
    CACHE.get_or_init(|| Mutex::new(HashMap::new()))
}

/// Run `decrypt` with the session's key for `scope`, fetching it first if
/// needed. The lock is held while fetching, so concurrent callers wait for
/// one prompt instead of each showing their own.
fn with_key(
    scope: PathBuf,
    retry_key: bool,
    fetch: impl Fn() -> Result<Vec<u8>, String>,
    decrypt: impl Fn(&[u8]) -> Result<Vec<u8>, String>,
) -> Result<Vec<u8>, TokenError> {
    let mut cache = key_cache().lock().unwrap_or_else(|e| e.into_inner());
    // A retry forgets a failure, but keeps a working key (no new prompt).
    if retry_key {
        if matches!(cache.get(&scope), Some(KeyState::Failed(_))) {
            cache.remove(&scope);
        } else if let Some(KeyState::Ready { recheck, .. }) = cache.get_mut(&scope) {
            *recheck = true;
        }
    }

    let (key, recheck) = match cache.get(&scope) {
        Some(KeyState::Ready { key, recheck }) => (key.clone(), *recheck),
        Some(KeyState::Failed(e)) => return Err(TokenError::KeyUnavailable(e.clone())),
        None => {
            return match fetch() {
                Ok(key) => {
                    let result = decrypt(&key).map_err(TokenError::Invalid);
                    cache.insert(scope, KeyState::Ready { key, recheck: true });
                    result
                }
                Err(e) => {
                    cache.insert(scope, KeyState::Failed(e.clone()));
                    Err(TokenError::KeyUnavailable(e))
                }
            };
        }
    };
    let err = match decrypt(&key) {
        Ok(plain) => return Ok(plain),
        Err(e) => e,
    };
    if !recheck {
        return Err(TokenError::Invalid(err));
    }
    // The cached key may be stale (Cursor regenerated it): refetch once. If
    // that fails or returns the same key, the token itself is bad: keep the
    // working key for the other accounts and stop rechecking.
    match fetch() {
        Ok(fresh) if fresh != key => {
            let result = decrypt(&fresh).map_err(TokenError::Invalid);
            cache.insert(
                scope,
                KeyState::Ready {
                    key: fresh,
                    recheck: true,
                },
            );
            result
        }
        _ => {
            cache.insert(
                scope,
                KeyState::Ready {
                    key,
                    recheck: false,
                },
            );
            Err(TokenError::Invalid(err))
        }
    }
}

/// Windows keeps one key per Cursor data folder; it never prompts.
#[cfg(target_os = "windows")]
fn decrypt_os_crypt(blob: &[u8], data_dir: &Path, retry_key: bool) -> Result<Vec<u8>, TokenError> {
    with_key(
        data_dir.to_path_buf(),
        retry_key,
        || windows::master_key(data_dir),
        |key| aes_gcm_decrypt(key, blob),
    )
}

#[cfg(target_os = "macos")]
fn decrypt_os_crypt(blob: &[u8], _data_dir: &Path, retry_key: bool) -> Result<Vec<u8>, TokenError> {
    let body = strip_version(blob, &["v10"]).map_err(TokenError::Invalid)?;
    with_key(
        PathBuf::new(),
        retry_key,
        || macos::keychain_password().map(|pw| pbkdf2_key(&pw, 1003).to_vec()),
        |key| cbc_decrypt(key, body),
    )
}

#[cfg(target_os = "linux")]
fn decrypt_os_crypt(blob: &[u8], _data_dir: &Path, retry_key: bool) -> Result<Vec<u8>, TokenError> {
    if let Some(body) = blob.strip_prefix(b"v11") {
        with_key(
            PathBuf::new(),
            retry_key,
            || linux::keyring_password().map(|pw| pbkdf2_key(&pw, 1).to_vec()),
            |key| cbc_decrypt(key, body),
        )
    } else if let Some(body) = blob.strip_prefix(b"v10") {
        cbc_decrypt(&pbkdf2_key(b"peanuts", 1), body).map_err(TokenError::Invalid)
    } else {
        Err(TokenError::Invalid(
            "Unknown token encryption version".to_string(),
        ))
    }
}

#[cfg(any(target_os = "windows", target_os = "macos", test))]
fn strip_version<'a>(blob: &'a [u8], versions: &[&str]) -> Result<&'a [u8], String> {
    versions
        .iter()
        .find_map(|v| blob.strip_prefix(v.as_bytes()))
        .ok_or_else(|| "Unknown token encryption version".to_string())
}

/// Chromium Windows format: `v10` | 12-byte nonce | ciphertext + 16-byte tag.
#[cfg(any(target_os = "windows", test))]
fn aes_gcm_decrypt(key: &[u8], blob: &[u8]) -> Result<Vec<u8>, String> {
    use aes_gcm::aead::{Aead, KeyInit};
    use aes_gcm::{Aes256Gcm, Nonce};
    let body = strip_version(blob, &["v10", "v11"])?;
    if body.len() < 12 + 16 {
        return Err("Encrypted token is too short".to_string());
    }
    let (nonce, ciphertext) = body.split_at(12);
    Aes256Gcm::new_from_slice(key)
        .map_err(|_| "Invalid Cursor encryption key".to_string())?
        .decrypt(Nonce::from_slice(nonce), ciphertext)
        .map_err(|_| "Could not decrypt token (wrong key?)".to_string())
}

/// Chromium macOS/Linux key: PBKDF2-HMAC-SHA1, salt "saltysalt", 16 bytes.
#[cfg(any(target_os = "macos", target_os = "linux", test))]
fn pbkdf2_key(password: &[u8], iterations: u32) -> [u8; 16] {
    let mut key = [0u8; 16];
    pbkdf2::pbkdf2_hmac::<sha1::Sha1>(password, b"saltysalt", iterations, &mut key);
    key
}

/// Chromium macOS/Linux format: AES-128-CBC, IV of 16 spaces, PKCS#7.
#[cfg(any(target_os = "macos", target_os = "linux", test))]
fn cbc_decrypt(key: &[u8], body: &[u8]) -> Result<Vec<u8>, String> {
    use aes::cipher::{block_padding::Pkcs7, BlockDecryptMut, KeyIvInit};
    cbc::Decryptor::<aes::Aes128>::new_from_slices(key, &[b' '; 16])
        .map_err(|_| "Invalid Cursor encryption key".to_string())?
        .decrypt_padded_vec_mut::<Pkcs7>(body)
        .map_err(|_| "Could not decrypt token (wrong key?)".to_string())
}

#[cfg(target_os = "windows")]
mod windows {
    use std::path::Path;

    use base64::Engine;
    use windows_sys::Win32::Foundation::LocalFree;
    use windows_sys::Win32::Security::Cryptography::{CryptUnprotectData, CRYPT_INTEGER_BLOB};

    /// The AES key from `<data dir>/Local State`, unwrapped with DPAPI.
    pub fn master_key(data_dir: &Path) -> Result<Vec<u8>, String> {
        let path = data_dir.join("Local State");
        let raw =
            std::fs::read_to_string(&path).map_err(|e| format!("Read Cursor Local State: {e}"))?;
        let json: serde_json::Value =
            serde_json::from_str(&raw).map_err(|e| format!("Invalid Local State: {e}"))?;
        let encoded = json
            .pointer("/os_crypt/encrypted_key")
            .and_then(|v| v.as_str())
            .ok_or_else(|| "Cursor Local State has no encryption key".to_string())?;
        let wrapped = base64::engine::general_purpose::STANDARD
            .decode(encoded)
            .map_err(|e| format!("Invalid Cursor encryption key: {e}"))?;
        let dpapi = wrapped
            .strip_prefix(b"DPAPI")
            .ok_or_else(|| "Unexpected Cursor encryption key format".to_string())?;
        unprotect(dpapi)
    }

    fn unprotect(data: &[u8]) -> Result<Vec<u8>, String> {
        let input = CRYPT_INTEGER_BLOB {
            cbData: data.len() as u32,
            pbData: data.as_ptr() as *mut u8,
        };
        let mut output = CRYPT_INTEGER_BLOB {
            cbData: 0,
            pbData: std::ptr::null_mut(),
        };
        // SAFETY: input points at `data` for the call; on success Windows
        // allocates output with LocalAlloc, which we copy and then free.
        let ok = unsafe {
            CryptUnprotectData(
                &input,
                std::ptr::null_mut(),
                std::ptr::null(),
                std::ptr::null(),
                std::ptr::null(),
                0,
                &mut output,
            )
        };
        if ok == 0 || output.pbData.is_null() {
            return Err("Windows could not unlock Cursor's encryption key".to_string());
        }
        let key =
            unsafe { std::slice::from_raw_parts(output.pbData, output.cbData as usize) }.to_vec();
        unsafe { LocalFree(output.pbData as _) };
        Ok(key)
    }
}

#[cfg(target_os = "macos")]
mod macos {
    use security_framework::item::{ItemClass, ItemSearchOptions, SearchResult};

    /// Password of the "Cursor Safe Storage" Keychain item.
    pub fn keychain_password() -> Result<Vec<u8>, String> {
        let results = ItemSearchOptions::new()
            .class(ItemClass::generic_password())
            .service("Cursor Safe Storage")
            .load_data(true)
            .limit(1)
            .search()
            .map_err(|e| format!("Keychain access to Cursor Safe Storage failed: {e}"))?;
        results
            .into_iter()
            .find_map(|r| match r {
                SearchResult::Data(d) => Some(d),
                _ => None,
            })
            .ok_or_else(|| "Cursor Safe Storage not found in the Keychain".to_string())
    }
}

#[cfg(target_os = "linux")]
mod linux {
    use std::process::{Command, Stdio};
    use std::time::{Duration, Instant};

    enum Lookup {
        Found(Vec<u8>),
        /// The tool isn't installed: try the other keyring.
        NoTool,
        /// Not found, locked, or the user dismissed the prompt: stop here, so
        /// one attempt never shows more than one prompt.
        Failed(String),
    }

    /// How long to wait for the user to answer a keyring prompt. Past this
    /// the lookup counts as denied, so an ignored dialog can't hang usage
    /// loading (and the key lock) forever.
    const PROMPT_TIMEOUT: Duration = Duration::from_secs(120);

    /// Run one lookup tool once.
    fn lookup(program: &str, args: &[&str]) -> Lookup {
        let mut child = match Command::new(program)
            .args(args)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
        {
            Ok(child) => child,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Lookup::NoTool,
            Err(e) => return Lookup::Failed(format!("{program}: {e}")),
        };
        // Output is a password or a short error, well under the pipe buffer,
        // so polling before reading it can't stall the child.
        let deadline = Instant::now() + PROMPT_TIMEOUT;
        loop {
            match child.try_wait() {
                Ok(Some(_)) => break,
                Ok(None) if Instant::now() < deadline => {
                    std::thread::sleep(Duration::from_millis(100))
                }
                Ok(None) => {
                    let _ = child.kill();
                    let _ = child.wait();
                    return Lookup::Failed(format!("{program}: no answer to the keyring prompt"));
                }
                Err(e) => {
                    let _ = child.kill();
                    let _ = child.wait();
                    return Lookup::Failed(format!("{program}: {e}"));
                }
            }
        }
        let out = match child.wait_with_output() {
            Ok(out) => out,
            Err(e) => return Lookup::Failed(format!("{program}: {e}")),
        };
        let pw = String::from_utf8_lossy(&out.stdout).trim_end().to_string();
        if out.status.success() && !pw.is_empty() {
            return Lookup::Found(pw.into_bytes());
        }
        // Skip Qt's locale chatter; keep the last real message.
        let stderr = String::from_utf8_lossy(&out.stderr);
        let reason = stderr
            .lines()
            .map(str::trim)
            .rfind(|l| !l.is_empty() && !l.to_lowercase().contains("locale"))
            .unwrap_or("key not found or access denied")
            .to_string();
        Lookup::Failed(format!("{program}: {reason}"))
    }

    /// GNOME Keyring / Secret Service: schema v2, `application = Cursor`.
    fn libsecret() -> Lookup {
        lookup(
            "secret-tool",
            &[
                "lookup",
                "xdg:schema",
                "chrome_libsecret_os_crypt_password_v2",
                "application",
                "Cursor",
            ],
        )
    }

    /// KWallet 5/6: wallet `kdewallet`, folder `Chromium Keys`, entry `Chromium Safe Storage`.
    fn kwallet() -> Lookup {
        lookup(
            "kwallet-query",
            &[
                "-f",
                "Chromium Keys",
                "-r",
                "Chromium Safe Storage",
                "kdewallet",
            ],
        )
    }

    /// KDE sessions use KWallet; everything else uses the Secret Service.
    /// This mirrors how Electron picks the keyring when Cursor starts.
    pub(super) fn prefers_kwallet(desktop: Option<&str>) -> bool {
        desktop.is_some_and(|d| d.split(':').any(|p| p.eq_ignore_ascii_case("KDE")))
    }

    /// The os_crypt password Cursor stored in the desktop keyring (names
    /// verified against Cursor 3.29, Nov 2026). Uses one keyring and one
    /// lookup, so at most one prompt; the other keyring is tried only when
    /// the first one's tool isn't installed.
    pub fn keyring_password() -> Result<Vec<u8>, String> {
        let desktop = std::env::var("XDG_CURRENT_DESKTOP").ok();
        let order: [fn() -> Lookup; 2] = if prefers_kwallet(desktop.as_deref()) {
            [kwallet, libsecret]
        } else {
            [libsecret, kwallet]
        };
        for backend in order {
            match backend() {
                Lookup::Found(pw) => return Ok(pw),
                Lookup::Failed(reason) => return Err(reason),
                Lookup::NoTool => {}
            }
        }
        Err(
            "No keyring tool found (install secret-tool for GNOME Keyring \
             or kwallet-query for KWallet)"
                .to_string(),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::cell::Cell;

    fn scope(name: &str) -> PathBuf {
        PathBuf::from(format!("/test-scope/{name}"))
    }

    #[test]
    fn key_is_fetched_once_per_session() {
        let fetches = Cell::new(0);
        let fetch = || {
            fetches.set(fetches.get() + 1);
            Ok(b"key".to_vec())
        };
        let ok = |k: &[u8]| Ok(k.to_vec());
        for _ in 0..3 {
            assert_eq!(with_key(scope("once"), false, fetch, ok).unwrap(), b"key");
        }
        assert_eq!(fetches.get(), 1);
    }

    #[test]
    fn denial_is_remembered_until_retry() {
        let fetches = Cell::new(0);
        let denied = || {
            fetches.set(fetches.get() + 1);
            Err("User canceled".to_string())
        };
        let ok = |k: &[u8]| Ok(k.to_vec());
        for _ in 0..3 {
            assert_eq!(
                with_key(scope("deny"), false, denied, ok),
                Err(TokenError::KeyUnavailable("User canceled".to_string()))
            );
        }
        assert_eq!(fetches.get(), 1, "no new prompt after a denial");

        // An explicit retry asks again, and a success is then cached.
        let granted = || {
            fetches.set(fetches.get() + 1);
            Ok(b"key".to_vec())
        };
        assert!(with_key(scope("deny"), true, granted, ok).is_ok());
        assert!(with_key(scope("deny"), false, granted, ok).is_ok());
        assert_eq!(fetches.get(), 2);
    }

    #[test]
    fn stale_key_is_refetched_once() {
        let version = Cell::new(0u8);
        let fetch = || {
            version.set(version.get() + 1);
            Ok(vec![version.get()])
        };
        // Only the newest key decrypts.
        let decrypt = |k: &[u8]| {
            if k == [version.get()] {
                Ok(b"plain".to_vec())
            } else {
                Err("bad key".to_string())
            }
        };
        assert!(with_key(scope("stale"), false, fetch, decrypt).is_ok());
        version.set(version.get() + 1); // Cursor regenerated its key
        assert!(with_key(scope("stale"), false, fetch, decrypt).is_ok());
        // A freshly fetched key that fails is a bad token, not a key problem.
        let wrong = |_: &[u8]| Err("bad token".to_string());
        assert_eq!(
            with_key(scope("fresh-bad"), false, fetch, wrong),
            Err(TokenError::Invalid("bad token".to_string()))
        );
    }

    #[test]
    fn bad_token_keeps_working_key() {
        let fetches = Cell::new(0);
        let fetch = || {
            fetches.set(fetches.get() + 1);
            Ok(b"key".to_vec())
        };
        let ok = |k: &[u8]| Ok(k.to_vec());
        let bad = |_: &[u8]| Err("bad token".to_string());
        assert!(with_key(scope("bad-token"), false, fetch, ok).is_ok());
        // Same key comes back: one recheck, then no more fetches.
        for _ in 0..3 {
            assert_eq!(
                with_key(scope("bad-token"), false, fetch, bad),
                Err(TokenError::Invalid("bad token".to_string()))
            );
        }
        assert_eq!(fetches.get(), 2);
        // A failing refetch must not lock out the other accounts.
        let denied = || Err("User canceled".to_string());
        assert!(with_key(scope("bad-token"), true, denied, bad).is_err());
        assert!(with_key(scope("bad-token"), false, denied, ok).is_ok());
    }

    #[test]
    fn retry_keeps_working_key() {
        let fetches = Cell::new(0);
        let fetch = || {
            fetches.set(fetches.get() + 1);
            Ok(b"key".to_vec())
        };
        let ok = |k: &[u8]| Ok(k.to_vec());
        assert!(with_key(scope("retry-ok"), false, fetch, ok).is_ok());
        assert!(with_key(scope("retry-ok"), true, fetch, ok).is_ok());
        assert_eq!(fetches.get(), 1, "a manual refresh must not prompt again");
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn keyring_choice_follows_desktop() {
        assert!(linux::prefers_kwallet(Some("KDE")));
        assert!(linux::prefers_kwallet(Some("ubuntu:KDE")));
        assert!(!linux::prefers_kwallet(Some("ubuntu:GNOME")));
        assert!(!linux::prefers_kwallet(Some("XFCE")));
        assert!(!linux::prefers_kwallet(None));
    }

    #[test]
    fn plain_tokens_pass_through() {
        let dir = std::env::temp_dir();
        assert_eq!(
            decrypt_token(" abc.def.ghi ", &dir, false).unwrap(),
            "abc.def.ghi"
        );
    }

    #[test]
    fn rejects_malformed_wrapper() {
        let dir = std::env::temp_dir();
        assert!(decrypt_token("cursor-safe-storage:v9:AAAA", &dir, false).is_err());
        assert!(decrypt_token("cursor-safe-storage:nocolon", &dir, false).is_err());
    }

    #[test]
    fn gcm_round_trip() {
        use aes_gcm::aead::{Aead, KeyInit};
        use aes_gcm::{Aes256Gcm, Nonce};
        let key = [7u8; 32];
        let nonce = [3u8; 12];
        let ct = Aes256Gcm::new_from_slice(&key)
            .unwrap()
            .encrypt(Nonce::from_slice(&nonce), b"secret-token".as_ref())
            .unwrap();
        let mut blob = b"v10".to_vec();
        blob.extend_from_slice(&nonce);
        blob.extend_from_slice(&ct);
        assert_eq!(aes_gcm_decrypt(&key, &blob).unwrap(), b"secret-token");
        assert!(aes_gcm_decrypt(&[8u8; 32], &blob).is_err());
    }

    #[test]
    fn cbc_round_trip() {
        use aes::cipher::{block_padding::Pkcs7, BlockEncryptMut, KeyIvInit};
        let key = pbkdf2_key(b"peanuts", 1);
        let ct = cbc::Encryptor::<aes::Aes128>::new((&key).into(), &[b' '; 16].into())
            .encrypt_padded_vec_mut::<Pkcs7>(b"secret-token");
        assert_eq!(cbc_decrypt(&key, &ct).unwrap(), b"secret-token");
    }
}
