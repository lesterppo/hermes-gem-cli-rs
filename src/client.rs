//! Gemini WebAPI client (blocking reqwest, rustls).
//!
//! `base` is normally `https://gemini.google.com`. Tests point it at a
//! local mock server via `GeminiClient::with_base` / the
//! `GEMINI_RS_BASE_URL` env var (test-only; the CLI honours it so the
//! mock deep test can run without a live session).

use std::time::Duration;

use reqwest::blocking::Client;
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

/// Build a blocking client: native roots + the VM's TLS-intercept CA
/// when present (previous Rust ports on this VM needed both), with the
/// proxy env already sanitised by the caller (see util::sanitize_no_proxy).
pub fn build_client(timeout_secs: u64) -> Result<Client, String> {
    let mut b = Client::builder()
        .timeout(Duration::from_secs(timeout_secs))
        .user_agent(BROWSER_UA);
    if let Ok(pem) = std::fs::read("/etc/ssl/certs/hatch-egress-ca.pem") {
        if let Ok(cert) = reqwest::Certificate::from_pem(&pem) {
            b = b.add_root_certificate(cert);
        }
    }
    b.build().map_err(|e| e.to_string())
}

pub struct GeminiClient {
    pub auth: Auth,
    pub tokens: BatchexecuteTokens,
    pub base: String,
    pub http: Client,
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
        let resp = self
            .http
            .get(&url)
            .header("User-Agent", BROWSER_UA)
            .header("Cookie", self.cookie())
            .header("Accept-Language", "en")
            .send()
            .map_err(|e| e.to_string())?;
        if !resp.status().is_success() {
            return Err(format!("AUTH_EXPIRED: /app returned HTTP {}", resp.status()));
        }
        let html = resp.text().map_err(|e| e.to_string())?;
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
        let mut req = self
            .http
            .post(format!("{}/_/BardChatUi/data/batchexecute", self.base))
            .header("Cookie", self.cookie());
        for (k, v) in self.batch_headers() {
            req = req.header(k, v);
        }
        let resp = req.form(&form).send().map_err(|e| e.to_string())?;
        let status = resp.status();
        let text = resp.text().map_err(|e| e.to_string())?;
        if !status.is_success() || text[..text.len().min(2000)].contains("[\"e\",4") {
            return Err(format!("RPC {} failed: HTTP {}", rpcid, status));
        }
        Ok(text)
    }

    pub fn rpc_payloads(&mut self, rpcid: &str, payload: &str) -> Result<Vec<Value>, String> {
        let text = self.rpc(rpcid, payload)?;
        Ok(parse_batchexecute(&text, rpcid))
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
    ) -> Result<ChatResult, String> {
        let inner = build_stream_request(
            prompt,
            self.tokens.language.clone().as_deref().unwrap_or("en"),
            metadata,
            gem_id,
            temporary,
            deep_research,
            extended_thinking,
            model_number,
        );
        let reqid = self.reqid;
        self.reqid += 100000;
        let mut url = format!(
            "{}/_/BardChatUi/data/assistant.lamda.BardFrontendService/StreamGenerate?hl=en&_reqid={}&rt=c",
            self.base, reqid
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
        let resp = self
            .http
            .post(&url)
            .header("User-Agent", BROWSER_UA)
            .header("Cookie", self.cookie())
            .header("Origin", "https://gemini.google.com")
            .header("Referer", "https://gemini.google.com/app")
            .header("X-Same-Domain", "1")
            .header("x-goog-ext-525005358-jspb", "[\"\",1]")
            .timeout(Duration::from_secs(timeout_secs))
            .form(&[
                ("at", self.tokens.at.clone().unwrap_or_default()),
                ("f.req", frep),
            ])
            .send()
            .map_err(|e| {
                if e.is_timeout() {
                    format!("TIMEOUT: generation timed out after {}s", timeout_secs)
                } else {
                    e.to_string()
                }
            })?;
        if !resp.status().is_success() {
            return Err(format!("GEN_FAILED: StreamGenerate HTTP {}", resp.status()));
        }
        let text = resp.text().map_err(|e| e.to_string())?;
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

    /// GET_USER_STATUS (otAQ7b) -> model name strings (best-effort:
    /// the library builds AvailableModel objects from payload [15];
    /// we return every distinct gemini-* / Flash / Pro label found).
    pub fn list_models(&mut self) -> Result<Vec<String>, String> {
        let payloads = self.rpc_payloads("otAQ7b", "[]")?;
        let mut out: Vec<String> = Vec::new();
        for pb in &payloads {
            if let Some(models) = pb.get(15).and_then(|m| m.as_array()) {
                for m in models {
                    let mut strs = Vec::new();
                    json_strings(m, &mut strs);
                    for s in strs {
                        if (s.contains("gemini-") || s == "Flash-Lite" || s.contains("Flash"))
                            && !out.contains(&s)
                        {
                            out.push(s);
                        }
                    }
                }
            }
        }
        Ok(out)
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

    pub fn default_metadata_value() -> Value {
        default_metadata()
    }
}
