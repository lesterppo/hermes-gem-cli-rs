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

fn chromium_bases(home: &Path, edge: bool) -> Vec<PathBuf> {
    let mut out = Vec::new();
    if edge {
        let b = home.join(".config").join("microsoft-edge");
        if b.is_dir() {
            out.push(b);
        }
        return out;
    }
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

/// Scan Chromium/Edge cookie stores. Returns (sid, ts, full jar) for the
/// first profile whose .google.com jar decrypts to a SID.
fn scan_chromium_bases(home: &Path, edge: bool) -> Option<(String, Option<String>, Vec<(String, String)>)> {
    for profile in chromium_bases(home, edge) {
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

/// Scan Firefox cookie stores. Firefox on Linux stores cookies in
/// plaintext sqlite (`moz_cookies`, no v10 encryption), so no key is
/// needed. Safari has no Linux store and is skipped (Python's
/// browser_cookie3 order is chrome, firefox, edge, safari).
pub fn scan_firefox(home: &Path) -> Option<(String, Option<String>, Vec<(String, String)>)> {
    let base = home.join(".mozilla").join("firefox");
    let Ok(rd) = std::fs::read_dir(&base) else {
        return None;
    };
    for e in rd.flatten() {
        let db = e.path().join("cookies.sqlite");
        if !db.exists() {
            continue;
        }
        let uri = format!("file:{}?mode=ro", db.to_string_lossy());
        let Ok(conn) = Connection::open_with_flags(
            &uri,
            rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY | rusqlite::OpenFlags::SQLITE_OPEN_URI,
        ) else {
            continue;
        };
        let Ok(mut stmt) = conn.prepare(
            "SELECT name, value FROM moz_cookies WHERE host LIKE '%google.com'",
        ) else {
            continue;
        };
        let Ok(rows) = stmt.query_map([], |r| {
            Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?))
        }) else {
            continue;
        };
        let jar: Vec<(String, String)> = rows.flatten().collect();
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
    None
}

/// Browser scan in Python's order (chrome, firefox, edge), with the
/// preferred browser moved first when one is named. Returns
/// (source, sid, ts, jar).
pub fn scan_browsers(
    home: &Path,
    preferred: Option<&str>,
) -> Option<(String, String, Option<String>, Vec<(String, String)>)> {
    let mut order = vec!["chrome", "firefox", "edge"];
    if let Some(p) = preferred {
        let pl = p.to_lowercase();
        let key = if pl.contains("firefox") {
            "firefox"
        } else if pl.contains("edge") {
            "edge"
        } else {
            "chrome"
        };
        order.retain(|x| *x != key);
        order.insert(0, key);
    }
    for src in order {
        let found = match src {
            "firefox" => scan_firefox(home),
            "edge" => scan_chromium_bases(home, true),
            _ => scan_chromium_bases(home, false),
        };
        if let Some((sid, ts, jar)) = found {
            return Some((src.to_string(), sid, ts, jar));
        }
    }
    None
}

/// Backwards-compatible Chromium-only scan (used by tests).
pub fn scan_chromium(home: &Path) -> Option<(String, Option<String>, Vec<(String, String)>)> {
    scan_chromium_bases(home, false)
}

/// CDP cookie tier (Python gemini.py `_cdp_cookies`): live cookies
/// from an already-running CDP browser, no decryption, no browser
/// launch. Endpoint: GEMINI_CDP_URL, GEM_PW_CDP, or
/// http://127.0.0.1:9223, unless GEMINI_NO_CDP is set. Flow: GET
/// /json/version for webSocketDebuggerUrl, then WebSocket
/// Network.enable + Network.getAllCookies (8s total, matching the
/// Playwright timeout). Returns None on any error, including "no
/// CDP browser listening" (connection refused, instant).
pub fn cdp_cookies() -> Option<(String, Option<String>)> {
    if std::env::var("GEMINI_NO_CDP").is_ok() {
        return None;
    }
    let base = std::env::var("GEMINI_CDP_URL")
        .ok()
        .filter(|s| !s.is_empty())
        .or_else(|| std::env::var("GEM_PW_CDP").ok().filter(|s| !s.is_empty()))
        .unwrap_or_else(|| "http://127.0.0.1:9223".to_string());
    let base = base.trim_end_matches('/').to_string();
    // /json/version via the leyline client (plain HTTP, no auth).
    let http = match crate::client::build_client(8) {
        Ok(c) => c,
        Err(_) => return None,
    };
    let (_, version_text) = http.get(&format!("{}/json/version", base), &[]).ok()?;
    let ws_url = serde_json::from_str::<serde_json::Value>(&version_text)
        .ok()?
        .get("webSocketDebuggerUrl")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string())?;
    // WebSocket flow on a fresh runtime, bounded by an 8s timeout.
    let rt = tokio::runtime::Runtime::new().ok()?;
    rt.block_on(async {
        let fut = async {
            use futures_util::{SinkExt, StreamExt};
            let (mut ws, _) = tokio_tungstenite::connect_async(&ws_url).await.ok()?;
            let send = |id: u32, method: &str| {
                serde_json::json!({"id": id, "method": method, "params": {}}).to_string()
            };
            ws.send(tokio_tungstenite::tungstenite::Message::Text(send(1, "Network.enable").into()))
                .await
                .ok()?;
            ws.send(tokio_tungstenite::tungstenite::Message::Text(send(2, "Network.getAllCookies").into()))
                .await
                .ok()?;
            while let Some(msg) = ws.next().await {
                let text = match msg.ok()? {
                    tokio_tungstenite::tungstenite::Message::Text(t) => t.to_string(),
                    _ => continue,
                };
                let v: serde_json::Value = serde_json::from_str(&text).ok()?;
                if v.get("id").and_then(|i| i.as_u64()) != Some(2) {
                    continue;
                }
                let cookies = v.get("result")?.get("cookies")?.as_array()?;
                let mut sid = None;
                let mut ts = None;
                for c in cookies {
                    let name = c.get("name").and_then(|n| n.as_str()).unwrap_or("");
                    let domain = c.get("domain").and_then(|d| d.as_str()).unwrap_or("");
                    if !domain.contains("google.com") {
                        continue;
                    }
                    let value = c.get("value").and_then(|x| x.as_str()).unwrap_or("").to_string();
                    if name == "__Secure-1PSID" {
                        sid = Some(value);
                    } else if name == "__Secure-1PSIDTS" {
                        ts = Some(value);
                    }
                }
                return sid.map(|s| (s, ts));
            }
            None
        };
        tokio::time::timeout(std::time::Duration::from_secs(8), fut).await.ok().flatten()
    })
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
    match client.get(
        &url,
        &[
            ("User-Agent".to_string(), BROWSER_UA.to_string()),
            ("Cookie".to_string(), auth.cookie_header()),
            ("Accept-Language".to_string(), "en".to_string()),
        ],
    ) {
        Ok((_, text)) => text.contains("SNlM0e"),
        Err(_) => false,
    }
}

/// Resolve auth, validating each candidate against /app. Returns the
/// first live candidate, or an error naming the tiers that were tried
/// (never any credential value). `preferred` is the --browser /
// GEMINI_BROWSER name, moved first in the browser scan order.
pub fn resolve_auth(home: &Path, base: &str, preferred: Option<&str>) -> Result<Auth, String> {
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
    if let Some((src, sid, ts, jar)) = scan_browsers(home, preferred) {
        tried.push(src.clone());
        if validate_auth(base, &sid, ts.as_deref(), &jar) {
            return Ok(Auth {
                sid,
                ts,
                jar,
                source: src,
            });
        }
    }
    // CDP fallback (Python gemini.py ordering: local browsers first,
    // CDP after — a CDP profile can hold a dead session, so it must
    // never outrank a working local profile).
    if let Some((sid, ts)) = cdp_cookies() {
        tried.push("cdp".to_string());
        if validate_auth(base, &sid, ts.as_deref(), &[]) {
            return Ok(Auth {
                sid,
                ts,
                jar: Vec::new(),
                source: "cdp".to_string(),
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
pub fn doctor_candidates(home: &Path, base: &str, preferred: Option<&str>) -> Vec<AuthCandidate> {
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
    if let Some((src, sid, ts, jar)) = scan_browsers(home, preferred) {
        let live = validate_auth(base, &sid, ts.as_deref(), &jar);
        out.push(AuthCandidate {
            source: src,
            has_ts: ts.is_some(),
            jar_cookies: Some(jar.len()),
            live,
        });
    }
    if let Some((sid, ts)) = cdp_cookies() {
        let live = validate_auth(base, &sid, ts.as_deref(), &[]);
        out.push(AuthCandidate {
            source: "cdp".to_string(),
            has_ts: ts.is_some(),
            jar_cookies: None,
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
    fn cdp_disabled_by_env() {
        // GEMINI_NO_CDP must short-circuit before any network attempt.
        std::env::set_var("GEMINI_NO_CDP", "1");
        let r = cdp_cookies();
        std::env::remove_var("GEMINI_NO_CDP");
        assert!(r.is_none());
    }

    #[test]
    fn firefox_scan_finds_sid() {
        // Firefox cookies are plaintext sqlite; build a fake profile
        // and check the scan picks up the SID/TS pair.
        let dir = std::env::temp_dir().join(format!("gem-rs-ff-{}", std::process::id()));
        let profile = dir.join(".mozilla").join("firefox").join("test.default");
        std::fs::create_dir_all(&profile).unwrap();
        let db = profile.join("cookies.sqlite");
        let conn = Connection::open(&db).unwrap();
        conn.execute(
            "CREATE TABLE moz_cookies (host TEXT, name TEXT, value TEXT)",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO moz_cookies VALUES ('.google.com', '__Secure-1PSID', 'FFSID')",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO moz_cookies VALUES ('.google.com', '__Secure-1PSIDTS', 'FFTS')",
            [],
        )
        .unwrap();
        drop(conn);
        let found = scan_firefox(&dir).expect("firefox scan");
        assert_eq!(found.0, "FFSID");
        assert_eq!(found.1.as_deref(), Some("FFTS"));
        let _ = std::fs::remove_dir_all(&dir);
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
