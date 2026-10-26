//! gem-cli-rs — Rust port of lesterppo/hermes-gem-cli (WebAPI approach).
//!
//! Stdout is always a compact JSON pointer (Python contract); the full
//! response goes to a file. Errors print {"ok":false,"err":...} and exit 1.
//! No cookie value is ever printed.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use clap::Parser;
use gem_cli_rs::auth::{doctor_candidates, home_dir, load_auth_cache, resolve_auth, save_auth_cache, Auth};
use gem_cli_rs::client::GeminiClient;
use gem_cli_rs::protocol::{default_metadata, extract_gem_id};
use gem_cli_rs::util::{classify_error, sanitize_no_proxy};
use serde_json::{json, Value};

#[derive(Parser, Debug)]
#[command(name = "gem-cli-rs", version, about = "Gemini Gem CLI via WebAPI (Rust port of hermes-gem-cli)")]
struct Args {
    /// Gem URL or Gem ID (not needed for --list-models/--list-gems/--doctor)
    url: Option<String>,
    /// Prompt words (joined with spaces)
    prompt: Vec<String>,

    #[arg(short = 'i', long = "image", action = clap::ArgAction::Append)]
    images: Vec<String>,
    #[arg(short = 'f', long = "file", action = clap::ArgAction::Append)]
    files: Vec<String>,
    #[arg(short = 'c', long = "conversation")]
    conversation: Option<String>,
    #[arg(long = "new")]
    new_conv: bool,
    #[arg(short = 'm', long = "model")]
    model: Option<String>,
    #[arg(long = "thinking")]
    thinking: Option<String>,
    #[arg(long = "img-gen")]
    image_gen: bool,
    #[arg(long = "img")]
    image_prompt: Option<String>,
    #[arg(long = "deep-research")]
    deep_research: bool,
    #[arg(long = "deep-research-status")]
    deep_research_status: Option<String>,
    #[arg(short = 'o', long = "output")]
    output: Option<String>,
    #[arg(long = "json-out")]
    json_out: bool,
    #[arg(long = "brief")]
    brief: bool,
    #[arg(short = 'q', long = "quiet")]
    quiet: bool,
    #[arg(long = "raw")]
    raw_mode: bool,
    #[arg(long = "browser")]
    browser: Option<String>,
    #[arg(long = "init")]
    init: bool,
    #[arg(long = "login")]
    login: bool,
    #[arg(short = 'p', long = "prompt-flag")]
    prompt_flag: Option<String>,
    #[arg(long = "create-gem")]
    create_gem_name: Option<String>,
    #[arg(long = "edit-gem")]
    edit_gem_id: Option<String>,
    #[arg(long = "delete-gem")]
    delete_gem_id: Option<String>,
    #[arg(long = "delete-chat")]
    delete_chat_id: Option<String>,
    #[arg(long = "gem-info")]
    gem_info: bool,
    #[arg(long = "clear")]
    clear_conv: bool,
    #[arg(long = "list-models")]
    list_models: bool,
    #[arg(long = "list-gems")]
    list_gems: bool,
    #[arg(long = "list-chats")]
    list_chats: bool,
    #[arg(long = "read-chat")]
    read_chat_id: Option<String>,
    #[arg(long = "fetch-latest")]
    fetch_latest_id: Option<String>,
    #[arg(long = "limit", default_value_t = 20)]
    limit: usize,
    #[arg(short = 't', long = "timeout", default_value_t = 120)]
    timeout: u64,
    #[arg(long = "timeout-soft")]
    timeout_soft: Option<u64>,
    #[arg(long = "no-retry")]
    no_retry: bool,
    #[arg(long = "extract-code")]
    extract_code: Option<usize>,
    #[arg(long = "extract-canvas")]
    extract_canvas: Option<String>,
    #[arg(long = "save-videos")]
    save_videos: Option<String>,
    #[arg(long = "save-media")]
    save_media: Option<String>,
    #[arg(long = "resume")]
    resume_session: Option<String>,
    #[arg(long = "temporary")]
    temporary: bool,
    #[arg(long = "show-thoughts")]
    show_thoughts: bool,
    #[arg(long = "stream")]
    stream: bool,
    #[arg(long = "doctor")]
    doctor: bool,
}

fn fail(code: &str, msg: &str, extra: Option<Value>) -> ! {
    let mut out = json!({"ok": false, "err": code, "msg": msg});
    if let Some(e) = extra {
        if let Some(obj) = e.as_object() {
            for (k, v) in obj {
                out[k] = v.clone();
            }
        }
    }
    println!("{}", out);
    std::process::exit(1);
}

fn log(raw: bool, msg: &str) {
    if !raw {
        eprintln!("[gem-cli-rs] {}", msg);
    }
}

fn short_path(p: &Path) -> String {
    if let Ok(cwd) = std::env::current_dir() {
        if let Ok(abs) = p.canonicalize().or_else(|_| Ok::<PathBuf, std::io::Error>(p.to_path_buf())) {
            if let Ok(rel) = abs.strip_prefix(&cwd) {
                return format!("./{}", rel.display());
            }
        }
    }
    p.display().to_string()
}

fn load_conv(path: &str) -> Option<Value> {
    let text = fs::read_to_string(path).ok()?;
    let v: Value = serde_json::from_str(&text).ok()?;
    let meta = v.get("metadata")?;
    if meta.is_array() && meta.as_array().map(|a| a.len() >= 1).unwrap_or(false) {
        Some(v)
    } else {
        None
    }
}

fn save_conv(path: &str, state: &Value) {
    let mut s = state.clone();
    s["updated"] = json!(gem_cli_rs::util::iso_now());
    let _ = fs::write(path, serde_json::to_string_pretty(&s).unwrap());
}

fn code_blocks(text: &str) -> Vec<(String, String)> {
    // Mirror Python: ```<lang>\n<code>``` (DOTALL).
    let mut out = Vec::new();
    let mut rest = text;
    while let Some(start) = rest.find("```") {
        let after = &rest[start + 3..];
        let Some(nl) = after.find('\n') else { break };
        let lang = after[..nl].to_string();
        let body = &after[nl + 1..];
        let Some(end) = body.find("```") else { break };
        out.push((lang, body[..end].trim().to_string()));
        rest = &body[end + 3..];
    }
    out
}

fn friendly_label(model: &Option<String>, thinking: &Option<String>) -> String {
    match (model.as_deref(), thinking.as_deref()) {
        (None, None) => "default".to_string(),
        (Some(m), _) => match m.to_lowercase().as_str() {
            "flash" | "fast" => "flash".to_string(),
            "pro" => "pro".to_string(),
            "thinking" | "think" => "thinking".to_string(),
            "lite" => "lite".to_string(),
            other => other.to_string(),
        },
        (None, Some(_)) => "default".to_string(),
    }
}

fn base_url() -> String {
    std::env::var("GEMINI_RS_BASE_URL")
        .ok()
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "https://gemini.google.com".to_string())
}

fn make_client(auth: &Auth, timeout: u64) -> GeminiClient {
    match GeminiClient::with_base(auth.clone(), timeout, base_url()) {
        Ok(c) => c,
        Err(e) => fail("INIT_FAILED", &e, None),
    }
}

fn main() -> ExitCode {
    sanitize_no_proxy();
    let args = Args::parse();
    let home = home_dir();
    let raw = args.raw_mode;

    // ── --init: validate + cache (same two fields as Python) ──
    if args.init {
        match resolve_auth(&home, &base_url()) {
            Ok(auth) => {
                if let Err(e) = save_auth_cache(&home, &auth.sid, auth.ts.as_deref()) {
                    fail("AUTH_CACHE_FAILED", &e.to_string(), None);
                }
                println!("{}", json!({"ok": true, "action": "init", "source": auth.source}));
                return ExitCode::SUCCESS;
            }
            Err(e) => fail("AUTH_EXPIRED", &e, None),
        }
    }

    // ── --login needs a browser (this binary has none) ──
    if args.login {
        fail(
            "NOT_YET",
            "Interactive browser login is not implemented in the Rust port. Sign in at gemini.google.com in a browser, then run gem-cli-rs --init (env GEMINI_SID/GEMINI_TS also work).",
            None,
        );
    }

    // ── --doctor: existence/liveness only, never values ──
    if args.doctor {
        let cands = doctor_candidates(&home, &base_url());
        let live = cands.iter().any(|c| c.live);
        let selected = cands.iter().find(|c| c.live).map(|c| c.source.clone());
        let sources: Vec<Value> = cands
            .iter()
            .map(|c| {
                json!({"src": c.source, "has_ts": c.has_ts, "jar_cookies": c.jar_cookies, "live": c.live})
            })
            .collect();
        println!(
            "{}",
            json!({
                "ok": true,
                "auth_cache_exists": load_auth_cache(&home).is_some(),
                "sources": sources,
                "selected": selected,
                "live_available": live,
                "hint": if live { Value::Null } else { json!("No live session. Sign in at gemini.google.com, then --init, or set GEMINI_SID/GEMINI_TS.") },
            })
        );
        return ExitCode::SUCCESS;
    }

    // ── --clear ──
    if args.clear_conv {
        let Some(conv) = &args.conversation else {
            fail("NO_CONV", "Use --clear with -c <file>.", None);
        };
        let p = Path::new(conv);
        let existed = p.exists();
        if existed {
            let _ = fs::remove_file(p);
        }
        println!("{}", json!({"ok": true, "action": "clear", "file": conv, "was_present": existed}));
        return ExitCode::SUCCESS;
    }

    // ── Uploads are not yet implemented (no content-push upload path) ──
    if !args.images.is_empty() || !args.files.is_empty() {
        fail(
            "UPLOAD_NOT_YET",
            "File/image upload (-f/-i) is not yet implemented in the Rust port (content-push upload path). Use the Python gem-cli for uploads, or chat with a text prompt.",
            None,
        );
    }
    if args.edit_gem_id.is_some() || args.delete_chat_id.is_some() {
        fail(
            "NOT_YET",
            "--edit-gem / --delete-chat are not yet implemented in the Rust port (UPDATE_BOT_METADATA / DELETE_CONVERSATION payloads not verified against a live session).",
            None,
        );
    }

    // ── Early validation, mirroring gem-cli.py: BAD_URL is reported before
    // any auth work on the chat/gem-info path, and a bare invocation (no
    // URL, no prompt, tty stdin) prints usage like Python's argparse.
    let discovery_only = args.list_models
        || args.list_gems
        || args.create_gem_name.is_some()
        || args.delete_gem_id.is_some()
        || args.list_chats
        || args.read_chat_id.is_some()
        || args.fetch_latest_id.is_some()
        || args.deep_research_status.is_some();
    if !discovery_only {
        if let Some(url) = &args.url {
            if let Err(e) = extract_gem_id(url) {
                fail("BAD_URL", &e, None);
            }
        } else if args.prompt.is_empty()
            && args.prompt_flag.is_none()
            && args.conversation.is_none()
        {
            eprintln!(
                "Usage: gem-cli-rs [OPTIONS] [URL] [PROMPT]...\n\nRun `gem-cli-rs --help` for the full flag list."
            );
            return ExitCode::from(2);
        }
    }

    // ── Auth for everything below ──
    let auth = match resolve_auth(&home, &base_url()) {
        Ok(a) => a,
        Err(e) => fail("AUTH_EXPIRED", &e, None),
    };
    let mut client = make_client(&auth, args.timeout);
    if let Err(e) = client.init() {
        fail(&classify_error(&e), &e, None);
    }

    // ── Discovery / CRUD (no prompt needed) ──
    if args.list_models {
        match client.list_models() {
            Ok(models) => {
                println!("{}", json!({"ok": true, "models": models}));
                return ExitCode::SUCCESS;
            }
            Err(e) => fail("LIST_FAILED", &e, None),
        }
    }
    if args.list_gems {
        match client.list_gems() {
            Ok(gems) => {
                let arr: Vec<Value> = gems
                    .iter()
                    .map(|g| json!({"id": g.id, "name": g.name, "description": g.description, "type": g.gem_type}))
                    .collect();
                println!("{}", json!({"ok": true, "gems": arr}));
                return ExitCode::SUCCESS;
            }
            Err(e) => fail("LIST_FAILED", &e, None),
        }
    }
    if args.create_gem_name.is_some() {
        let name = args.create_gem_name.clone().unwrap();
        let prompt = if !args.prompt.is_empty() {
            args.prompt.join(" ")
        } else {
            args.prompt_flag.clone().unwrap_or_default()
        };
        match client.create_gem(&name, &format!("Hermes task-specific Gem: {}", name), &prompt) {
            Ok(id) => {
                println!("{}", json!({"ok": true, "action": "create-gem", "id": id, "name": name}));
                return ExitCode::SUCCESS;
            }
            Err(e) => fail("GEM_CREATE_FAILED", &e, None),
        }
    }
    if let Some(id) = &args.delete_gem_id {
        match client.delete_gem(id) {
            Ok(()) => {
                println!("{}", json!({"ok": true, "action": "delete-gem", "id": id}));
                return ExitCode::SUCCESS;
            }
            Err(e) => fail("GEM_DELETE_FAILED", &e, None),
        }
    }
    if args.list_chats {
        match client.list_chats(args.limit) {
            Ok(chats) => {
                let arr: Vec<Value> = chats
                    .iter()
                    .map(|(cid, title)| json!({"cid": cid, "title": title}))
                    .collect();
                let total = arr.len();
                println!("{}", json!({"ok": true, "chats": arr, "total": total}));
                return ExitCode::SUCCESS;
            }
            Err(e) => fail("LIST_CHATS_FAILED", &e, None),
        }
    }
    if let Some(cid) = &args.read_chat_id {
        match client.read_chat(cid, args.limit) {
            Ok(turns) => {
                if turns.is_empty() {
                    fail("CHAT_NOT_FOUND", &format!("Chat {} not found or empty.", cid), None);
                }
                let arr: Vec<Value> = turns
                    .iter()
                    .map(|(role, text)| json!({"role": role, "text": text}))
                    .collect();
                let total = arr.len();
                println!(
                    "{}",
                    json!({"ok": true, "chat": {"cid": cid, "turns": arr, "total": total, "order": "oldest-first"}})
                );
                return ExitCode::SUCCESS;
            }
            Err(e) => fail("READ_CHAT_FAILED", &e, None),
        }
    }
    if let Some(cid) = &args.fetch_latest_id {
        match client.read_chat(cid, 1) {
            Ok(turns) => {
                let last = turns.iter().rev().find(|(r, _)| r == "model");
                match last {
                    Some((_, text)) => {
                        println!("{}", json!({"ok": true, "cid": cid, "text": text}));
                        return ExitCode::SUCCESS;
                    }
                    None => fail("FETCH_LATEST_FAILED", &format!("No response for {}", cid), None),
                }
            }
            Err(e) => fail("FETCH_LATEST_FAILED", &e, None),
        }
    }
    if let Some(cid) = &args.deep_research_status {
        // Python: there is no research-task entity; the only real signal
        // is whether the report has landed in the conversation.
        match client.read_chat(cid, 10) {
            Ok(turns) => {
                let last = turns.iter().rev().find(|(r, _)| r == "model");
                match last {
                    Some((_, text)) => {
                        let status = if text.len() > 200 { "report_present" } else { "in_progress" };
                        println!("{}", json!({"ok": true, "cid": cid, "status": status, "text": text}));
                        return ExitCode::SUCCESS;
                    }
                    None => fail("FETCH_LATEST_FAILED", &format!("No response for {}", cid), None),
                }
            }
            Err(e) => fail("FETCH_LATEST_FAILED", &e, None),
        }
    }

    // ── Chat path ──
    let Some(url) = &args.url else {
        fail("NO_URL", "Shared Gem URL or Gem ID is required.", None);
    };
    let gem_id = match extract_gem_id(url) {
        Ok(id) => id,
        Err(e) => fail("BAD_URL", &e, None),
    };

    // --gem-info needs auth but no prompt.
    if args.gem_info {
        match client.gem_info(&gem_id) {
            Ok(g) => {
                if g.name.is_empty() && g.gem_type == "external" {
                    println!(
                        "{}",
                        json!({"ok": true, "gem": {"id": gem_id, "name": "", "description": "", "type": "external", "note": "Shared Gem — not in your library"}})
                    );
                } else {
                    println!(
                        "{}",
                        json!({"ok": true, "gem": {"id": g.id, "name": g.name, "description": g.description, "type": g.gem_type}})
                    );
                }
                return ExitCode::SUCCESS;
            }
            Err(e) => fail("GEM_INFO_FAILED", &e, None),
        }
    }

    // Build prompt (mirrors gem-cli.py).
    let mut prompt = if let Some(img) = &args.image_prompt {
        format!("Generate an image: {}", img)
    } else if let Some(pf) = &args.prompt_flag {
        let mut p = pf.clone();
        if !atty_stdin() {
            let stdin = read_stdin();
            if !stdin.is_empty() {
                p = format!("{}\n\n{}", p, stdin);
            }
        }
        p
    } else if !args.prompt.is_empty() {
        args.prompt.join(" ")
    } else if !atty_stdin() {
        let s = read_stdin();
        if s.is_empty() {
            fail("NO_PROMPT", "No prompt provided.", None);
        }
        s
    } else if args.image_gen {
        "Generate an image.".to_string()
    } else {
        fail("NO_PROMPT", "No prompt provided. Use positional args, -p, or pipe via stdin.", None);
    };
    if args.brief && !prompt.to_lowercase().starts_with("be concise") {
        prompt = format!("Be concise. {}", prompt);
    }

    // Conversation state.
    let mut conv_state: Option<Value> = None;
    let mut metadata = default_metadata();
    if let Some(resume) = &args.resume_session {
        // Resume by chat id: metadata carries the id; turns start at 0.
        conv_state = Some(json!({"cid": resume, "metadata": [resume, ""], "turns": 0}));
        metadata = json!([resume, ""]);
        log(raw, &format!("Resuming session {}", resume));
    } else if let Some(conv) = &args.conversation {
        if !args.new_conv {
            if let Some(state) = load_conv(conv) {
                metadata = state.get("metadata").cloned().unwrap_or_else(default_metadata);
                conv_state = Some(state);
            }
        }
        if conv_state.is_none() {
            conv_state = Some(json!({"cid": null, "metadata": null, "turns": 0}));
        }
    }
    if args.image_gen && args.model.is_none() {
        log(raw, "Image generation: using default model (Python forces flash)");
    }

    let actual_timeout = if args.deep_research && args.timeout == 120 {
        600
    } else {
        args.timeout
    };
    let _ = args.timeout_soft; // Accepted for CLI compatibility; the hard timeout governs.
    let max_attempts = if args.no_retry { 1 } else { 3 };
    let model_label = friendly_label(&args.model, &args.thinking);
    let extended = args
        .thinking
        .as_deref()
        .map(|t| matches!(t.to_lowercase().as_str(), "extended" | "advanced"))
        .unwrap_or(false);

    let mut last_err: Option<String> = None;
    for attempt in 0..max_attempts {
        if attempt > 0 {
            log(raw, &format!("Retry {}...", attempt + 1));
        }
        match client.chat(
            &prompt,
            &metadata,
            Some(&gem_id),
            args.temporary,
            args.deep_research,
            extended,
            None,
            actual_timeout,
        ) {
            Ok(res) => {
                let mut text = res.text.clone();
                if args.show_thoughts && !res.thoughts.is_empty() {
                    text = format!("{}\n\n## Thoughts\n\n{}", text, res.thoughts);
                }
                // Conversation update.
                if let Some(state) = &mut conv_state {
                    if !res.metadata.is_null() {
                        state["cid"] = json!(res.cid);
                        state["metadata"] = res.metadata.clone();
                        let turns = state["turns"].as_u64().unwrap_or(0);
                        state["turns"] = json!(turns + 1);
                    }
                    if let Some(conv) = &args.conversation {
                        save_conv(conv, state);
                    }
                }
                // Media downloads (best effort; failures are logged, not fatal).
                let mut vids: Vec<String> = Vec::new();
                let mut media: Vec<String> = Vec::new();
                if let Some(dir) = &args.save_videos {
                    for (i, u) in res.videos.iter().enumerate() {
                        if download_media(&client, u, dir, &format!("video-{}", i)).is_ok() {
                            vids.push(u.clone());
                        }
                    }
                }
                if let Some(dir) = &args.save_media {
                    for (i, u) in res.images.iter().enumerate() {
                        if download_media(&client, u, dir, &format!("media-{}", i)).is_ok() {
                            media.push(u.clone());
                        }
                    }
                }
                // Output file.
                let ext = if args.json_out { ".json" } else { ".md" };
                let default_name = format!(
                    "gem-cli-rs-{}{}",
                    chrono_stamp(),
                    ext
                );
                let out_path = args
                    .output
                    .clone()
                    .map(PathBuf::from)
                    .unwrap_or_else(|| PathBuf::from(default_name));
                let blocks = code_blocks(&text);
                if args.json_out {
                    let mut payload = json!({"ok": true, "text": text, "model": model_label});
                    if !res.images.is_empty() {
                        payload["images"] = json!(res.images.iter().map(|u| json!({"url": u, "alt": ""})).collect::<Vec<_>>());
                    }
                    if let Some(state) = &conv_state {
                        payload["conversation"] = state.clone();
                    }
                    let _ = fs::write(&out_path, serde_json::to_string_pretty(&payload).unwrap());
                } else {
                    let mut out_text = text.clone();
                    if !res.images.is_empty() {
                        out_text.push_str("\n\n## Generated Images\n\n");
                        for (i, u) in res.images.iter().enumerate() {
                            out_text.push_str(&format!("{}. ![ ]({})\n", i + 1, u));
                        }
                    }
                    let _ = fs::write(&out_path, out_text);
                }
                if let Some(n) = args.extract_code {
                    if n < 1 || n > blocks.len() {
                        fail(
                            "BAD_CODE_INDEX",
                            &format!("Code block {} not found. Response has {} code blocks.", n, blocks.len()),
                            None,
                        );
                    }
                    let (lang, code) = &blocks[n - 1];
                    let code_path = args
                        .output
                        .clone()
                        .map(PathBuf::from)
                        .unwrap_or_else(|| PathBuf::from(format!("gem-cli-rs-code-{}.{}", n, if lang.is_empty() { "txt" } else { lang })));
                    let _ = fs::write(&code_path, code);
                    println!(
                        "{}",
                        json!({"ok": true, "action": "extract-code", "n": n, "lang": if lang.is_empty() { "text" } else { lang }, "f": short_path(&code_path), "s": fs::metadata(&code_path).map(|m| m.len()).unwrap_or(0)})
                    );
                    return ExitCode::SUCCESS;
                }
                if let Some(canvas_file) = &args.extract_canvas {
                    let html = blocks
                        .iter()
                        .find(|(l, _)| l == "html")
                        .map(|(_, c)| c.clone())
                        .or_else(|| {
                            if text.contains("<html") {
                                Some(text.clone())
                            } else {
                                None
                            }
                        });
                    match html {
                        Some(h) => {
                            let _ = fs::write(canvas_file, h);
                            println!("{}", json!({"ok": true, "action": "extract-canvas", "f": canvas_file}));
                        }
                        None => fail("CANVAS_NOT_FOUND", "No Canvas/HTML artifact in response.", None),
                    }
                    return ExitCode::SUCCESS;
                }
                if args.stream {
                    // Buffered in v1 (blocking client reads the full
                    // response); the text is still emitted here.
                    println!("{}", text);
                }
                let size = fs::metadata(&out_path).map(|m| m.len()).unwrap_or(0);
                let mut pointer = json!({"ok": true, "f": short_path(&out_path), "s": size});
                if !blocks.is_empty() {
                    pointer["b"] = json!(blocks.len());
                }
                if !res.images.is_empty() {
                    pointer["imgs"] = json!(res.images.len());
                }
                if !vids.is_empty() {
                    pointer["vids"] = json!(vids.len());
                }
                if !media.is_empty() {
                    pointer["media"] = json!(media.len());
                }
                if args.show_thoughts && !res.thoughts.is_empty() {
                    pointer["thoughts"] = json!(true);
                }
                pointer["model"] = json!(model_label);
                pointer["gem"] = json!(gem_id);
                if args.deep_research {
                    pointer["dr"] = json!(true);
                }
                if args.temporary {
                    pointer["tmp"] = json!(true);
                }
                if let Some(state) = &conv_state {
                    pointer["c"] = state["cid"].clone();
                    pointer["t"] = state["turns"].clone();
                }
                println!("{}", pointer);
                return ExitCode::SUCCESS;
            }
            Err(e) => {
                let kind = classify_error(&e);
                if kind == "AUTH_EXPIRED" && attempt + 1 < max_attempts {
                    log(raw, "Auth expired, re-resolving...");
                    match resolve_auth(&home, &base_url()) {
                        Ok(a) => {
                            client = make_client(&a, args.timeout);
                            if client.init().is_ok() {
                                continue;
                            }
                        }
                        Err(_) => {}
                    }
                }
                if kind == "RATE_LIMIT" && attempt + 1 < max_attempts {
                    log(raw, "Rate limited, retrying (no sleep in v1 fast path)...");
                    continue;
                }
                last_err = Some(e);
                if attempt + 1 >= max_attempts {
                    break;
                }
            }
        }
    }
    let e = last_err.unwrap_or_else(|| "RETRY_EXHAUSTED".to_string());
    if e == "RETRY_EXHAUSTED" {
        fail("RETRY_EXHAUSTED", &format!("Failed after {} attempts.", max_attempts), None);
    }
    fail(&classify_error(&e), &e, None);
}

fn atty_stdin() -> bool {
    // Without a libc isatty binding, treat a non-piped stdin conservatively:
    // if stdin has data available via a pipe it is read by callers only
    // when this returns false. We approximate with a /dev/stdin metadata
    // check: character devices are terminals, pipes/files are not.
    use std::os::unix::fs::FileTypeExt;
    match fs::metadata("/dev/stdin") {
        Ok(m) => m.file_type().is_char_device(),
        Err(_) => true,
    }
}

fn read_stdin() -> String {
    use std::io::Read;
    let mut s = String::new();
    let _ = std::io::stdin().read_to_string(&mut s);
    s.trim().to_string()
}

fn chrono_stamp() -> String {
    // YYYYMMDD-HHMMSS in UTC from epoch seconds.
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let _ = secs;
    // Reuse the civil algorithm via iso_now and reformat.
    let iso = gem_cli_rs::util::iso_now();
    format!(
        "{}{}{}-{}{}{}",
        &iso[0..4], &iso[5..7], &iso[8..10], &iso[11..13], &iso[14..16], &iso[17..19]
    )
}

fn download_media(client: &GeminiClient, url: &str, dir: &str, stem: &str) -> Result<(), String> {
    let _ = fs::create_dir_all(dir);
    let ext = url.rsplit('.').next().unwrap_or("bin");
    let ext = if ext.len() <= 5 && ext.chars().all(|c| c.is_ascii_alphanumeric()) {
        ext
    } else {
        "bin"
    };
    let resp = client
        .http
        .get(url)
        .header("User-Agent", gem_cli_rs::auth::BROWSER_UA)
        .header("Cookie", client.auth.cookie_header())
        .send()
        .map_err(|e| e.to_string())?;
    let bytes = resp.bytes().map_err(|e| e.to_string())?;
    fs::write(format!("{}/{}.{}", dir, stem, ext), &bytes).map_err(|e| e.to_string())?;
    Ok(())
}
