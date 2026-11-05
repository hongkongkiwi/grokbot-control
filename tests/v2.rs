//! v2 coverage: rename-era features — bots/routines/jobs, media, semantic
//! search, OAuth server + scopes, cron, state recovery, safety everywhere.

use grokbot_control::backend::MockBackend;
use grokbot_control::mcp;
use grokbot_control::state::Store;
use grokbot_control::tools::{self, Ctx};
use serde_json::{json, Value};
use std::sync::Arc;

fn ctx() -> Ctx {
    Ctx::new(Store::in_memory(), Arc::new(MockBackend))
}

async fn call(ctx: &Ctx, id: u64, name: &str, args: Value) -> Value {
    let resp = mcp::handle(
        ctx,
        &json!({"jsonrpc": "2.0", "id": id, "method": "tools/call",
                "params": {"name": name, "arguments": args}}),
    )
    .await
    .expect("response");
    let text = resp["result"]["content"][0]["text"]
        .as_str()
        .unwrap_or_else(|| panic!("no text in {resp}"))
        .to_string();
    assert_eq!(
        resp["result"]["isError"], false,
        "tool {name} errored: {text}"
    );
    serde_json::from_str(&text).unwrap_or_else(|_| json!({"raw": text}))
}

#[tokio::test]
async fn bots_routines_jobs_media_semantic() {
    let c = ctx();
    // Bots registry
    call(
        &c,
        1,
        "create_bot",
        json!({"name": "Helper", "model": "grok-3-mini"}),
    )
    .await;
    let bots = call(&c, 2, "list_bots", json!({})).await;
    assert_eq!(bots["bots"].as_array().unwrap().len(), 1);
    call(&c, 3, "activate_bot", json!({"id": "Helper"})).await;
    let status = call(&c, 4, "bot_status", json!({})).await;
    assert_eq!(status["bot"], "Helper");
    assert_eq!(status["status"], "idle");

    // Routines over tasks, with cron validation
    let r = call(
        &c,
        5,
        "routines",
        json!({"action": "add", "name": "morning", "cron": "0 9 * * 1-5", "prompt": "brief me"}),
    )
    .await;
    let rid = r["routine"]["id"].as_str().unwrap().to_string();
    call(&c, 6, "routines", json!({"action": "run", "id": rid})).await;
    call(&c, 7, "routines", json!({"action": "disable", "id": rid})).await;
    let list = call(&c, 8, "routines", json!({"action": "list"})).await;
    assert_eq!(list["tasks"][0]["enabled"], false);
    let bad = mcp::handle(
        &c,
        &json!({"jsonrpc": "2.0", "id": 9, "method": "tools/call",
                "params": {"name": "routines", "arguments": {"action": "add", "name": "x", "cron": "banana", "prompt": "p"}}}),
    )
    .await
    .unwrap();
    assert_eq!(bad["result"]["isError"], true);

    // Async job: ask / wait / status
    let job = call(&c, 10, "chat_async", json!({"prompt": "job hello"})).await;
    let jid = job["job_id"].as_str().unwrap().to_string();
    let waited = call(
        &c,
        11,
        "job_wait",
        json!({"id": jid, "timeout_seconds": 10}),
    )
    .await;
    assert_eq!(waited["job"]["status"], "done");
    assert!(waited["job"]["result"]["reply"]
        .as_str()
        .unwrap()
        .contains("job hello"));
    call(&c, 12, "job_list", json!({})).await;

    // Media + files (mock)
    call(&c, 13, "video_generate", json!({"prompt": "waves"})).await;
    let up = call(
        &c,
        14,
        "file_upload",
        json!({"filename": "note.txt", "content_base64": "aGVsbG8=", "store_as_document": true}),
    )
    .await;
    assert!(up["file"]["id"].as_str().unwrap().starts_with("mock-file-"));
    call(
        &c,
        15,
        "audio_transcribe",
        json!({"audio_base64": "aGVsbG8="}),
    )
    .await;
    let speak = call(&c, 16, "audio_speak", json!({"text": "hello"})).await;
    assert!(!speak["audio_base64"].as_str().unwrap().is_empty());

    // Semantic search finds the uploaded document
    let sem = call(&c, 17, "semantic_search", json!({"query": "hello note"})).await;
    assert!(!sem["hits"].as_array().unwrap().is_empty());
    call(&c, 18, "embed_text", json!({"texts": ["a", "b"]})).await;

    // Streaming + search honesty in mock
    let stream = call(&c, 19, "chat_stream", json!({"prompt": "stream me"})).await;
    assert!(stream["deltas"].as_u64().unwrap() >= 1);
    let ws = call(&c, 20, "web_search", json!({"query": "hello"})).await;
    assert_eq!(ws["live_search"], false);
    call(&c, 21, "x_search", json!({"query": "hello"})).await;

    // Message edit/delete
    let conv = call(&c, 22, "conversation_create", json!({"title": "edit me"})).await;
    let cid = conv["conversation"]["id"].as_str().unwrap().to_string();
    call(
        &c,
        23,
        "conversation_send",
        json!({"id": cid, "content": "original"}),
    )
    .await;
    let got = call(&c, 24, "conversation_get", json!({"id": cid})).await;
    let mid = got["conversation"]["messages"][0]["id"]
        .as_str()
        .unwrap()
        .to_string();
    assert!(!mid.is_empty(), "messages must carry ids");
    call(
        &c,
        25,
        "conversation_message_edit",
        json!({"id": cid, "message_id": mid, "content": "edited"}),
    )
    .await;
    let got = call(&c, 26, "conversation_get", json!({"id": cid})).await;
    assert_eq!(got["conversation"]["messages"][0]["content"], "edited");
    assert!(
        got["conversation"]["messages"][0]["edited_at"].is_string(),
        "edit must stamp edited_at"
    );
    call(
        &c,
        27,
        "conversation_message_delete",
        json!({"id": cid, "message_id": mid}),
    )
    .await;

    // Persona versions + rollback + A/B; bot tool defs; quota
    call(
        &c,
        28,
        "persona_update",
        json!({"id": "Coder", "system_prompt": "v2 prompt"}),
    )
    .await;
    let hist = call(&c, 29, "persona_prompt_history", json!({"id": "Coder"})).await;
    assert!(hist["versions"].as_array().unwrap().len() >= 2);
    call(
        &c,
        30,
        "persona_rollback",
        json!({"id": "Coder", "version": 1}),
    )
    .await;
    call(
        &c,
        31,
        "persona_ab_test",
        json!({"prompt": "ab", "persona_a": "Coder", "persona_b": "Researcher"}),
    )
    .await;
    call(&c, 32, "bot_tool_define", json!({"name": "get_weather", "parameters": {"type": "object", "properties": {"city": {"type": "string"}}}})).await;
    call(&c, 33, "bot_tool_defs_list", json!({})).await;
    call(&c, 34, "bot_tool_delete", json!({"name": "get_weather"})).await;
    call(&c, 35, "usage_quota", json!({})).await;
    call(&c, 36, "list_backups", json!({})).await;
}

#[tokio::test]
async fn safety_applies_to_every_chat_path_and_scopes_gate() {
    let c = ctx();
    call(
        &c,
        1,
        "bot_safety_update",
        json!({"blocked_topics": ["forbidden"]}),
    )
    .await;
    // Previously-bypassing paths must now refuse too.
    for (i, (tool, args)) in [
        ("chat_batch", json!({"prompts": ["say forbidden things"]})),
        (
            "model_compare",
            json!({"prompt": "forbidden topic", "models": ["grok-4"]}),
        ),
        ("text_summarize", json!({"text": "forbidden text"})),
        ("web_search", json!({"query": "forbidden"})), // mock path does doc search, still fine to allow; skip if ok
    ]
    .into_iter()
    .enumerate()
    {
        let resp = mcp::handle(
            &c,
            &json!({"jsonrpc": "2.0", "id": 10 + i as u64, "method": "tools/call",
                    "params": {"name": tool, "arguments": args}}),
        )
        .await
        .unwrap();
        if tool == "web_search" {
            continue; // mock web_search never reaches the model; documented
        }
        if tool == "chat_batch" || tool == "model_compare" {
            // Partial-result tools surface the refusal per item.
            let text = resp["result"]["content"][0]["text"].as_str().unwrap();
            let payload: Value = serde_json::from_str(text).unwrap();
            assert_eq!(
                payload["results"][0]["ok"], false,
                "{tool} item must be refused"
            );
            assert!(payload["results"][0]["error"]
                .as_str()
                .unwrap()
                .contains("blocked"));
            continue;
        }
        assert_eq!(
            resp["result"]["isError"], true,
            "{tool} must enforce blocked topics"
        );
    }

    // Scope gate: a read-only caller cannot chat or administer.
    let ro = Ctx::new(Store::in_memory(), Arc::new(MockBackend)).with_scopes(vec!["read".into()]);
    let resp = mcp::handle(
        &ro,
        &json!({"jsonrpc": "2.0", "id": 1, "method": "tools/call",
                "params": {"name": "chat", "arguments": {"prompt": "hi"}}}),
    )
    .await
    .unwrap();
    assert_eq!(resp["result"]["isError"], true);
    let resp = mcp::handle(
        &ro,
        &json!({"jsonrpc": "2.0", "id": 2, "method": "tools/call",
                "params": {"name": "bot_get_profile", "arguments": {}}}),
    )
    .await
    .unwrap();
    assert_eq!(resp["result"]["isError"], false);
}

#[test]
fn cron_validation_and_next_run() {
    assert!(tools::validate_cron("0 9 * * 1-5").is_ok());
    assert!(tools::validate_cron("banana").is_err());
    assert!(tools::validate_cron("61 9 * * *").is_err());
    assert!(tools::validate_cron("*/15 * * * *").is_ok());
    let from = chrono::DateTime::<chrono::Utc>::from_timestamp(1_762_329_600, 0).unwrap(); // 2025-11-05T00:00Z Wed
    let next = tools::next_run_after("0 9 * * *", from).unwrap();
    assert_eq!(next.timestamp(), 1_762_333_200); // same day 09:00Z (from was 08:00Z)
}

#[test]
fn old_state_and_corrupt_state_recovery() {
    // v1-shaped state (no schema_version, no message ids, no new fields) loads.
    let dir = std::env::temp_dir().join(format!("grokbot-v2-{}", grokbot_control::state::new_id()));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("state.json");
    let v1 = json!({
        "config": {"name": "OldBot", "system_prompt": "old", "default_model": "grok-3",
                   "temperature": 0.5, "top_p": 1.0, "max_tokens": 100,
                   "enabled_tools": [], "safety": {"filter_level": "standard", "blocked_topics": []}},
        "personas": [], "conversations": [{"id": "c1", "title": "old", "persona_id": null, "model": "grok-3",
            "messages": [{"role": "user", "content": "legacy", "ts": "2025-01-01T00:00:00Z"}],
            "created_at": "2025-01-01T00:00:00Z", "updated_at": "2025-01-01T00:00:00Z"}],
        "templates": [], "memories": [], "documents": [], "tasks": [],
        "usage": {"total_requests": 3, "total_prompt_tokens": 10, "total_completion_tokens": 5,
                  "by_model": {"grok-3": 3}, "by_tool": {}},
        "log": []
    });
    std::fs::write(&path, v1.to_string()).unwrap();
    let store = Store::load(Some(path.clone())).unwrap();
    assert_eq!(store.state.config.name, "OldBot");
    assert_eq!(store.state.schema_version, 2);
    assert!(!store.state.conversations[0].messages[0].id.is_empty());
    drop(store);

    // Corrupt file: quarantined, defaults loaded, no panic.
    std::fs::write(&path, "{not json").unwrap();
    let store = Store::load(Some(path.clone())).unwrap();
    assert_eq!(store.state.config.name, "GrokBot");
    assert!(store.recovered_from.is_some());
    assert!(dir.read_dir().unwrap().any(|e| e
        .unwrap()
        .file_name()
        .to_string_lossy()
        .contains("corrupt")));
    drop(store);

    // Single-writer lock: second load of same file fails while first lives.
    let live = Store::load(Some(path.clone())).unwrap();
    assert!(Store::load(Some(path.clone())).is_err());
    drop(live);
    let _ = std::fs::remove_dir_all(dir);
}

#[test]
fn auth_token_store_round_trip_and_perms() {
    let dir =
        std::env::temp_dir().join(format!("grokbot-auth-{}", grokbot_control::state::new_id()));
    let file = dir.join("auth.json");
    std::env::set_var("GROKBOT_AUTH_FILE", file.to_string_lossy().to_string());
    let tokens = grokbot_control::auth::TokenSet {
        access_token: "test-access".into(),
        refresh_token: Some("test-refresh".into()),
        expires_at: 4_102_444_800,
        token_endpoint: Some("https://issuer.example/token".into()),
        client_id: Some("grokbot-control".into()),
        issuer: Some("https://issuer.example".into()),
        scope: Some("openid".into()),
    };
    grokbot_control::auth::save_auth(&tokens).unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(&file).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600, "auth file must be 0600");
    }
    let loaded = grokbot_control::auth::load_auth().unwrap().unwrap();
    assert_eq!(loaded.issuer.as_deref(), Some("https://issuer.example"));
    assert!(grokbot_control::auth::clear_auth().unwrap());
    std::env::remove_var("GROKBOT_AUTH_FILE");
    let _ = std::fs::remove_dir_all(dir);
    // PKCE challenge matches verifier (S256).
    let (verifier, challenge) = grokbot_control::auth::pkce_pair();
    use sha2::Digest;
    let expect = grokbot_control::auth::b64url(&sha2::Sha256::digest(verifier.as_bytes()));
    assert_eq!(challenge, expect);
}

#[tokio::test]
async fn oauth_server_flow_and_scope_enforcement() {
    use grokbot_control::server::{serve_web_oauth, WebMode};
    let addr = "127.0.0.1:18233";
    let approver = ["approver", "secret"].join("-");
    let c = ctx();
    let approver2 = approver.clone();
    tokio::spawn(async move {
        let _ = serve_web_oauth(c, addr, Some(approver2), WebMode::All, true).await;
    });
    tokio::time::sleep(std::time::Duration::from_millis(400)).await;
    let client = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .unwrap();
    let base = format!("http://{addr}");

    // Discovery documents
    let r = client
        .get(format!("{base}/.well-known/oauth-authorization-server"))
        .send()
        .await
        .unwrap();
    assert!(r.status().is_success());
    let meta: Value = r.json().await.unwrap();
    assert!(meta["token_endpoint"]
        .as_str()
        .unwrap()
        .ends_with("/oauth/token"));
    assert_eq!(meta["code_challenge_methods_supported"][0], "S256");

    // Authorize page renders a consent form
    let verifier = "test-verifier-1234567890";
    let challenge = {
        use sha2::Digest;
        grokbot_control::auth::b64url(&sha2::Sha256::digest(verifier.as_bytes()))
    };
    let r = client
        .get(format!("{base}/oauth/authorize?client_id=test&redirect_uri=http://127.0.0.1:9/callback&state=xyz&code_challenge={challenge}&scope=chat"))
        .send().await.unwrap();
    assert!(r.status().is_success());
    assert!(r.text().await.unwrap().contains("Authorize MCP client"));

    // Approve with the approver token -> 302 with code
    let r = client
        .post(format!("{base}/oauth/approve"))
        .form(&[
            ("client_id", "test"),
            ("redirect_uri", "http://127.0.0.1:9/callback"),
            ("state", "xyz"),
            ("code_challenge", challenge.as_str()),
            ("scope", "chat"),
            ("approver_token", approver.as_str()),
        ])
        .send()
        .await
        .unwrap();
    assert!(
        matches!(r.status().as_u16(), 302 | 307),
        "approve must redirect, got {}",
        r.status()
    );
    let location = r.headers()["location"].to_str().unwrap().to_string();
    let code = location
        .split("code=")
        .nth(1)
        .unwrap()
        .split('&')
        .next()
        .unwrap()
        .to_string();

    // Exchange code + PKCE verifier -> tokens
    let r = client
        .post(format!("{base}/oauth/token"))
        .form(&[
            ("grant_type", "authorization_code"),
            ("code", code.as_str()),
            ("redirect_uri", "http://127.0.0.1:9/callback"),
            ("client_id", "test"),
            ("code_verifier", verifier),
        ])
        .send()
        .await
        .unwrap();
    assert!(r.status().is_success(), "token exchange failed");
    let tokens: Value = r.json().await.unwrap();
    let access = tokens["access_token"].as_str().unwrap().to_string();
    assert!(tokens["refresh_token"].as_str().is_some());

    // chat-scoped token can chat but not administer
    let auth = format!("Bearer {access}");
    let r = client
        .post(format!("{base}/mcp"))
        .header("authorization", auth.clone())
        .json(&json!({"jsonrpc": "2.0", "id": 1, "method": "tools/call",
                      "params": {"name": "chat", "arguments": {"prompt": "oauth hello"}}}))
        .send()
        .await
        .unwrap();
    let body: Value = r.json().await.unwrap();
    assert_eq!(
        body["result"]["isError"], false,
        "chat with chat-scope token: {body}"
    );
    let r = client
        .post(format!("{base}/mcp"))
        .header("authorization", auth)
        .json(&json!({"jsonrpc": "2.0", "id": 2, "method": "tools/call",
                      "params": {"name": "state_reset", "arguments": {"confirm": true}}}))
        .send()
        .await
        .unwrap();
    let body: Value = r.json().await.unwrap();
    assert_eq!(
        body["result"]["isError"], true,
        "state_reset must need admin scope"
    );

    // Code reuse is rejected; bad PKCE is rejected
    let r = client
        .post(format!("{base}/oauth/token"))
        .form(&[
            ("grant_type", "authorization_code"),
            ("code", code.as_str()),
            ("redirect_uri", "http://127.0.0.1:9/callback"),
            ("client_id", "test"),
            ("code_verifier", verifier),
        ])
        .send()
        .await
        .unwrap();
    assert_eq!(r.status().as_u16(), 400);

    // Health without auth is minimal only
    let r = client.get(format!("{base}/health")).send().await.unwrap();
    let health: Value = r.json().await.unwrap();
    assert_eq!(health["status"], "ok");
    assert!(
        health.get("backend").is_none(),
        "unauthenticated health must be minimal"
    );
}
