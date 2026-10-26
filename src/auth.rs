//! Auth resolution, mirroring the Python original's tiers.
//!
//! Order: env vars -> auth.json cache -> Chromium cookie stores.
//! Cookie values are kept in memory only. Nothing in this module prints,
//! logs, or persists a cookie value (auth.json is only written by the
//! explicit `--init` path in main, with the same two fields the Python
//! original writes).

use std::path::{Path, PathBuf};

use aes::Aes128;
use aes_gcm::{aead::Aead, Aes128Gcm, KeyInit};
use base64::{engine::general_purpose::STANDARD as B64, Engine};
use cbc::cipher::generic_array::GenericArray;
use cbc::cipher::{BlockDecryptMut, KeyIvInit};
use cbc::Decryptor;
use pbkdf2::pbkdf2_hmac;
use rusqlite::Connection;
use sha1::Sha1;

pub const BROWSER_UA: &str = "Mozilla/5.0 (X11; Linux x86_64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/145.0.0.0 Safari/537.36";

#[derive(Debug, Clone, Default)]
pub struct Auth {
    pub sid: String,
    pub ts: Option<String>,
    /// Full .google.com cookie jar when one was available (browser tier).
    /// The SID/TS pair always wins over jar entries of the same name.
    pub jar: Vec<(String, String)>,
    pub source: String,
}

impl Auth {
    pub fn cookie_header(&self) -> String {
        let mut parts: Vec<String> = Vec::new();
        for (k, v) in &self.jar {
            if k == "__Secure-1PSID" || k == "__Secure-1PSIDTS" {
                continue;
            }
            parts.push(format!("{}={}", k, v));
        }
        parts.push(format!("__Secure-1PSID={}", self.sid));
        if let Some(ts) = &self.ts {
            if !ts.is_empty() {
                parts.push(format!("__Secure-1PSIDTS={}", ts));
            }
        }
        parts.join("; ")
    }
}

#[derive(Debug, Clone)]
pub struct AuthCandidate {
    pub source: String,
    pub has_ts: bool,
    /// Number of cookies in the full jar, when a jar was available.
    pub jar_cookies: Option<usize>,
    pub live: bool,
}

pub fn home_dir() -> PathBuf {
    std::env::var("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from("/root"))
}

pub fn auth_cache_path(home: &Path) -> PathBuf {
    home.join(".gemini-cli").join("auth.json")
}

pub fn load_auth_cache(home: &Path) -> Option<(String, Option<String>)> {
    let text = std::fs::read_to_string(auth_cache_path(home)).ok()?;
    let v: serde_json::Value = serde_json::from_str(&text).ok()?;
    let sid = v
        .get("__Secure-1PSID")
        .or_else(|| v.get("sid"))
        .and_then(|x| x.as_str())
        .filter(|s| !s.is_empty())?
        .to_string();
    let ts = v
        .get("__Secure-1PSIDTS")
        .or_else(|| v.get("ts"))
        .and_then(|x| x.as_str())
        .filter(|s| !s.is_empty())
        .map(|s| s.to_string());
    Some((sid, ts))
}

pub fn save_auth_cache(home: &Path, sid: &str, ts: Option<&str>) -> std::io::Result<()> {
    let dir = home.join(".gemini-cli");
    std::fs::create_dir_all(&dir)?;
    let obj = serde_json::json!({
        "__Secure-1PSID": sid,
        "__Secure-1PSIDTS": ts.unwrap_or(""),
        "updated": crate::util::iso_now(),
    });
    std::fs::write(auth_cache_path(home), serde_json::to_string(&obj).unwrap())
}

// ── Chromium cookie decryption ─────────────────────────────────────
// Chrome on Linux stores v10 cookies. Two shapes exist:
//  * AES-128-CBC with PBKDF2-SHA1("peanuts", "saltysalt", 1) and a
//    16-space IV (legacy / no-keyring profiles).
//  * AES-128-GCM with a key from Local State's os_crypt.encrypted_key
//    (DPAPI prefix stripped, CBC-peanuts decrypted). If no such key is
//    present (or the keyring password differs), GCM decryption fails and
//    the tier simply yields nothing — the caller falls through to the
//    documented re-sign-in path. Decrypted values are never printed.

fn printable_ascii(b: &[u8]) -> bool {
    !b.is_empty() && b.iter().all(|c| (32..=126).contains(c))
}

fn decrypt_cbc_peanuts(data: &[u8]) -> Option<Vec<u8>> {
    let mut key = [0u8; 16];
    pbkdf2_hmac::<Sha1>(b"peanuts", b"saltysalt", 1, &mut key);
    let iv = [b' '; 16];
    let dec = Decryptor::<Aes128>::new(GenericArray::from_slice(&key), GenericArray::from_slice(&iv));
    let out = dec.decrypt_padded_vec_mut::<cbc::cipher::block_padding::Pkcs7>(data).ok()?;
    if printable_ascii(&out) {
        Some(out)
    } else {
        None
    }
}

fn local_state_gcm_key(profile_dir: &Path) -> Option<[u8; 16]> {
    let text = std::fs::read_to_string(profile_dir.join("Local State")).ok()?;
    let v: serde_json::Value = serde_json::from_str(&text).ok()?;
    let enc = v.get("os_crypt")?.get("encrypted_key")?.as_str()?;
    let raw = B64.decode(enc).ok()?;
    if raw.len() <= 5 || &raw[..5] != b"DPAPI" {
        return None;
    }
    let dec = decrypt_cbc_peanuts(&raw[5..])?;
    if dec.len() == 16 {
        let mut k = [0u8; 16];
        k.copy_from_slice(&dec);
        Some(k)
    } else {
        None
    }
}

fn decrypt_gcm(data: &[u8], key: &[u8; 16]) -> Option<Vec<u8>> {
    if data.len() <= 12 + 16 {
        return None;
    }
    let cipher = Aes128Gcm::new_from_slice(key).ok()?;
    let out = cipher
        .decrypt(data[..12].into(), &data[12..])
        .ok()?;
    if printable_ascii(&out) {
        Some(out)
    } else {
        None
    }
}

/// Decrypt one Chromium `encrypted_value` (v10 prefix stripped by caller
/// contract: pass the raw blob). Returns the plaintext string, or None.
pub fn decrypt_chromium_value(blob: &[u8], profile_dir: &Path) -> Option<String> {
    if blob.len() < 4 || &blob[..3] != b"v10" {
        return None;
    }
    let data = &blob[3..];
    if let Some(pt) = decrypt_cbc_peanuts(data) {
        return String::from_utf8(pt).ok();
    }
    if let Some(key) = local_state_gcm_key(profile_dir) {
        if let Some(pt) = decrypt_gcm(data, &key) {
            return String::from_utf8(pt).ok();
        }
    }
    None
}

fn chromium_profiles(home: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    for base in [
        home.join(".gemini-cli").join("cr-profile"),
        home.join(".config").join("google-chrome"),
        home.join(".config").join("chromium"),
    ] {
        if base.is_dir() {
            out.push(base);
        }
    }
    out
}

/// Scan Chromium cookie stores. Returns (sid, ts, full jar) for the first
/// profile whose .google.com jar decrypts to a SID, plus the jar size.
pub fn scan_chromium(home: &Path) -> Option<(String, Option<String>, Vec<(String, String)>)> {
    for profile in chromium_profiles(home) {
        let mut dbs: Vec<PathBuf> = Vec::new();
        let def = profile.join("Default").join("Cookies");
        if def.exists() {
            dbs.push(def);
        }
        if let Ok(rd) = std::fs::read_dir(&profile) {
            for e in rd.flatten() {
                let name = e.file_name().to_string_lossy().to_string();
                if name.starts_with("Profile ") {
                    let c = e.path().join("Cookies");
                    if c.exists() {
                        dbs.push(c);
                    }
                }
            }
        }
        for db in dbs {
            let uri = format!("file:{}?mode=ro", db.to_string_lossy());
            let conn = match Connection::open_with_flags(
                &uri,
                rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY | rusqlite::OpenFlags::SQLITE_OPEN_URI,
            ) {
                Ok(c) => c,
                Err(_) => continue,
            };
            let mut stmt = match conn.prepare(
                "SELECT name, encrypted_value FROM cookies WHERE host_key LIKE '%google.com'",
            ) {
                Ok(s) => s,
                Err(_) => continue,
            };
            let rows = match stmt.query_map([], |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, Vec<u8>>(1)?,
                ))
            }) {
                Ok(rows) => rows,
                Err(_) => continue,
            };
            let mut jar: Vec<(String, String)> = Vec::new();
            for row in rows.flatten() {
                if let Some(val) = decrypt_chromium_value(&row.1, &profile) {
                    jar.push((row.0, val));
                }
            }
            let sid = jar
                .iter()
                .find(|(k, _)| k == "__Secure-1PSID")
                .map(|(_, v)| v.clone());
            if let Some(sid) = sid {
                let ts = jar
                    .iter()
                    .find(|(k, _)| k == "__Secure-1PSIDTS")
                    .map(|(_, v)| v.clone());
                return Some((sid, ts, jar));
            }
        }
    }
    None
}

/// Cheap real check (Python `_validate_auth`): does /app hand back a
/// session token for this SID/TS pair? `base` is the Gemini origin
/// (https://gemini.google.com, or a mock base in tests).
pub fn validate_auth(base: &str, sid: &str, ts: Option<&str>, jar: &[(String, String)]) -> bool {
    if sid.is_empty() {
        return false;
    }
    let auth = Auth {
        sid: sid.to_string(),
        ts: ts.map(|s| s.to_string()),
        jar: jar.to_vec(),
        source: String::new(),
    };
    let client = match crate::client::build_client(20) {
        Ok(c) => c,
        Err(_) => return false,
    };
    let url = format!("{}/app", base.trim_end_matches('/'));
    let resp = client
        .get(&url)
        .header("User-Agent", BROWSER_UA)
        .header("Cookie", auth.cookie_header())
        .header("Accept-Language", "en")
        .send();
    match resp {
        Ok(r) => r
            .text()
            .map(|t| t.contains("SNlM0e"))
            .unwrap_or(false),
        Err(_) => false,
    }
}

/// Resolve auth, validating each candidate against /app. Returns the
/// first live candidate, or an error naming the tiers that were tried
/// (never any credential value).
pub fn resolve_auth(home: &Path, base: &str) -> Result<Auth, String> {
    let mut tried: Vec<String> = Vec::new();
    if let (Ok(sid), ts) = (
        std::env::var("GEMINI_SID"),
        std::env::var("GEMINI_TS").ok().filter(|s| !s.is_empty()),
    ) {
        if !sid.is_empty() {
            tried.push("env".to_string());
            if validate_auth(base, &sid, ts.as_deref(), &[]) {
                return Ok(Auth {
                    sid,
                    ts,
                    jar: Vec::new(),
                    source: "env".to_string(),
                });
            }
        }
    }
    if let Some((sid, ts)) = load_auth_cache(home) {
        tried.push("auth.json".to_string());
        if validate_auth(base, &sid, ts.as_deref(), &[]) {
            return Ok(Auth {
                sid,
                ts,
                jar: Vec::new(),
                source: "auth.json".to_string(),
            });
        }
    }
    if let Some((sid, ts, jar)) = scan_chromium(home) {
        tried.push("chromium".to_string());
        if validate_auth(base, &sid, ts.as_deref(), &jar) {
            return Ok(Auth {
                sid,
                ts,
                jar,
                source: "chromium".to_string(),
            });
        }
    }
    Err(format!(
        "AUTH_EXPIRED: no live Gemini session (tried: {}). Set GEMINI_SID/GEMINI_TS, run --init after signing in at gemini.google.com, or re-sign in.",
        if tried.is_empty() {
            "none found".to_string()
        } else {
            tried.join(", ")
        }
    ))
}

/// Doctor-style candidate report: existence/liveness only, no values.
pub fn doctor_candidates(home: &Path, base: &str) -> Vec<AuthCandidate> {
    let mut out = Vec::new();
    if let Ok(sid) = std::env::var("GEMINI_SID") {
        if !sid.is_empty() {
            let ts = std::env::var("GEMINI_TS").ok().filter(|s| !s.is_empty());
            let live = validate_auth(base, &sid, ts.as_deref(), &[]);
            out.push(AuthCandidate {
                source: "env".to_string(),
                has_ts: ts.is_some(),
                jar_cookies: None,
                live,
            });
        }
    }
    if let Some((sid, ts)) = load_auth_cache(home) {
        let live = validate_auth(base, &sid, ts.as_deref(), &[]);
        out.push(AuthCandidate {
            source: "auth.json".to_string(),
            has_ts: ts.is_some(),
            jar_cookies: None,
            live,
        });
    }
    if let Some((sid, ts, jar)) = scan_chromium(home) {
        let live = validate_auth(base, &sid, ts.as_deref(), &jar);
        out.push(AuthCandidate {
            source: "chromium".to_string(),
            has_ts: ts.is_some(),
            jar_cookies: Some(jar.len()),
            live,
        });
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cookie_header_prefers_pair() {
        let a = Auth {
            sid: "SID".into(),
            ts: Some("TS".into()),
            jar: vec![
                ("NID".into(), "nid".into()),
                ("__Secure-1PSID".into(), "OLD".into()),
            ],
            source: "test".into(),
        };
        let h = a.cookie_header();
        assert!(h.contains("__Secure-1PSID=SID"));
        assert!(!h.contains("OLD"));
        assert!(h.contains("NID=nid"));
    }

    #[test]
    fn cbc_round_trip() {
        // Encrypt with the same CBC-peanuts scheme, then decrypt.
        use cbc::cipher::{BlockEncryptMut, KeyIvInit};
        use cbc::Encryptor;
        let mut key = [0u8; 16];
        pbkdf2_hmac::<Sha1>(b"peanuts", b"saltysalt", 1, &mut key);
        let iv = [b' '; 16];
        let enc = Encryptor::<Aes128>::new(GenericArray::from_slice(&key), GenericArray::from_slice(&iv));
        let ct = enc.encrypt_padded_vec_mut::<cbc::cipher::block_padding::Pkcs7>(b"test-cookie-value");
        let mut blob = b"v10".to_vec();
        blob.extend_from_slice(&ct);
        let dir = std::env::temp_dir();
        assert_eq!(
            decrypt_chromium_value(&blob, &dir).as_deref(),
            Some("test-cookie-value")
        );
    }
}
