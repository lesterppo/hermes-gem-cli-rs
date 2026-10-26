//! Gemini WebAPI protocol: init-token extraction, StreamGenerate
//! request construction, StreamGenerate response parsing, and
//! browser-shaped batchexecute request/response handling.
//!
//! Shapes follow gemini-webapi (client.py `_generate`, `_batch_execute`)
//! and the Python original's `BrowserBatchRPC`:
//!  * StreamGenerate: POST .../StreamGenerate?hl=&_reqid=&rt=c&bl=&f.sid=
//!    with form `at=<SNlM0e>` and `f.req=[null, <inner JSON>]`, where the
//!    inner request is an 81-slot list (see `build_stream_request`).
//!  * batchexecute: POST .../batchexecute with form rpcids, source-path,
//!    bl, hl, f.req, at, where
//!    f.req = [[["<rpcid>", "<payload JSON string>", null, "generic"]]]
//!    and browser headers Origin / Referer / X-Same-Domain are present
//!    (the library's plain batchexecute gets `["e",4]` rejections on
//!    read RPCs without them).

use regex::Regex;
use serde_json::{json, Value};

/// Tokens extracted from gemini.google.com/app HTML.
#[derive(Debug, Clone, Default)]
pub struct BatchexecuteTokens {
    /// SNlM0e access token (`at` parameter).
    pub at: Option<String>,
    /// Build label (`bl` parameter, from `cfb2h`).
    pub bl: Option<String>,
    /// Frontend session id (`f.sid`, from `FdrFJe`).
    pub sid: Option<String>,
    pub language: Option<String>,
    pub push_id: Option<String>,
}

fn extract_one(html: &str, key: &str) -> Option<String> {
    let re = Regex::new(&format!(r#""{}":\s*"(.*?)""#, regex::escape(key))).ok()?;
    re.captures(html)
        .and_then(|c| c.get(1))
        .map(|m| m.as_str().to_string())
}

/// Extract init tokens from /app HTML. Returns None fields when absent;
/// callers treat a missing `at` as AUTH_EXPIRED (mirrors the Python
/// original's `BrowserBatchRPC.init` and gemini-webapi's init).
pub fn extract_tokens(html: &str) -> BatchexecuteTokens {
    BatchexecuteTokens {
        at: extract_one(html, "SNlM0e"),
        bl: extract_one(html, "cfb2h"),
        sid: extract_one(html, "FdrFJe"),
        language: extract_one(html, "TuX5cc"),
        push_id: extract_one(html, "qKIAYe"),
    }
}

/// The Python original's Gem-ID extraction: a gemini.google.com/gem/<id>
/// URL, or a bare id (>= 5 chars, no slash/space).
pub fn extract_gem_id(url: &str) -> Result<String, String> {
    if let Some(idx) = url.find("gemini.google.com/gem/") {
        let rest = &url[idx + "gemini.google.com/gem/".len()..];
        let id: String = rest
            .chars()
            .take_while(|c| c.is_ascii_alphanumeric() || *c == '-' || *c == '_')
            .collect();
        if !id.is_empty() {
            return Ok(id);
        }
    }
    if !url.contains('/') && !url.contains(' ') && url.len() >= 5 {
        return Ok(url.to_string());
    }
    Err(format!("Cannot extract Gem ID from: {}", url))
}

/// Default conversation metadata (gemini-webapi DEFAULT_METADATA):
/// ["", "", "", null x6, ""].
pub fn default_metadata() -> Value {
    json!(["", "", "", null, null, null, null, null, null, ""])
}

fn uuid_hex() -> String {
    uuid::Uuid::new_v4().simple().to_string()
}

/// Build the 81-slot StreamGenerate inner request, mirroring
/// gemini-webapi `_generate` exactly (indices documented there):
/// [0] message, [1] language, [2] chat metadata, [3]/[4] deep-research
/// tokens, [6]=[1], [7]=1 (streaming), [10]=1, [11]=0, [17]=[[0]],
/// [18]=0, [19]=gem id, [27]=1, [30]=[4], [41]=[1], [45]=temporary,
/// [49]=deep-research, [53]=0, [54]/[55]=deep-research, [59]=request
/// uuid, [61]=[], [68]=1, [79]=model number, [80]=thinking level.
#[allow(clippy::too_many_arguments)]
pub fn build_stream_request(
    prompt: &str,
    language: &str,
    metadata: &Value,
    gem_id: Option<&str>,
    temporary: bool,
    deep_research: bool,
    extended_thinking: bool,
    model_number: Option<i64>,
) -> Value {
    let mut req: Vec<Value> = vec![Value::Null; 81];
    req[0] = json!([prompt, 0, null, null, null, null, 0]);
    req[1] = json!([language]);
    req[2] = metadata.clone();
    if deep_research {
        // Python uses secrets.token_urlsafe(2600); a long unique token
        // serves the same purpose here.
        let token: String = (0..8).map(|_| uuid_hex()).collect();
        req[3] = json!(format!("!{}", token));
        req[4] = json!(uuid_hex());
    }
    req[6] = json!([1]);
    req[7] = json!(1);
    req[10] = json!(1);
    req[11] = json!(0);
    req[17] = json!([[0]]);
    req[18] = json!(0);
    if let Some(g) = gem_id {
        req[19] = json!(g);
    }
    req[27] = json!(1);
    req[30] = json!([4]);
    req[41] = json!(1);
    if temporary {
        req[45] = json!(1);
    }
    if deep_research {
        req[49] = json!(1);
        req[54] = json!([[[[[1]]]]]);
        req[55] = json!([[1]]);
    }
    req[53] = json!(0);
    req[59] = json!(uuid::Uuid::new_v4().to_string().to_uppercase());
    req[61] = json!([]);
    req[68] = json!(1);
    req[79] = json!(model_number.unwrap_or(1));
    req[80] = json!(if extended_thinking { 2 } else { 1 });
    Value::Array(req)
}

/// Build a browser-shaped batchexecute form body (as key/value pairs).
/// `payload` is the RPC payload JSON string (already serialised).
pub fn batchexecute_form(
    rpcid: &str,
    payload: &str,
    tokens: &BatchexecuteTokens,
) -> Vec<(String, String)> {
    let inner = vec![
        Value::String(rpcid.to_string()),
        Value::String(payload.to_string()),
        Value::Null,
        Value::String("generic".to_string()),
    ];
    let freq = serde_json::to_string(&vec![vec![inner]]).unwrap_or_default();
    vec![
        ("rpcids".into(), rpcid.into()),
        ("source-path".into(), "/app".into()),
        ("bl".into(), tokens.bl.clone().unwrap_or_default()),
        ("hl".into(), "en".into()),
        ("f.req".into(), freq),
        ("at".into(), tokens.at.clone().unwrap_or_default()),
    ]
}

/// Parse a batchexecute response body for `rpcid`, returning the inner
/// payload JSON values. Lines may carry the `)]}'` prefix; each part is
/// an array whose [1] is the rpcid and [2] is a JSON string body.
pub fn parse_batchexecute(text: &str, rpcid: &str) -> Vec<Value> {
    let mut out = Vec::new();
    for line in text.lines() {
        let line = line.trim().trim_start_matches(")]}'");
        if line.is_empty() {
            continue;
        }
        if let Ok(v) = serde_json::from_str::<Value>(line) {
            collect_rpc(&v, rpcid, &mut out);
        }
    }
    out
}

fn collect_rpc(v: &Value, rpcid: &str, out: &mut Vec<Value>) {
    if let Some(arr) = v.as_array() {
        if arr.len() >= 3 {
            if let (Some(id), Some(body)) = (arr[1].as_str(), arr[2].as_str()) {
                if id == rpcid {
                    if let Ok(parsed) = serde_json::from_str::<Value>(body) {
                        out.push(parsed);
                    }
                    return;
                }
            }
        }
        for item in arr {
            collect_rpc(item, rpcid, out);
        }
    }
}

/// One parsed StreamGenerate result.
#[derive(Debug, Clone, Default)]
pub struct StreamChat {
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

fn get<'a>(v: &'a Value, idx: usize) -> Option<&'a Value> {
    v.as_array().and_then(|a| a.get(idx))
}

fn get_str(v: &Value, idx: usize) -> String {
    get(v, idx).and_then(|x| x.as_str()).unwrap_or("").to_string()
}

/// JSPB sparse-field read: field `n` lives at index `n`, or, when the
/// array is short, under key `n+1` of the trailing object.
fn get_field<'a>(v: &'a Value, n: usize) -> Option<&'a Value> {
    if let Some(arr) = v.as_array() {
        if arr.len() > n {
            return Some(&arr[n]);
        }
        if let Some(last) = arr.last() {
            if let Some(obj) = last.as_object() {
                return obj.get(&(n + 1).to_string());
            }
        }
    }
    None
}

fn collect_urls(v: &Value, out: &mut Vec<String>) {
    match v {
        Value::String(s) => {
            if s.starts_with("https://") && !out.contains(s) {
                out.push(s.clone());
            }
        }
        Value::Array(a) => {
            for x in a {
                collect_urls(x, out);
            }
        }
        Value::Object(o) => {
            for (_, x) in o {
                collect_urls(x, out);
            }
        }
        _ => {}
    }
}

/// Parse a StreamGenerate response body. Accepts the library's frame
/// shape (a frame array whose [2] is an inner JSON string) as well as a
/// bare inner-JSON object. Returns the last complete candidate turn.
pub fn parse_stream_response(text: &str) -> StreamChat {
    let mut result = StreamChat::default();
    for line in text.lines() {
        let line = line.trim().trim_start_matches(")]}'");
        if line.is_empty() {
            continue;
        }
        let Ok(v) = serde_json::from_str::<Value>(line) else {
            continue;
        };
        visit_stream(&v, &mut result);
    }
    result
}

fn visit_stream(v: &Value, out: &mut StreamChat) {
    // Frame shape: [?, ?, "<inner JSON string>", ...]
    if let Some(arr) = v.as_array() {
        if arr.len() >= 3 {
            if let Some(inner) = arr[2].as_str() {
                if let Ok(pj) = serde_json::from_str::<Value>(inner) {
                    process_inner(&pj, out);
                    return;
                }
            }
        }
        // Bare inner shape: [null, metadata, null, null, candidates, ...]
        if arr.len() >= 5 && arr[4].is_array() && arr[1].is_array() {
            process_inner(v, out);
            return;
        }
        for item in arr {
            visit_stream(item, out);
        }
    }
}

fn process_inner(pj: &Value, out: &mut StreamChat) {
    let Some(meta) = get(pj, 1) else { return };
    if !meta.is_array() {
        return;
    }
    let cid = get_str(meta, 0);
    let rid = get_str(meta, 1);
    if !cid.is_empty() {
        out.cid = cid;
    }
    if !rid.is_empty() {
        out.rid = rid;
    }
    out.metadata = meta.clone();
    let Some(cands) = get(pj, 4).and_then(|c| c.as_array()) else {
        return;
    };
    for cand in cands {
        let rcid = get_str(cand, 0);
        if !rcid.is_empty() {
            out.rcid = rcid;
        }
        // Candidate text: [1][0]; thoughts: [37][0][0].
        let text = get(cand, 1)
            .and_then(|t| get(t, 0))
            .and_then(|t| t.as_str())
            .unwrap_or("")
            .to_string();
        if !text.is_empty() {
            out.text = text;
        }
        let thoughts = get(cand, 37)
            .and_then(|t| get(t, 0))
            .and_then(|t| get(t, 0))
            .and_then(|t| t.as_str())
            .unwrap_or("")
            .to_string();
        if !thoughts.is_empty() {
            out.thoughts = thoughts;
        }
        if let Some(ind) = get(cand, 8).and_then(|i| get(i, 0)).and_then(|i| i.as_i64()) {
            if ind == 2 {
                out.completed = true;
            }
        }
        // Rich content block [12]: web images field 1, generated
        // images field 7, video field 59, media field 86.
        if let Some(block) = get(cand, 12) {
            if let Some(web) = get_field(block, 1) {
                let mut urls = Vec::new();
                collect_urls(web, &mut urls);
                for u in urls {
                    if !out.images.contains(&u) {
                        out.images.push(u);
                    }
                }
            }
            if let Some(gen) = get_field(block, 7) {
                let mut urls = Vec::new();
                collect_urls(gen, &mut urls);
                for u in urls {
                    if !out.images.contains(&u) {
                        out.images.push(u);
                    }
                }
            }
            if let Some(vid) = get_field(block, 59) {
                let mut urls = Vec::new();
                collect_urls(vid, &mut urls);
                for u in urls {
                    if !out.videos.contains(&u) {
                        out.videos.push(u);
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn gem_id_extraction() {
        assert_eq!(
            extract_gem_id("https://gemini.google.com/gem/AbCdEf1234").unwrap(),
            "AbCdEf1234"
        );
        assert_eq!(extract_gem_id("AbCdEf1234").unwrap(), "AbCdEf1234");
        assert!(extract_gem_id("not a url").is_err());
        assert!(extract_gem_id("https://example.com/x").is_err());
    }

    #[test]
    fn token_extraction() {
        let html = r#"{"SNlM0e":"ATOK","cfb2h":"BL1","FdrFJe":"SID9","TuX5cc":"en","qKIAYe":"PUSH"}"#;
        let t = extract_tokens(html);
        assert_eq!(t.at.as_deref(), Some("ATOK"));
        assert_eq!(t.bl.as_deref(), Some("BL1"));
        assert_eq!(t.sid.as_deref(), Some("SID9"));
    }

    #[test]
    fn stream_request_indices() {
        let req = build_stream_request(
            "hi",
            "en",
            &default_metadata(),
            Some("GEM1"),
            false,
            false,
            true,
            None,
        );
        let arr = req.as_array().unwrap();
        assert_eq!(arr.len(), 81);
        assert_eq!(arr[0][0], "hi");
        assert_eq!(arr[7], 1);
        assert_eq!(arr[19], "GEM1");
        assert_eq!(arr[80], 2);
        assert_eq!(arr[30], json!([4]));
    }

    #[test]
    fn batchexecute_form_shape() {
        let toks = BatchexecuteTokens {
            at: Some("AT".into()),
            bl: Some("BL".into()),
            ..Default::default()
        };
        let form = batchexecute_form("CNgdBe", "[3,['en'],0]", &toks);
        let freq = form.iter().find(|(k, _)| k == "f.req").unwrap().1.clone();
        let parsed: Value = serde_json::from_str(&freq).unwrap();
        assert_eq!(parsed[0][0][0], "CNgdBe");
        assert_eq!(parsed[0][0][1], "[3,['en'],0]");
        assert_eq!(parsed[0][0][3], "generic");
    }

    #[test]
    fn stream_parse_mock_frame() {
        let inner = json!([
            null,
            ["c_1", "r_2"],
            null,
            null,
            [[
                "rc_3",
                ["Hello mock"],
                null, null, null, null, null, null,
                [2]
            ]]
        ]);
        let frame = json!([null, null, serde_json::to_string(&inner).unwrap()]);
        let body = format!(")]}}'\n{}", serde_json::to_string(&frame).unwrap());
        let out = parse_stream_response(&body);
        assert_eq!(out.text, "Hello mock");
        assert_eq!(out.cid, "c_1");
        assert_eq!(out.rid, "r_2");
        assert_eq!(out.rcid, "rc_3");
        assert!(out.completed);
    }

    #[test]
    fn batchexecute_parse() {
        let body = ")]}'\n[[null,\"CNgdBe\",\"[[\\\"g1\\\",[\\\"My Gem\\\",\\\"desc\\\"]]]\"]]";
        let out = parse_batchexecute(body, "CNgdBe");
        assert_eq!(out.len(), 1);
    }
}
