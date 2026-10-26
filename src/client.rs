//! Gemini WebAPI client (leyline Chrome-impersonating transport, run
//! through a tokio Runtime so the public API stays blocking).
//!
//! Transport history: plain reqwest/rustls could not reach
//! gemini.google.com from this VM (hyper IncompleteMessage, while
//! Chrome-impersonated curl_cffi and real Chromium succeeded). Leyline
//! (Chrome 154 profile: TLS ClientHello, HTTP/2 settings, header order
//! from shipped-browser captures, BoringSSL built from source) reaches
//! /app authenticated with SNlM0e/cfb2h present, verified 2026-10-26.
//!
//! `base` is normally `https://gemini.google.com`. Tests point it at a
//! local mock server via `GeminiClient::with_base` / the
//! `GEMINI_RS_BASE_URL` env var (test-only; the CLI honours it so the
//! mock deep test can run without a live session).

use std::time::Duration;

use leyline::multipart::{Form, Part};
use leyline::{Browser, ProxyConfig, Session, TlsTrustConfig};
use serde_json::{json, Value};

use crate::auth::{Auth, BROWSER_UA};
use crate::protocol::{
    batchexecute_form, build_stream_request, default_metadata, extract_tokens,
    parse_batchexecute, parse_stream_response, BatchexecuteTokens,
};

#[derive(Debug, Clone)]
pub struct Gem {
    pub id: String,
    pub name: String,
    pub description: String,
    pub gem_type: String,
}

/// One AvailableModel, mirroring gemini-webapi's AvailableModel:
/// model_id (hex), model_name (gemini-<slug>), display_name, description,
/// capacity / capacity_field (account tier), model_number (slot 79),
/// aliases (name, display, slug forms).
#[derive(Debug, Clone)]
pub struct ModelInfo {
    pub id: String,
    pub name: String,
    pub display: String,
    pub description: String,
    pub capacity: i64,
    pub capacity_field: i64,
    pub model_number: i64,
    pub aliases: Vec<String>,
}

impl ModelInfo {
    /// Python build_model_header: '[1,null,null,null,"id",null,null,0,
    /// [4,5,6,8],null,null,<tail>, null,null,<model_number>]' where
    /// tail = "null,<capacity>" for field 13, else <capacity>.
    pub fn header_value(&self) -> String {
        let tail = if self.capacity_field == 13 {
            format!("null,{}", self.capacity)
        } else {
            self.capacity.to_string()
        };
        format!(
            "[1,null,null,null,\"{}\",null,null,0,[4,5,6,8],null,null,{}, null,null,{}]",
            self.id, tail, self.model_number
        )
    }
}

fn slug(s: &str) -> String {
    s.trim().to_lowercase().replace(' ', "-")
}

fn derive_name_aliases(id: &str, category: &str, display: &str) -> (String, Vec<String>) {
    let mut aliases: Vec<String> = Vec::new();
    if !id.is_empty() {
        aliases.push(id.to_lowercase());
    }
    let mut primary = format!("gemini-{}", id);
    let cat = category.trim();
    let disp = display.trim();
    if !cat.is_empty() {
        let cs = slug(cat);
        aliases.push(cat.to_lowercase());
        aliases.push(cs.clone());
        aliases.push(format!("gemini-{}", cs));
        // Version alias from the display name, e.g. "Gemini 3 models"
        // -> gemini-3-<slug>, as in Python's _derive_name_and_aliases.
        let major: String = disp.chars().skip_while(|c| !c.is_ascii_digit()).take_while(|c| c.is_ascii_digit()).collect();
        if !major.is_empty() {
            aliases.push(format!("gemini-{}-{}", major, cs));
        }
        primary = format!("gemini-{}", cs);
    } else if !disp.is_empty() {
        let ds = slug(disp);
        aliases.push(disp.to_lowercase());
        aliases.push(ds.clone());
        aliases.push(format!("gemini-{}", ds));
        primary = format!("gemini-{}", ds);
    }
    aliases.push(primary.clone());
    aliases.sort();
    aliases.dedup();
    (primary, aliases)
}

#[derive(Debug, Clone, Default)]
pub struct ChatResult {
    pub text: String,
    pub thoughts: String,
    pub cid: String,
    pub rid: String,
    pub rcid: String,
    pub metadata: Value,
    pub images: Vec<String>,
    pub videos: Vec<String>,
    pub completed: bool,
}

/// Blocking wrapper over a leyline Session: the session is async, the
/// runtime turns each request into a blocking call. Public API stays
/// synchronous so main.rs and the mock test are unchanged.
pub struct LeylineHttp {
    pub session: Session,
    pub rt: tokio::runtime::Runtime,
}

impl LeylineHttp {
    pub fn new(timeout_secs: u64) -> Result<Self, String> {
        let mut trust = TlsTrustConfig::new();
        // The VM's TLS-intercept CA, when present (previous Rust ports
        // on this VM needed it for the egress proxy).
        if std::path::Path::new("/etc/ssl/certs/hatch-egress-ca.pem").exists() {
            trust = trust.add_ca_file("/etc/ssl/certs/hatch-egress-ca.pem");
        }
        let session = Session::builder()
            .browser(Browser::Chrome154)
            .proxy(ProxyConfig::new().env(true))
            .tls_trust(trust)
            .timeout(Duration::from_secs(timeout_secs))
            .build()
            .map_err(|e| e.to_string())?;
        let rt = tokio::runtime::Runtime::new().map_err(|e| e.to_string())?;
        Ok(Self { session, rt })
    }

    /// Returns (status_code, body). Headers are (name, value) pairs.
    pub fn get(&self, url: &str, headers: &[(String, String)]) -> Result<(u16, String), String> {
        let session = &self.session;
        let url = url.to_string();
        let headers = headers.to_vec();
        self.rt.block_on(async move {
            let mut req = session.get(&url);
            for (k, v) in &headers {
                req = req.header(k.as_str(), v.as_str());
            }
            let resp = req.send().await.map_err(|e| e.to_string())?;
            let status = resp.status().as_u16();
            let text = resp.text().await.map_err(|e| e.to_string())?;
            Ok((status, text))
        })
    }

    /// Raw-bytes GET (media downloads); headers as (name, value) pairs.
    pub fn get_bytes(&self, url: &str, headers: &[(String, String)]) -> Result<Vec<u8>, String> {
        let session = &self.session;
        let url = url.to_string();
        let headers = headers.to_vec();
        self.rt.block_on(async move {
            let mut req = session.get(&url);
            for (k, v) in &headers {
                req = req.header(k.as_str(), v.as_str());
            }
            let resp = req.send().await.map_err(|e| e.to_string())?;
            let bytes = resp.bytes().await.map_err(|e| e.to_string())?;
            Ok(bytes.to_vec())
        })
    }

    /// Multipart POST (content-push file upload); headers as pairs.
    pub fn post_multipart(
        &self,
        url: &str,
        headers: &[(String, String)],
        form: Form,
        timeout_secs: Option<u64>,
    ) -> Result<(u16, String), String> {
        let session = &self.session;
        let url = url.to_string();
        let headers = headers.to_vec();
        self.rt.block_on(async move {
            let mut req = session.post(&url);
            for (k, v) in &headers {
                req = req.header(k.as_str(), v.as_str());
            }
            if let Some(t) = timeout_secs {
                req = req.timeout(Duration::from_secs(t));
            }
            req = req.multipart(form);
            let resp = req.send().await.map_err(|e| e.to_string())?;
            let status = resp.status().as_u16();
            let text = resp.text().await.map_err(|e| e.to_string())?;
            Ok((status, text))
        })
    }

    /// Streaming POST: calls on_chunk with each raw body chunk as it
    /// arrives (decoded, e.g. gunzip), returns (status, full body).
    pub fn post_stream(
        &self,
        url: &str,
        headers: &[(String, String)],
        form: &[(String, String)],
        timeout_secs: Option<u64>,
        on_chunk: &mut dyn FnMut(&[u8]),
    ) -> Result<(u16, Vec<u8>), String> {
        let session = &self.session;
        let url = url.to_string();
        let headers = headers.to_vec();
        let form = form.to_vec();
        self.rt.block_on(async move {
            use futures_util::StreamExt;
            let mut req = session.post(&url).stream();
            for (k, v) in &headers {
                req = req.header(k.as_str(), v.as_str());
            }
            if let Some(t) = timeout_secs {
                req = req.timeout(Duration::from_secs(t));
            }
            req = req.form(form);
            let resp = req.send().await.map_err(|e| e.to_string())?;
            let status = resp.status().as_u16();
            let mut stream = resp.into_decoded_stream(None).map_err(|e| e.to_string())?;
            let mut full: Vec<u8> = Vec::new();
            while let Some(chunk) = stream.next().await {
                let bytes = chunk.map_err(|e| e.to_string())?;
                on_chunk(&bytes);
                full.extend_from_slice(&bytes);
            }
            Ok((status, full))
        })
    }

    pub fn post_form(
        &self,
        url: &str,
        headers: &[(String, String)],
        form: &[(String, String)],
        timeout_secs: Option<u64>,
    ) -> Result<(u16, String), String> {
        let session = &self.session;
        let url = url.to_string();
        let headers = headers.to_vec();
        let form = form.to_vec();
        self.rt.block_on(async move {
            let mut req = session.post(&url);
            for (k, v) in &headers {
                req = req.header(k.as_str(), v.as_str());
            }
            if let Some(t) = timeout_secs {
                req = req.timeout(Duration::from_secs(t));
            }
            req = req.form(form);
            let resp = req.send().await.map_err(|e| e.to_string())?;
            let status = resp.status().as_u16();
            let text = resp.text().await.map_err(|e| e.to_string())?;
            Ok((status, text))
        })
    }
}

/// Build the leyline transport: Chrome 154 profile, env proxy, the
/// VM's TLS-intercept CA when present, with the proxy env already
/// sanitised by the caller (see util::sanitize_no_proxy).
pub fn build_client(timeout_secs: u64) -> Result<LeylineHttp, String> {
    LeylineHttp::new(timeout_secs)
}

pub struct GeminiClient {
    pub auth: Auth,
    pub tokens: BatchexecuteTokens,
    pub base: String,
    pub http: LeylineHttp,
    pub reqid: i64,
}

fn json_strings(v: &Value, out: &mut Vec<String>) {
    match v {
        Value::String(s) => out.push(s.clone()),
        Value::Array(a) => {
            for x in a {
                json_strings(x, out);
            }
        }
        Value::Object(o) => {
            for (_, x) in o {
                json_strings(x, out);
            }
        }
        _ => {}
    }
}

impl GeminiClient {
    pub fn new(auth: Auth, timeout_secs: u64) -> Result<Self, String> {
        Self::with_base(auth, timeout_secs, "https://gemini.google.com".to_string())
    }

    pub fn with_base(auth: Auth, timeout_secs: u64, base: String) -> Result<Self, String> {
        Ok(Self {
            auth,
            tokens: BatchexecuteTokens::default(),
            base: base.trim_end_matches('/').to_string(),
            http: build_client(timeout_secs)?,
            reqid: 100000,
        })
    }

    fn cookie(&self) -> String {
        self.auth.cookie_header()
    }

    /// GET /app and extract init tokens. A missing SNlM0e is
    /// AUTH_EXPIRED, exactly like the Python original.
    pub fn init(&mut self) -> Result<(), String> {
        let url = format!("{}/app", self.base);
        let (status, html) = self.http.get(
            &url,
            &[
                ("User-Agent".to_string(), BROWSER_UA.to_string()),
                ("Cookie".to_string(), self.cookie()),
                ("Accept-Language".to_string(), "en".to_string()),
            ],
        )?;
        if !(200..300).contains(&status) {
            return Err(format!("AUTH_EXPIRED: /app returned HTTP {}", status));
        }
        let toks = extract_tokens(&html);
        if toks.at.is_none() {
            return Err(
                "AUTH_EXPIRED: no SNlM0e in /app HTML (not signed in?)".to_string(),
            );
        }
        self.tokens = toks;
        Ok(())
    }

    fn batch_headers(&self) -> Vec<(String, String)> {
        vec![
            ("Content-Type".into(), "application/x-www-form-urlencoded;charset=UTF-8".into()),
            ("Origin".into(), "https://gemini.google.com".into()),
            ("Referer".into(), "https://gemini.google.com/app".into()),
            ("X-Same-Domain".into(), "1".into()),
            ("User-Agent".into(), BROWSER_UA.into()),
        ]
    }

    /// Browser-shaped batchexecute RPC (mirrors BrowserBatchRPC.rpc).
    pub fn rpc(&mut self, rpcid: &str, payload: &str) -> Result<String, String> {
        let form = batchexecute_form(rpcid, payload, &self.tokens);
        let mut headers = vec![("Cookie".to_string(), self.cookie())];
        headers.extend(self.batch_headers());
        let form_vec: Vec<(String, String)> = form
            .into_iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect();
        let (status, text) = self.http.post_form(
            &format!("{}/_/BardChatUi/data/batchexecute", self.base),
            &headers,
            &form_vec,
            None,
        )?;
        if !(200..300).contains(&status) || text[..text.len().min(2000)].contains("[\"e\",4") {
            return Err(format!("RPC {} failed: HTTP {}", rpcid, status));
        }
        Ok(text)
    }

    pub fn rpc_payloads(&mut self, rpcid: &str, payload: &str) -> Result<Vec<Value>, String> {
        let text = self.rpc(rpcid, payload)?;
        Ok(parse_batchexecute(&text, rpcid))
    }

    /// Build a StreamGenerate (url, headers, form) triple, shared by
    /// the buffered and streaming chat paths.
    #[allow(clippy::too_many_arguments)]
    fn chat_request(
        &mut self,
        prompt: &str,
        metadata: &Value,
        gem_id: Option<&str>,
        temporary: bool,
        deep_research: bool,
        extended_thinking: bool,
        model_number: Option<i64>,
        file_data: Option<Value>,
        model_header: Option<String>,
    ) -> (String, Vec<(String, String)>, Vec<(String, String)>) {
        let inner = build_stream_request(
            prompt,
            self.tokens.language.clone().as_deref().unwrap_or("en"),
            metadata,
            gem_id,
            temporary,
            deep_research,
            extended_thinking,
            model_number,
            file_data,
        );
        let reqid = self.reqid;
        self.reqid += 100000;
        let lang = self.tokens.language.clone().unwrap_or_else(|| "en".to_string());
        let mut url = format!(
            "{}/_/BardChatUi/data/assistant.lamda.BardFrontendService/StreamGenerate?hl={}&_reqid={}&rt=c",
            self.base, lang, reqid
        );
        if let Some(bl) = &self.tokens.bl {
            url.push_str(&format!("&bl={}", bl));
        }
        if let Some(sid) = &self.tokens.sid {
            url.push_str(&format!("&f.sid={}", sid));
        }
        let frep = serde_json::to_string(&vec![
            Value::Null,
            Value::String(serde_json::to_string(&inner).unwrap()),
        ])
        .unwrap();
        // Model header (Python: selected.model_header, with extended
        // flag and session id appended) — only when a model was resolved.
        let mut chat_headers: Vec<(String, String)> = vec![
            ("User-Agent".to_string(), BROWSER_UA.to_string()),
            ("Cookie".to_string(), self.cookie()),
            ("Origin".to_string(), "https://gemini.google.com".to_string()),
            ("Referer".to_string(), "https://gemini.google.com/app".to_string()),
            ("X-Same-Domain".to_string(), "1".to_string()),
            (
                "x-goog-ext-525005358-jspb".to_string(),
                "[\"\",1]".to_string(),
            ),
        ];
        if let Some(h) = &model_header {
            // Python appends [extended?2:1, sessionid] to the header list
            // before serialising; our header is already a JSON string, so
            // splice the two trailing values in before the closing bracket.
            let mut parsed: Value = serde_json::from_str(h).unwrap_or(Value::Null);
            if let Value::Array(a) = &mut parsed {
                a.push(json!(if extended_thinking { 2 } else { 1 }));
                if let Some(sid) = &self.tokens.sid {
                    a.push(json!(sid));
                }
            }
            chat_headers.push((
                "x-goog-ext-525001261-jspb".to_string(),
                serde_json::to_string(&parsed).unwrap_or_else(|_| h.clone()),
            ));
            chat_headers.push(("x-goog-ext-73010989-jspb".to_string(), "[0]".to_string()));
            chat_headers.push(("x-goog-ext-73010990-jspb".to_string(), "[0,0,0]".to_string()));
        }
        let form = vec![
            ("at".to_string(), self.tokens.at.clone().unwrap_or_default()),
            ("f.req".to_string(), frep),
        ];
        (url, chat_headers, form)
    }

    /// StreamGenerate chat. `metadata` is the conversation state (or
    /// DEFAULT_METADATA for a fresh chat); returns the final turn.
    #[allow(clippy::too_many_arguments)]
    pub fn chat(
        &mut self,
        prompt: &str,
        metadata: &Value,
        gem_id: Option<&str>,
        temporary: bool,
        deep_research: bool,
        extended_thinking: bool,
        model_number: Option<i64>,
        timeout_secs: u64,
        file_data: Option<Value>,
        model_header: Option<String>,
    ) -> Result<ChatResult, String> {
        let (url, headers, form) = self.chat_request(
            prompt, metadata, gem_id, temporary, deep_research, extended_thinking, model_number, file_data, model_header,
        );
        let (status, text) = self.http.post_form(&url, &headers, &form, Some(timeout_secs))?;
        if !(200..300).contains(&status) {
            return Err(format!("GEN_FAILED: StreamGenerate HTTP {}", status));
        }
        if text.contains("UNAUTHENTICATED") || text.contains("not authenticated") {
            return Err("AUTH_EXPIRED: StreamGenerate rejected the session".to_string());
        }
        let parsed = parse_stream_response(&text);
        if parsed.text.is_empty() && parsed.cid.is_empty() {
            return Err("GEN_FAILED: empty StreamGenerate response".to_string());
        }
        Ok(ChatResult {
            text: parsed.text,
            thoughts: parsed.thoughts,
            cid: parsed.cid,
            rid: parsed.rid,
            rcid: parsed.rcid,
            metadata: parsed.metadata,
            images: parsed.images,
            videos: parsed.videos,
            completed: parsed.completed,
        })
    }

    /// Streaming chat: same request as chat(), but each body chunk is
    /// re-parsed and only the new text suffix is handed to on_delta,
    /// exactly like gemini-webapi's text_delta. Returns the final turn.
    #[allow(clippy::too_many_arguments)]
    pub fn chat_stream(
        &mut self,
        prompt: &str,
        metadata: &Value,
        gem_id: Option<&str>,
        temporary: bool,
        deep_research: bool,
        extended_thinking: bool,
        model_number: Option<i64>,
        timeout_secs: u64,
        file_data: Option<Value>,
        model_header: Option<String>,
        on_delta: &mut dyn FnMut(&str),
    ) -> Result<ChatResult, String> {
        let (url, headers, form) = self.chat_request(
            prompt, metadata, gem_id, temporary, deep_research, extended_thinking, model_number, file_data, model_header,
        );
        let mut buf: Vec<u8> = Vec::new();
        let mut prev_text = String::new();
        let (status, full) = self.http.post_stream(&url, &headers, &form, Some(timeout_secs), &mut |chunk| {
            buf.extend_from_slice(chunk);
            let text = String::from_utf8_lossy(&buf).to_string();
            let parsed = parse_stream_response(&text);
            if !parsed.text.is_empty() && parsed.text != prev_text {
                if let Some(delta) = parsed.text.strip_prefix(&prev_text) {
                    if !delta.is_empty() {
                        on_delta(delta);
                    }
                } else {
                    // Server revised the text (rare); emit the whole
                    // new text rather than lose content.
                    on_delta(&parsed.text);
                }
                prev_text = parsed.text;
            }
        })?;
        let _ = full;
        if !(200..300).contains(&status) {
            return Err(format!("GEN_FAILED: StreamGenerate HTTP {}", status));
        }
        let text = String::from_utf8_lossy(&buf).to_string();
        if text.contains("UNAUTHENTICATED") || text.contains("not authenticated") {
            return Err("AUTH_EXPIRED: StreamGenerate rejected the session".to_string());
        }
        let parsed = parse_stream_response(&text);
        if parsed.text.is_empty() && parsed.cid.is_empty() {
            return Err("GEN_FAILED: empty StreamGenerate response".to_string());
        }
        Ok(ChatResult {
            text: parsed.text,
            thoughts: parsed.thoughts,
            cid: parsed.cid,
            rid: parsed.rid,
            rcid: parsed.rcid,
            metadata: parsed.metadata,
            images: parsed.images,
            videos: parsed.videos,
            completed: parsed.completed,
        })
    }

    /// GET_USER_STATUS (otAQ7b) -> AvailableModel objects, exactly as
    /// gemini-webapi's AvailableModel.parse_models_from_rpc: payload
    /// [15] is the model list, [16] tier flags, [17] capability flags
    /// feed compute_capacity, and from_rpc reads id [0], category [1]
    /// or [10], display [11] or [19] or [1], description [12] or [2],
    /// model_number [17] or [9] else 1.
    pub fn list_model_infos(&mut self) -> Result<Vec<ModelInfo>, String> {
        let payloads = self.rpc_payloads("otAQ7b", "[]")?;
        let mut out: Vec<ModelInfo> = Vec::new();
        for pb in &payloads {
            let Some(models) = pb.get(15).and_then(|m| m.as_array()) else {
                continue;
            };
            let tier_flags: Vec<i64> = pb
                .get(16)
                .and_then(|v| v.as_array())
                .map(|a| a.iter().filter_map(|x| x.as_i64()).collect())
                .unwrap_or_default();
            let cap_flags: Vec<i64> = pb
                .get(17)
                .and_then(|v| v.as_array())
                .map(|a| a.iter().filter_map(|x| x.as_i64()).collect())
                .unwrap_or_default();
            let (capacity, capacity_field) = if tier_flags.contains(&21) {
                (1, 13)
            } else if tier_flags.contains(&22) {
                (2, 13)
            } else if cap_flags.contains(&115) {
                (4, 12)
            } else if tier_flags.contains(&16) || cap_flags.contains(&106) {
                (3, 12)
            } else if tier_flags.contains(&8) || cap_flags.contains(&19) {
                (2, 12)
            } else {
                (1, 12)
            };
            for m in models {
                let Some(arr) = m.as_array() else { continue };
                let id = arr.get(0).and_then(|x| x.as_str()).unwrap_or("").to_string();
                if id.is_empty() {
                    continue;
                }
                let str_at = |i: usize| -> String {
                    arr.get(i).and_then(|x| x.as_str()).unwrap_or("").to_string()
                };
                let category = if !str_at(1).is_empty() { str_at(1) } else { str_at(10) };
                let display = if !str_at(11).is_empty() {
                    str_at(11)
                } else if !str_at(19).is_empty() {
                    str_at(19)
                } else {
                    str_at(1)
                };
                let description = if !str_at(12).is_empty() { str_at(12) } else { str_at(2) };
                let model_number = arr
                    .get(17)
                    .and_then(|x| x.as_i64())
                    .or_else(|| arr.get(9).and_then(|x| x.as_i64()))
                    .unwrap_or(1);
                let (name, aliases) = derive_name_aliases(&id, &category, &display);
                out.push(ModelInfo {
                    id,
                    name,
                    display: if display.is_empty() { category.clone() } else { display },
                    description,
                    capacity,
                    capacity_field,
                    model_number,
                    aliases,
                });
            }
            if !out.is_empty() {
                break;
            }
        }
        Ok(out)
    }

    /// GET_USER_STATUS -> model name strings (Python's list_models
    /// returns str(AvailableModel) == model_name for each).
    pub fn list_models(&mut self) -> Result<Vec<String>, String> {
        Ok(self
            .list_model_infos()?
            .into_iter()
            .map(|m| m.name)
            .collect())
    }

    /// Resolve a -m name against the registry, mirroring
    /// gemini-webapi's _resolve_model_by_name: exact id/name/display,
    /// then alias, then version-agnostic normalised match. Unknown
    /// names fail with the available list, as Python does.
    pub fn resolve_model(&mut self, name: &str) -> Result<ModelInfo, String> {
        let models = self.list_model_infos()?;
        let target = name.to_lowercase().trim().to_string();
        for m in &models {
            if m.id.to_lowercase() == target || m.name.to_lowercase() == target || m.display.to_lowercase() == target {
                return Ok(m.clone());
            }
        }
        for m in &models {
            if m.aliases.iter().any(|a| a == &target) {
                return Ok(m.clone());
            }
        }
        let norm = |s: &str| -> String {
            let t = s.strip_prefix("gemini-").unwrap_or(s);
            let t = t.split_once('-').map(|(_, rest)| rest).unwrap_or(t);
            t.replace('_', "-").to_string()
        };
        // Python strips the gemini-<version>- prefix; approximate with
        // the same rule applied to both sides and a token match.
        let norm_target = norm(&target);
        for m in &models {
            let nm = norm(&m.name.to_lowercase());
            if nm == norm_target || nm.split('-').any(|tok| tok == norm_target) {
                return Ok(m.clone());
            }
        }
        Err(format!(
            "Unknown model name: '{}'. Available registered models: {}",
            name,
            models.iter().map(|m| m.name.clone()).collect::<Vec<_>>().join(", ")
        ))
    }

    /// Upload one file to content-push (Python upload_file): multipart
    /// part "file", Push-ID feeds/mcudyrk2a4khkz, X-Tenant-Id
    /// bard-storage. Returns (uploaded_url, filename) for the
    /// StreamGenerate file_data slot.
    pub fn upload_file(&mut self, path: &str) -> Result<(String, String), String> {
        let data = std::fs::read(path)
            .map_err(|e| format!("FILE_NOT_FOUND: {} ({})", path, e))?;
        let filename = std::path::Path::new(path)
            .file_name()
            .and_then(|s| s.to_str())
            .unwrap_or("file")
            .to_string();
        let mime = match std::path::Path::new(path)
            .extension()
            .and_then(|s| s.to_str())
            .map(|s| s.to_lowercase())
            .as_deref()
        {
            Some("png") => "image/png",
            Some("jpg") | Some("jpeg") => "image/jpeg",
            Some("gif") => "image/gif",
            Some("webp") => "image/webp",
            Some("pdf") => "application/pdf",
            Some("txt") | Some("md") => "text/plain",
            Some("csv") => "text/csv",
            Some("json") => "application/json",
            _ => "application/octet-stream",
        };
        let part = Part::bytes(data)
            .filename(filename.clone())
            .mime(mime);
        let form = Form::new().part("file", part);
        let (status, text) = self.http.post_multipart(
            "https://content-push.googleapis.com/upload",
            &[
                ("User-Agent".to_string(), BROWSER_UA.to_string()),
                ("Cookie".to_string(), self.cookie()),
                ("Origin".to_string(), "https://gemini.google.com".to_string()),
                ("Referer".to_string(), "https://gemini.google.com/".to_string()),
                ("X-Tenant-Id".to_string(), "bard-storage".to_string()),
                // Python: init_session.push_id, falling back to the
                // default feeds/mcudyrk2a4khkz.
                ("Push-ID".to_string(), self.tokens.push_id.clone().unwrap_or_else(|| "feeds/mcudyrk2a4khkz".to_string())),
            ],
            form,
            Some(120),
        )?;
        if !(200..300).contains(&status) {
            return Err(format!("UPLOAD_FAILED: content-push HTTP {}", status));
        }
        Ok((text.trim().to_string(), filename))
    }

    /// LIST_BOTS (CNgdBe), system (3) then custom (2), as in the
    /// Python original's `_fetch_gems_rpc`.
    pub fn list_gems(&mut self) -> Result<Vec<Gem>, String> {
        let mut out: Vec<Gem> = Vec::new();
        for (kind, payload) in [("system", "[3,['en'],0]"), ("custom", "[2,['en'],0]")] {
            let payloads = self.rpc_payloads("CNgdBe", payload)?;
            for pb in &payloads {
                if let Some(gems) = pb.get(2).and_then(|g| g.as_array()) {
                    for gem in gems {
                        let id = gem.get(0).and_then(|x| x.as_str()).unwrap_or("").to_string();
                        if id.is_empty() {
                            continue;
                        }
                        let meta = gem.get(1).and_then(|x| x.as_array());
                        let name = meta
                            .and_then(|m| m.get(0))
                            .and_then(|x| x.as_str())
                            .unwrap_or("")
                            .to_string();
                        let desc = meta
                            .and_then(|m| m.get(1))
                            .and_then(|x| x.as_str())
                            .unwrap_or("")
                            .to_string();
                        out.push(Gem {
                            id,
                            name,
                            description: desc,
                            gem_type: kind.to_string(),
                        });
                    }
                }
            }
        }
        Ok(out)
    }

    pub fn gem_info(&mut self, gem_id: &str) -> Result<Gem, String> {
        let gems = self.list_gems()?;
        if let Some(g) = gems.into_iter().find(|g| g.id == gem_id) {
            return Ok(g);
        }
        Ok(Gem {
            id: gem_id.to_string(),
            name: String::new(),
            description: String::new(),
            gem_type: "external".to_string(),
        })
    }

    /// CREATE_BOT (oMH3Zd), payload shape from the Python fallback.
    pub fn create_gem(&mut self, name: &str, description: &str, prompt: &str) -> Result<String, String> {
        let payload = serde_json::to_string(&vec![vec![
            json!(name),
            json!(description),
            json!(prompt),
            Value::Null,
            Value::Null,
            Value::Null,
            Value::Null,
            Value::Null,
            json!(0),
            Value::Null,
            json!(1),
            Value::Null,
            Value::Null,
            Value::Null,
            json!([]),
        ]])
        .unwrap();
        let payloads = self.rpc_payloads("oMH3Zd", &payload)?;
        for pb in &payloads {
            if let Some(id) = pb.get(0).and_then(|x| x.as_str()) {
                return Ok(id.to_string());
            }
            if let Some(arr) = pb.as_array() {
                if let Some(id) = arr.iter().find_map(|x| x.as_str()) {
                    return Ok(id.to_string());
                }
            }
        }
        Err("GEM_CREATE_FAILED: no gem id in response".to_string())
    }

    /// DELETE_BOT (UXcSJb).
    pub fn delete_gem(&mut self, gem_id: &str) -> Result<(), String> {
        let payload = serde_json::to_string(&vec![json!(gem_id), json!([0, null, 0])]).unwrap();
        self.rpc("UXcSJb", &payload)?;
        Ok(())
    }

    /// UPDATE_BOT_METADATA (kHv0Vd), payload shape from gemini-webapi's
    /// GemMixin.update_gem: [gem_id, [name, description, prompt, null
    /// x5, 0, null, 1, null x3, [], 0]].
    pub fn update_gem(&mut self, gem_id: &str, name: &str, description: &str, prompt: &str) -> Result<(), String> {
        let payload = serde_json::to_string(&vec![
            json!(gem_id),
            json!([name, description, prompt, Value::Null, Value::Null, Value::Null, Value::Null, Value::Null, 0, Value::Null, 1, Value::Null, Value::Null, Value::Null, [], 0]),
        ])
        .unwrap();
        self.rpc("kHv0Vd", &payload)?;
        Ok(())
    }

    /// DELETE_CONVERSATION (GzXR5e) + GET_TASKS_IN_CONVERSATION
    /// (qWymEb), as in gemini-webapi's ChatMixin.delete_chat.
    pub fn delete_chat(&mut self, cid: &str) -> Result<(), String> {
        let payload = serde_json::to_string(&vec![json!(cid)]).unwrap();
        self.rpc("GzXR5e", &payload)?;
        let tasks_payload = serde_json::to_string(&vec![json!(cid), json!([1, Value::Null, 0, 1])]).unwrap();
        let _ = self.rpc("qWymEb", &tasks_payload);
        Ok(())
    }

    /// LIST_CONVERSATIONS (MaZiqc).
    pub fn list_chats(&mut self, limit: usize) -> Result<Vec<(String, String)>, String> {
        let payload = serde_json::to_string(&vec![
            json!(limit),
            Value::Null,
            json!([0, null, 1]),
        ])
        .unwrap();
        let payloads = self.rpc_payloads("MaZiqc", &payload)?;
        let mut out: Vec<(String, String)> = Vec::new();
        for pb in &payloads {
            if let Some(chats) = pb.get(2).and_then(|c| c.as_array()) {
                for cd in chats {
                    let cid = cd.get(0).and_then(|x| x.as_str()).unwrap_or("");
                    if cid.starts_with("c_") {
                        let title = cd.get(1).and_then(|x| x.as_str()).unwrap_or("").to_string();
                        if !out.iter().any(|(c, _)| c == cid) {
                            out.push((cid.to_string(), title));
                        }
                    }
                }
            }
        }
        Ok(out.into_iter().take(limit).collect())
    }

    /// LIST_CONVERSATION_TURNS (hNvQHb), oldest-first turn list.
    pub fn read_chat(&mut self, cid: &str, limit: usize) -> Result<Vec<(String, String)>, String> {
        let payload = serde_json::to_string(&vec![
            json!(cid),
            json!(limit),
            Value::Null,
            json!(1),
            json!([1]),
            json!([4]),
            Value::Null,
            json!(1),
        ])
        .unwrap();
        let payloads = self.rpc_payloads("hNvQHb", &payload)?;
        let mut pairs: Vec<Vec<(String, String)>> = Vec::new();
        for pb in &payloads {
            if let Some(turns) = pb.get(0).and_then(|t| t.as_array()) {
                for turn in turns {
                    let user = turn
                        .get(2)
                        .and_then(|u| u.get(0))
                        .and_then(|u| u.get(0))
                        .and_then(|u| u.as_str())
                        .unwrap_or("")
                        .to_string();
                    let model = turn
                        .get(3)
                        .and_then(|m| m.get(0))
                        .and_then(|m| m.get(0))
                        .and_then(|m| m.get(1))
                        .and_then(|m| m.get(0))
                        .and_then(|m| m.as_str())
                        .unwrap_or("")
                        .to_string();
                    let mut pair = Vec::new();
                    if !user.is_empty() {
                        pair.push(("user".to_string(), user));
                    }
                    if !model.is_empty() {
                        pair.push(("model".to_string(), model.chars().take(4000).collect()));
                    }
                    if !pair.is_empty() {
                        pairs.push(pair);
                    }
                }
            }
        }
        // hNvQHb serves newest-first; reverse whole pairs (Python does
        // the same) so user/model order inside a turn is preserved.
        let mut turns = Vec::new();
        for pair in pairs.into_iter().rev() {
            turns.extend(pair);
        }
        Ok(turns)
    }

    /// Deep-research report document, Python gemini-webapi
    /// `extract_deep_research_document`: the finished report is NOT
    /// the reply text (that is only a short "completed" notice) — it
    /// sits at candidate[30][0], with task id at [3], title at [2],
    /// and the markdown body at [4] (fallback [17][0]). The body is
    /// empty while the research is still running. Returns
    /// Some((title, content)) only when a non-empty body exists.
    pub fn read_deep_research_document(&mut self, cid: &str) -> Result<Option<(String, String)>, String> {
        let payload = serde_json::to_string(&vec![
            json!(cid),
            json!(10),
            Value::Null,
            json!(1),
            json!([1]),
            json!([4]),
            Value::Null,
            json!(1),
        ])
        .unwrap();
        let payloads = self.rpc_payloads("hNvQHb", &payload)?;
        for pb in &payloads {
            if let Some(turns) = pb.get(0).and_then(|t| t.as_array()) {
                for turn in turns {
                    // Python: candidates_list = conv_turn[3][0], then
                    // each candidate_data in that list. The old code
                    // passed the whole list as one candidate, so [30]
                    // never resolved and the poll always returned None.
                    let Some(cands) = turn.get(3).and_then(|m| m.get(0)).and_then(|c| c.as_array()) else {
                        continue;
                    };
                    for cand in cands {
                        if let Some(doc) = extract_deep_research_document(cand) {
                            return Ok(Some(doc));
                        }
                    }
                }
            }
        }
        Ok(None)
    }

    pub fn default_metadata_value() -> Value {
        default_metadata()
    }
}

/// Pure extraction for read_deep_research_document, unit-tested
/// below: candidate[30][0] with a non-null task id at [3], title
/// at [2], body at [4] (fallback [17][0]); None when no document
/// or the body is still empty (research running).
pub fn extract_deep_research_document(cand: &Value) -> Option<(String, String)> {
    let doc = cand.get(30).and_then(|d| d.get(0))?;
    if doc.get(3).map(|v| v.is_null()).unwrap_or(true) {
        return None;
    }
    let title = doc.get(2).and_then(|t| t.as_str()).unwrap_or("").to_string();
    let content = doc
        .get(4)
        .and_then(|c| c.as_str())
        .filter(|s| !s.is_empty())
        .map(|s| s.to_string())
        .or_else(|| {
            doc.get(17)
                .and_then(|c| c.get(0))
                .and_then(|c| c.as_str())
                .filter(|s| !s.is_empty())
                .map(|s| s.to_string())
        })?;
    Some((title, content))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn deep_research_document_extraction() {
        // Candidate with [30][0] = [id, null, title, task_id, body].
        let mut cand = vec![Value::Null; 31];
        cand[30] = json!([["doc1", null, "Printing Press", "task9", "# Report\nBody here"]]);
        let doc = extract_deep_research_document(&json!(cand));
        assert_eq!(doc, Some(("Printing Press".to_string(), "# Report\nBody here".to_string())));
        // Empty body (research running) -> None.
        let mut cand2 = vec![Value::Null; 31];
        cand2[30] = json!([["doc1", null, "Printing Press", "task9", ""]]);
        assert!(extract_deep_research_document(&json!(cand2)).is_none());
        // Null task id (non-research attachment) -> None.
        let mut cand3 = vec![Value::Null; 31];
        cand3[30] = json!([["x", null, "Card", null, "body"]]);
        assert!(extract_deep_research_document(&json!(cand3)).is_none());
    }

    #[test]
    fn auto_model_name_and_header() {
        // Live Auto row: [cf41..., "Auto", "Gemini 3 models", ...] with
        // display at [11] = "自動" in the localised UI — Python's
        // model_name is gemini-<category slug> when a category exists.
        let (name, aliases) = derive_name_aliases("cf41b0e0dd7d53e5", "Auto", "自動");
        assert_eq!(name, "gemini-auto");
        assert!(aliases.contains(&"auto".to_string()));
        let info = ModelInfo {
            id: "cf41b0e0dd7d53e5".into(),
            name,
            display: "自動".into(),
            description: String::new(),
            capacity: 1,
            capacity_field: 12,
            model_number: 6,
            aliases,
        };
        assert_eq!(
            info.header_value(),
            "[1,null,null,null,\"cf41b0e0dd7d53e5\",null,null,0,[4,5,6,8],null,null,1, null,null,6]"
        );
    }
}
