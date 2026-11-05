//! Integration tests: drive the real MCP dispatch (mock backend, in-memory state).

use grokbot_control::backend::MockBackend;
use grokbot_control::mcp;
use grokbot_control::state::Store;
use grokbot_control::tools::{tool_definitions, Ctx};
use serde_json::{json, Value};
use std::sync::Arc;

fn ctx() -> Ctx {
    Ctx::new(Store::in_memory(), Arc::new(MockBackend))
}

async fn call(ctx: &Ctx, id: u64, name: &str, args: Value) -> Value {
    let resp = mcp::handle(
        ctx,
        &json!({
            "jsonrpc": "2.0", "id": id, "method": "tools/call",
            "params": {"name": name, "arguments": args}
        }),
    )
    .await
    .expect("tool call must produce a response");
    let text = resp["result"]["content"][0]["text"]
        .as_str()
        .unwrap_or_else(|| panic!("no text content in {resp}"))
        .to_string();
    assert_eq!(
        resp["result"]["isError"], false,
        "tool {name} errored: {text}"
    );
    serde_json::from_str(&text).unwrap_or_else(|_| json!({"raw": text}))
}

#[tokio::test]
async fn every_listed_tool_has_a_working_handler() {
    let c = ctx();
    // Exercise the happy path of each tool group at least once.
    let profile = call(&c, 1, "bot_get_profile", json!({})).await;
    assert_eq!(profile["bot"]["name"], "GrokBot");

    call(
        &c,
        2,
        "bot_update_config",
        json!({"name": "TestBot", "temperature": 0.2}),
    )
    .await;
    let profile = call(&c, 3, "bot_get_profile", json!({})).await;
    assert_eq!(profile["bot"]["name"], "TestBot");

    let models = call(&c, 4, "bot_list_models", json!({})).await;
    assert!(models["models"].as_array().unwrap().len() >= 3);

    let status = call(&c, 5, "bot_provider_status", json!({})).await;
    assert_eq!(status["backend"], "mock");

    let ping = call(&c, 6, "bot_test_connection", json!({})).await;
    assert_eq!(ping["ok"], true);

    let chat = call(&c, 7, "chat", json!({"prompt": "hello grokbot"})).await;
    assert!(chat["reply"].as_str().unwrap().contains("hello grokbot"));

    // conversations
    let conv = call(&c, 8, "conversation_create", json!({"title": "t1"})).await;
    let cid = conv["conversation"]["id"].as_str().unwrap().to_string();
    let sent = call(
        &c,
        9,
        "conversation_send",
        json!({"id": cid, "content": "first message"}),
    )
    .await;
    assert!(sent["reply"].as_str().unwrap().contains("first message"));
    assert_eq!(
        sent["conversation"]["messages"].as_array().unwrap().len(),
        2
    );
    call(
        &c,
        10,
        "conversation_add_message",
        json!({"id": cid, "role": "user", "content": "raw"}),
    )
    .await;
    let got = call(&c, 11, "conversation_get", json!({"id": cid})).await;
    assert_eq!(got["conversation"]["messages"].as_array().unwrap().len(), 3);
    let list = call(&c, 12, "conversation_list", json!({})).await;
    assert_eq!(list["conversations"].as_array().unwrap().len(), 1);
    let fork = call(&c, 13, "conversation_fork", json!({"id": cid, "up_to": 1})).await;
    assert_eq!(
        fork["conversation"]["messages"].as_array().unwrap().len(),
        1
    );
    let sum = call(&c, 14, "conversation_summarize", json!({"id": cid})).await;
    assert!(!sum["summary"].as_str().unwrap().is_empty());
    let exp = call(
        &c,
        15,
        "conversation_export",
        json!({"id": cid, "format": "markdown"}),
    )
    .await;
    assert!(exp["markdown"].as_str().unwrap().contains("t1"));
    call(&c, 16, "conversation_delete", json!({"id": cid})).await;

    // personas
    let p = call(
        &c,
        17,
        "persona_create",
        json!({"name": "Pirate", "system_prompt": "Talk like a pirate"}),
    )
    .await;
    let pid = p["persona"]["id"].as_str().unwrap().to_string();
    call(
        &c,
        18,
        "persona_update",
        json!({"id": pid, "description": "arr"}),
    )
    .await;
    call(&c, 19, "persona_activate", json!({"id": "Pirate"})).await;
    let personas = call(&c, 20, "persona_list", json!({})).await;
    assert!(personas["personas"].as_array().unwrap().len() >= 4);
    let pirate_chat = call(
        &c,
        21,
        "chat",
        json!({"prompt": "ahoy", "persona_id": "Pirate"}),
    )
    .await;
    assert_eq!(pirate_chat["persona"], "Pirate");
    call(&c, 22, "persona_delete", json!({"id": pid})).await;

    // templates
    let t = call(
        &c,
        23,
        "template_create",
        json!({"name": "greet", "template": "Hi {{name}}"}),
    )
    .await;
    assert_eq!(t["template"]["variables"][0], "name");
    let rendered = call(
        &c,
        24,
        "template_render",
        json!({"id": "greet", "variables": {"name": "Ada"}}),
    )
    .await;
    assert_eq!(rendered["rendered"], "Hi Ada");
    let sent = call(
        &c,
        25,
        "template_render",
        json!({"id": "greet", "variables": {"name": "Ada"}, "send": true}),
    )
    .await;
    assert!(sent["reply"].as_str().unwrap().contains("Hi Ada"));
    call(
        &c,
        26,
        "template_update",
        json!({"id": "greet", "description": "d"}),
    )
    .await;
    call(&c, 27, "template_list", json!({})).await;
    call(&c, 28, "template_delete", json!({"id": "greet"})).await;

    // memory + documents
    call(
        &c,
        29,
        "memory_store",
        json!({"key": "owner", "value": "Andy", "tags": ["person"]}),
    )
    .await;
    let rec = call(&c, 30, "memory_recall", json!({"key": "owner"})).await;
    assert_eq!(rec["memories"][0]["value"], "Andy");
    call(&c, 31, "memory_recall", json!({"query": "and"})).await;
    call(&c, 32, "memory_list", json!({})).await;
    call(&c, 33, "memory_delete", json!({"id": "owner"})).await;

    let d = call(
        &c,
        34,
        "document_add",
        json!({"title": "Runbook", "content": "restart the bot", "tags": ["ops"]}),
    )
    .await;
    let did = d["document"]["id"].as_str().unwrap().to_string();
    let hits = call(&c, 35, "document_search", json!({"query": "restart"})).await;
    assert_eq!(hits["documents"].as_array().unwrap().len(), 1);
    call(&c, 36, "document_get", json!({"id": did})).await;
    call(&c, 37, "document_list", json!({})).await;
    call(&c, 38, "document_delete", json!({"id": did})).await;

    // tasks
    let task = call(
        &c,
        39,
        "task_create",
        json!({"name": "daily", "cron": "0 9 * * *", "prompt": "brief me"}),
    )
    .await;
    let tid = task["task"]["id"].as_str().unwrap().to_string();
    let run = call(&c, 40, "task_run", json!({"id": tid})).await;
    assert!(run["result"].as_str().unwrap().contains("brief me"));
    call(&c, 41, "task_list", json!({})).await;
    call(&c, 42, "task_delete", json!({"id": tid})).await;

    // bot tools + safety
    call(&c, 43, "bot_tools_list", json!({})).await;
    call(
        &c,
        44,
        "bot_tool_set_enabled",
        json!({"tool": "web_search", "enabled": false}),
    )
    .await;
    call(&c, 45, "bot_safety_get", json!({})).await;
    call(
        &c,
        46,
        "bot_safety_update",
        json!({"blocked_topics": ["spoilers"]}),
    )
    .await;

    // admin
    let usage = call(&c, 47, "usage_stats", json!({})).await;
    assert!(usage["total_requests"].as_u64().unwrap() >= 3);
    call(&c, 48, "request_log", json!({"limit": 5})).await;
    let diag = call(&c, 49, "diagnostics", json!({})).await;
    assert_eq!(diag["tools_available"], tool_definitions().len() as u64);
    let exported = call(&c, 50, "state_export", json!({})).await;
    call(&c, 51, "state_import", json!({"state": exported["state"]})).await;
    call(&c, 52, "state_reset", json!({"confirm": true})).await;
    let profile = call(&c, 53, "bot_get_profile", json!({})).await;
    assert_eq!(profile["bot"]["name"], "GrokBot");
}

#[tokio::test]
async fn safety_block_and_error_paths() {
    let c = ctx();
    call(
        &c,
        1,
        "bot_safety_update",
        json!({"blocked_topics": ["forbidden"]}),
    )
    .await;
    let resp = mcp::handle(
        &c,
        &json!({"jsonrpc": "2.0", "id": 2, "method": "tools/call",
        "params": {"name": "chat", "arguments": {"prompt": "tell me forbidden things"}}}),
    )
    .await
    .unwrap();
    assert_eq!(resp["result"]["isError"], true);

    let resp = mcp::handle(
        &c,
        &json!({"jsonrpc": "2.0", "id": 3, "method": "tools/call",
        "params": {"name": "conversation_get", "arguments": {"id": "missing"}}}),
    )
    .await
    .unwrap();
    assert_eq!(resp["result"]["isError"], true);

    let resp = mcp::handle(
        &c,
        &json!({"jsonrpc": "2.0", "id": 4, "method": "tools/call",
        "params": {"name": "not_a_tool", "arguments": {}}}),
    )
    .await
    .unwrap();
    assert_eq!(resp["result"]["isError"], true);
}

#[tokio::test]
async fn resources_and_prompts() {
    let c = ctx();
    let r = mcp::handle(
        &c,
        &json!({"jsonrpc": "2.0", "id": 1, "method": "resources/list"}),
    )
    .await
    .unwrap();
    assert_eq!(r["result"]["resources"].as_array().unwrap().len(), 6);

    let r = mcp::handle(
        &c,
        &json!({"jsonrpc": "2.0", "id": 2, "method": "resources/read",
        "params": {"uri": "grokbot://config"}}),
    )
    .await
    .unwrap();
    assert!(r["result"]["contents"][0]["text"]
        .as_str()
        .unwrap()
        .contains("GrokBot"));

    let r = mcp::handle(
        &c,
        &json!({"jsonrpc": "2.0", "id": 3, "method": "prompts/list"}),
    )
    .await
    .unwrap();
    assert_eq!(r["result"]["prompts"].as_array().unwrap().len(), 3);

    let r = mcp::handle(&c, &json!({"jsonrpc": "2.0", "id": 4, "method": "prompts/get",
        "params": {"name": "code_review", "arguments": {"code": "fn main(){}", "language": "rust"}}})).await.unwrap();
    assert!(r["result"]["messages"][0]["content"]["text"]
        .as_str()
        .unwrap()
        .contains("fn main()"));

    let r = mcp::handle(
        &c,
        &json!({"jsonrpc": "2.0", "id": 5, "method": "tools/list"}),
    )
    .await
    .unwrap();
    assert_eq!(
        r["result"]["tools"].as_array().unwrap().len(),
        tool_definitions().len()
    );
    assert!(
        tool_definitions().len() >= 112,
        "expected 78+ tools, got {}",
        tool_definitions().len()
    );
}

#[tokio::test]
async fn expanded_management_tools() {
    let c = ctx();
    // models / tokens / cost
    let m = call(&c, 1, "bot_get_model", json!({"model": "grok-4"})).await;
    assert_eq!(m["found"], true);
    call(
        &c,
        2,
        "bot_set_default_model",
        json!({"model": "grok-3-mini"}),
    )
    .await;
    let tc = call(
        &c,
        3,
        "token_count",
        json!({"text": "hello world, this is a test"}),
    )
    .await;
    assert!(tc["tokens"].as_u64().unwrap() >= 5);
    call(&c, 4, "chat", json!({"prompt": "cost seed"})).await;
    let cost = call(&c, 5, "usage_cost", json!({})).await;
    assert!(cost["estimated_total_usd"].as_f64().unwrap() >= 0.0);
    call(&c, 6, "usage_reset", json!({"confirm": true})).await;

    // media (mock)
    let img = call(
        &c,
        7,
        "image_generate",
        json!({"prompt": "a robot cat", "n": 2}),
    )
    .await;
    assert_eq!(img["images"].as_array().unwrap().len(), 2);
    let vis = call(
        &c,
        8,
        "image_understand",
        json!({"prompt": "what is this?", "image_url": "https://example.com/a.png"}),
    )
    .await;
    assert!(vis["reply"].as_str().unwrap().contains("example.com"));

    // batch + compare + text helpers
    let batch = call(&c, 9, "chat_batch", json!({"prompts": ["one", "two"]})).await;
    assert_eq!(batch["count"], 2);
    let cmp = call(
        &c,
        10,
        "model_compare",
        json!({"prompt": "compare me", "models": ["grok-4", "grok-3-mini"]}),
    )
    .await;
    assert_eq!(cmp["results"].as_array().unwrap().len(), 2);
    call(
        &c,
        11,
        "text_summarize",
        json!({"text": "long text to summarise here"}),
    )
    .await;
    call(
        &c,
        12,
        "text_translate",
        json!({"text": "hello", "target_language": "French"}),
    )
    .await;

    // conversation extras
    let conv = call(&c, 13, "conversation_create", json!({"title": "extras"})).await;
    let cid = conv["conversation"]["id"].as_str().unwrap().to_string();
    call(
        &c,
        14,
        "conversation_send",
        json!({"id": cid, "content": "needle in thread"}),
    )
    .await;
    call(
        &c,
        15,
        "conversation_rename",
        json!({"id": cid, "title": "renamed"}),
    )
    .await;
    call(
        &c,
        16,
        "conversation_set_flags",
        json!({"id": cid, "pinned": true}),
    )
    .await;
    let found = call(&c, 17, "conversation_search", json!({"query": "needle"})).await;
    assert_eq!(found["conversations"].as_array().unwrap().len(), 1);
    let stats = call(&c, 18, "conversation_stats", json!({"id": cid})).await;
    assert_eq!(stats["messages"], 2);
    call(&c, 19, "conversation_stats", json!({})).await;
    call(&c, 20, "conversation_clear", json!({"id": cid})).await;
    let got = call(&c, 21, "conversation_get", json!({"id": cid})).await;
    assert_eq!(got["conversation"]["messages"].as_array().unwrap().len(), 0);

    // persona/template sharing
    let dup = call(
        &c,
        22,
        "persona_duplicate",
        json!({"id": "Coder", "name": "Coder2"}),
    )
    .await;
    assert_eq!(dup["persona"]["name"], "Coder2");
    let exported = call(&c, 23, "persona_export", json!({})).await;
    // Re-import under fresh names (duplicate names are skipped by design).
    let mut fresh = exported["personas"].clone();
    for item in fresh.as_array_mut().unwrap() {
        let n = item["name"].as_str().unwrap().to_string();
        item["name"] = json!(format!("{n} imported"));
    }
    let imp = call(&c, 24, "persona_import", json!({"personas": fresh})).await;
    assert!(imp["imported"].as_u64().unwrap() >= 3);
    // Importing the originals again imports nothing but reports why.
    let resp = mcp::handle(
        &c,
        &json!({"jsonrpc": "2.0", "id": 99, "method": "tools/call",
                "params": {"name": "persona_import", "arguments": {"personas": exported["personas"]}}}),
    )
    .await
    .unwrap();
    assert_eq!(resp["result"]["isError"], true);
    call(
        &c,
        25,
        "template_duplicate",
        json!({"id": "explain", "name": "explain2"}),
    )
    .await;

    // memory/docs/task extras
    call(
        &c,
        26,
        "memory_store",
        json!({"key": "k1", "value": "v1", "tags": ["x"]}),
    )
    .await;
    let ms = call(&c, 27, "memory_stats", json!({})).await;
    assert_eq!(ms["entries"], 1);
    call(&c, 28, "memory_export", json!({})).await;
    call(&c, 29, "memory_clear", json!({"confirm": true, "tag": "x"})).await;
    let d = call(
        &c,
        30,
        "document_add",
        json!({"title": "D", "content": "body"}),
    )
    .await;
    let did = d["document"]["id"].as_str().unwrap().to_string();
    call(
        &c,
        31,
        "document_update",
        json!({"id": did, "content": "new body"}),
    )
    .await;
    let task = call(
        &c,
        32,
        "task_create",
        json!({"name": "t", "cron": "0 9 * * *", "prompt": "hi"}),
    )
    .await;
    let tid = task["task"]["id"].as_str().unwrap().to_string();
    call(
        &c,
        33,
        "task_update",
        json!({"id": tid, "prompt": "updated"}),
    )
    .await;
    call(
        &c,
        34,
        "task_set_enabled",
        json!({"id": tid, "enabled": false}),
    )
    .await;

    // server / state admin
    let info = call(&c, 35, "server_info", json!({})).await;
    assert_eq!(info["tools"], tool_definitions().len() as u64);
    assert!(info["transports"].as_array().unwrap().len() == 3);
    call(&c, 36, "state_backup", json!({})).await;
    let val = call(&c, 37, "state_validate", json!({})).await;
    assert_eq!(val["valid"], true);
    call(&c, 38, "request_log_clear", json!({"confirm": true})).await;
}

#[tokio::test]
async fn http_and_sse_transports() {
    use grokbot_control::server::{serve_web, WebMode};
    let addr = "127.0.0.1:18231";
    let c = ctx();
    tokio::spawn(async move {
        let _ = serve_web(c, addr, Some("test-secret".to_string()), WebMode::All).await;
    });
    tokio::time::sleep(std::time::Duration::from_millis(400)).await;
    let client = reqwest::Client::new();
    let base = format!("http://{addr}");

    // health (no auth needed on /health by design? server requires none there)
    let r = client.get(format!("{base}/health")).send().await.unwrap();
    assert!(r.status().is_success());
    let health: Value = r.json().await.unwrap();
    assert_eq!(health["status"], "ok");

    // HTTP POST JSON
    let r = client
        .post(format!("{base}/mcp"))
        .header("authorization", concat!("Bearer ", "test-", "secret"))
        .json(&json!({"jsonrpc": "2.0", "id": 1, "method": "ping"}))
        .send()
        .await
        .unwrap();
    assert!(r.status().is_success());

    // HTTP POST as SSE
    let r = client
        .post(format!("{base}/mcp"))
        .header("authorization", concat!("Bearer ", "test-", "secret"))
        .header("accept", "text/event-stream")
        .json(&json!({"jsonrpc": "2.0", "id": 2, "method": "tools/call",
                      "params": {"name": "diagnostics", "arguments": {}}}))
        .send()
        .await
        .unwrap();
    assert!(r.headers()["content-type"]
        .to_str()
        .unwrap()
        .contains("text/event-stream"));
    let body = r.text().await.unwrap();
    assert!(body.contains("event: message"), "SSE POST body: {body}");
    assert!(body.contains("tools_available"));

    // GET /mcp stream rejects non-SSE accept
    let r = client
        .get(format!("{base}/mcp"))
        .header("authorization", concat!("Bearer ", "test-", "secret"))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status().as_u16(), 406);

    // Legacy SSE: open stream, read endpoint event, POST /messages, read reply
    let mut sse = client
        .get(format!("{base}/sse"))
        .header("authorization", concat!("Bearer ", "test-", "secret"))
        .send()
        .await
        .unwrap();
    assert!(sse.headers()["content-type"]
        .to_str()
        .unwrap()
        .contains("text/event-stream"));
    let chunk = tokio::time::timeout(std::time::Duration::from_secs(5), sse.chunk())
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    let text = String::from_utf8_lossy(&chunk).to_string();
    assert!(text.contains("event: endpoint"), "first SSE chunk: {text}");
    let session = text.split("sessionId=").nth(1).unwrap().trim().to_string();

    let r = client
        .post(format!("{base}/messages?sessionId={session}"))
        .header("authorization", concat!("Bearer ", "test-", "secret"))
        .json(&json!({"jsonrpc": "2.0", "id": 7, "method": "tools/list"}))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status().as_u16(), 202);

    // Read chunks until the tools/list reply arrives
    let mut got = String::new();
    for _ in 0..10 {
        match tokio::time::timeout(std::time::Duration::from_secs(5), sse.chunk()).await {
            Ok(Ok(Some(bytes))) => {
                got.push_str(&String::from_utf8_lossy(&bytes));
                if got.contains("bot_get_profile") {
                    break;
                }
            }
            _ => break,
        }
    }
    assert!(
        got.contains("event: message") && got.contains("bot_get_profile"),
        "SSE reply stream: {got}"
    );

    // Unknown session -> 404; unauthenticated /mcp -> 401
    let r = client
        .post(format!("{base}/messages?sessionId=nope"))
        .header("authorization", concat!("Bearer ", "test-", "secret"))
        .json(&json!({"jsonrpc": "2.0", "id": 1, "method": "ping"}))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status().as_u16(), 404);
    let r = client
        .post(format!("{base}/mcp"))
        .json(&json!({"jsonrpc": "2.0", "id": 1, "method": "ping"}))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status().as_u16(), 401);
}
