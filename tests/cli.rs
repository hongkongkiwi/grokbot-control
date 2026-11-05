//! CLI mode tests: the same engine as MCP, driven through `run_tool`.

use grokbot_control::backend::MockBackend;
use grokbot_control::cli::{build_args, find_tool, run_tool, tool_names};
use grokbot_control::state::Store;
use grokbot_control::tools::Ctx;
use serde_json::json;
use std::sync::Arc;

fn ctx() -> Ctx {
    Ctx::new(Store::in_memory(), Arc::new(MockBackend))
}

#[tokio::test]
async fn run_tool_end_to_end_and_errors() {
    let c = ctx();
    assert_eq!(
        tool_names().len(),
        grokbot_control::tools::tool_definitions().len()
    );

    // Hyphenated name + schema coercion, same as MCP `chat`.
    let (payload, ok) = run_tool(
        &c,
        "chat",
        &[
            "--prompt".into(),
            "cli e2e".into(),
            "--temperature".into(),
            "0.2".into(),
        ],
        None,
    )
    .await;
    assert!(ok);
    assert!(payload["reply"].as_str().unwrap().contains("cli e2e"));

    // Stateful round-trip through the CLI surface.
    let (_stored, ok) = run_tool(
        &c,
        "memory-store",
        &[
            "--key".into(),
            "k".into(),
            "--value".into(),
            "v".into(),
            "--tags".into(),
            "a,b".into(),
        ],
        None,
    )
    .await;
    assert!(ok);
    let (payload, ok) = run_tool(&c, "memory_recall", &[], Some("{\"key\":\"k\"}")).await;
    assert!(ok);
    assert_eq!(payload["memories"][0]["tags"], json!(["a", "b"]));

    // Unknown tool: structured error, success=false (binary exits 1).
    let (payload, ok) = run_tool(&c, "nope", &[], None).await;
    assert!(!ok);
    assert_eq!(payload["ok"], false);

    // Unknown argument: error carries a usage line for self-correction.
    let (payload, ok) = run_tool(&c, "chat", &["--bogus".into(), "x".into()], None).await;
    assert!(!ok);
    assert!(payload["usage"].as_str().unwrap().contains("run chat"));
    assert!(payload["error"].as_str().unwrap().contains("--bogus"));
}

#[test]
fn every_tool_is_cli_callable() {
    // The CLI is schema-driven: each tool must expose a properties map
    // (possibly empty) that build_args can validate against.
    for name in tool_names() {
        let tool = find_tool(&name).expect("tool listed but not findable");
        let args = build_args(&tool, &[], Some("{}"));
        let required = tool["inputSchema"]["required"]
            .as_array()
            .map(|a| a.len())
            .unwrap_or(0);
        if required == 0 {
            assert!(
                args.is_ok(),
                "tool {name} with no required args must build empty args"
            );
        } else {
            assert!(
                args.is_err(),
                "tool {name} must report its missing required args"
            );
        }
    }
}
