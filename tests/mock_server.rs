//! End-to-end test against a local mock Gemini server.
//!
//! The mock serves /app HTML (SNlM0e/cfb2h/FdrFJe), a StreamGenerate
//! response in the library's frame shape, and batchexecute LIST_BOTS /
//! GET_USER_STATUS responses. No live session and no real cookies are
//! involved; the "cookies" here are literal test strings.

use gem_cli_rs::auth::Auth;
use gem_cli_rs::client::GeminiClient;
use gem_cli_rs::protocol::default_metadata;
use std::io::{Read, Write};
use std::net::TcpListener;
use std::thread;

fn stream_body() -> String {
    let inner = serde_json::json!([
        null,
        ["c_mock", "r_mock"],
        null,
        null,
        [[
            "rc_mock",
            ["Hello from mock Gemini"],
            null, null, null, null, null, null,
            [2]
        ]]
    ]);
    let frame = serde_json::json!([null, null, serde_json::to_string(&inner).unwrap()]);
    format!(")]}}'\n{}", serde_json::to_string(&frame).unwrap())
}

fn batch_body(rpcid: &str, payload: serde_json::Value) -> String {
    let part = serde_json::json!([null, rpcid, serde_json::to_string(&payload).unwrap()]);
    format!(")]}}'\n{}", serde_json::to_string(&vec![part]).unwrap())
}

fn start_mock() -> String {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    thread::spawn(move || {
        for mut stream in listener.incoming().flatten() {
            // Read the full request: headers + Content-Length body.
            // A single read() is not enough — some transports send
            // headers and body in separate packets, and routing on the
            // body (rpcid) then sees an empty body and returns [].
            let mut raw: Vec<u8> = Vec::new();
            let mut buf = [0u8; 8192];
            loop {
                match stream.read(&mut buf) {
                    Ok(0) => break,
                    Ok(n) => {
                        raw.extend_from_slice(&buf[..n]);
                        let text = String::from_utf8_lossy(&raw);
                        if let Some(pos) = text.find("\r\n\r\n") {
                            let headers = &text[..pos];
                            let body_len = headers
                                .lines()
                                .find_map(|l| {
                                    let (k, v) = l.split_once(':')?;
                                    if k.trim().eq_ignore_ascii_case("content-length") {
                                        v.trim().parse::<usize>().ok()
                                    } else {
                                        None
                                    }
                                })
                                .unwrap_or(0);
                            if raw.len() >= pos + 4 + body_len {
                                break;
                            }
                        }
                        if raw.len() > 1_000_000 {
                            break;
                        }
                    }
                    Err(_) => break,
                }
            }
            let req = String::from_utf8_lossy(&raw).to_string();
            let first_line = req.lines().next().unwrap_or("").to_string();
            let body = if first_line.contains("GET /app") {
                "{\"SNlM0e\":\"MOCKAT\",\"cfb2h\":\"MOCKBL\",\"FdrFJe\":\"MOCKSID\",\"TuX5cc\":\"en\",\"qKIAYe\":\"MOCKPUSH\"}".to_string()
            } else if first_line.contains("StreamGenerate") {
                stream_body()
            } else if first_line.contains("batchexecute") {
                // The request body carries f.req; route on rpcids in the URL is
                // absent (form-encoded), so inspect the raw request text.
                if req.contains("CNgdBe") {
                    batch_body(
                        "CNgdBe",
                        serde_json::json!([null, null, [["gem1", ["Test Gem", "A test gem"]]]]),
                    )
                } else if req.contains("otAQ7b") {
                    // Realistic AvailableModel rows: [hex id, category,
                    // description, ..., display at [11], model_number
                    // at [17]] — category "Flash" -> name "gemini-flash".
                    let mut flash = vec![serde_json::Value::Null; 18];
                    flash[0] = serde_json::json!("fbb127bbb056c959");
                    flash[1] = serde_json::json!("Flash");
                    flash[2] = serde_json::json!("Fast model");
                    flash[11] = serde_json::json!("Flash");
                    flash[17] = serde_json::json!(1);
                    let mut pro = vec![serde_json::Value::Null; 18];
                    pro[0] = serde_json::json!("e6fa609c3fa255c0");
                    pro[1] = serde_json::json!("Pro");
                    pro[2] = serde_json::json!("Capable model");
                    pro[11] = serde_json::json!("Pro");
                    pro[17] = serde_json::json!(3);
                    let mut models = vec![serde_json::Value::Null; 18];
                    models[15] = serde_json::json!([flash, pro]);
                    batch_body("otAQ7b", serde_json::Value::Array(models))
                } else if req.contains("MaZiqc") {
                    batch_body(
                        "MaZiqc",
                        serde_json::json!([null, "tok", [["c_chat1", "Chat one"]]]),
                    )
                } else if req.contains("UXcSJb") {
                    batch_body("UXcSJb", serde_json::json!(["ok"]))
                } else if req.contains("oMH3Zd") {
                    batch_body("oMH3Zd", serde_json::json!(["newgem123"]))
                } else {
                    batch_body("UNKNOWN", serde_json::json!([]))
                }
            } else {
                "{}".to_string()
            };
            let resp = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                body.len(),
                body
            );
            let _ = stream.write_all(resp.as_bytes());
        }
    });
    format!("http://127.0.0.1:{}", addr.port())
}

fn mock_client(base: &str) -> GeminiClient {
    let auth = Auth {
        sid: "test-sid".into(),
        ts: Some("test-ts".into()),
        jar: vec![],
        source: "test".into(),
    };
    GeminiClient::with_base(auth, 30, base.to_string()).unwrap()
}

#[test]
fn mock_end_to_end() {
    let base = start_mock();
    let mut client = mock_client(&base);
    client.init().expect("mock init");
    assert_eq!(client.tokens.at.as_deref(), Some("MOCKAT"));
    assert_eq!(client.tokens.bl.as_deref(), Some("MOCKBL"));

    let res = client
        .chat(
            "hello",
            &default_metadata(),
            Some("gem1"),
            false,
            false,
            false,
            None,
            30,
            None,
            None,
        )
        .expect("mock chat");
    assert_eq!(res.text, "Hello from mock Gemini");
    assert_eq!(res.cid, "c_mock");
    assert_eq!(res.rid, "r_mock");
    assert!(res.completed);

    let gems = client.list_gems().expect("mock gems");
    assert_eq!(gems.len(), 2); // system + custom legs return the same mock gem
    assert_eq!(gems[0].id, "gem1");
    assert_eq!(gems[0].name, "Test Gem");

    let models = client.list_models().expect("mock models");
    assert_eq!(models, vec!["gemini-flash", "gemini-pro"]);
    let infos = client.list_model_infos().expect("mock model infos");
    assert_eq!(infos[0].model_number, 1);
    assert_eq!(infos[1].model_number, 3);
    assert!(infos[0].header_value().contains("fbb127bbb056c959"));
    let resolved = client.resolve_model("flash").expect("mock resolve flash");
    assert_eq!(resolved.name, "gemini-flash");

    let chats = client.list_chats(10).expect("mock chats");
    assert_eq!(chats[0].0, "c_chat1");

    let id = client
        .create_gem("G", "d", "p")
        .expect("mock create");
    assert_eq!(id, "newgem123");
    client.delete_gem("gem1").expect("mock delete");
}
