//! MCP JSON-RPC 2.0 dispatch: initialize, ping, tools, resources, prompts.

use crate::tools::{self, Ctx};
use crate::{SERVER_NAME, SUPPORTED_PROTOCOLS, VERSION};

pub const LATEST_PROTOCOL: &str = "2025-06-18";

/// Per-connection protocol state (kept by transports, not in Ctx).
#[derive(Debug, Clone, Default)]
pub struct Conn {
    pub stage: u8, // 0 = new, 1 = initialized (initialize seen), 2 = ready
    pub protocol: String,
    pub log_level: String,
}
use serde_json::{json, Value};

fn ok(id: &Value, result: Value) -> Value {
    json!({"jsonrpc": "2.0", "id": id, "result": result})
}
fn err(id: &Value, code: i64, message: &str) -> Value {
    json!({"jsonrpc": "2.0", "id": id, "error": {"code": code, "message": message}})
}
fn text_result(payload: &Value) -> Value {
    let text = if payload.is_string() {
        payload.as_str().unwrap().to_string()
    } else {
        serde_json::to_string_pretty(payload).unwrap_or_default()
    };
    let mut result = json!({"content": [{"type": "text", "text": text}], "isError": false});
    if payload.is_object() {
        result["structuredContent"] = payload.clone();
    }
    result
}

/// Handle one JSON-RPC request with default (ready) connection state.
/// Transports that own a connection should use `handle_conn`.
pub async fn handle(ctx: &Ctx, req: &Value) -> Option<Value> {
    let mut conn = Conn {
        stage: 2,
        protocol: LATEST_PROTOCOL.to_string(),
        log_level: String::new(),
    };
    handle_conn(ctx, &mut conn, req).await
}

/// Handle one JSON-RPC request value. Returns `None` for notifications.
pub async fn handle_conn(ctx: &Ctx, conn: &mut Conn, req: &Value) -> Option<Value> {
    // Envelope validation (finding 12).
    if !req.is_object() {
        return Some(err(
            &Value::Null,
            -32600,
            "Invalid Request: expected a JSON-RPC object",
        ));
    }
    if let Some(v) = req.get("jsonrpc") {
        if v.as_str() != Some("2.0") {
            return Some(err(
                &Value::Null,
                -32600,
                "Invalid Request: jsonrpc must be \"2.0\"",
            ));
        }
    }
    let id = req.get("id").cloned().unwrap_or(Value::Null);
    if !(id.is_null() || id.is_string() || id.is_number())
        || req
            .get("id")
            .is_some_and(|i| i.is_boolean() || i.is_object() || i.is_array())
    {
        return Some(err(
            &Value::Null,
            -32600,
            "Invalid Request: id must be a string, number, or null",
        ));
    }
    let Some(method) = req
        .get("method")
        .and_then(|m| m.as_str())
        .filter(|m| !m.is_empty())
    else {
        // Responses (result/error without method) are ignored.
        if req.get("result").is_some() || req.get("error").is_some() {
            return None;
        }
        return Some(err(
            &id,
            -32600,
            "Invalid Request: missing or invalid method",
        ));
    };
    let method = method.to_string();
    let method = method.as_str();
    let params = req.get("params").cloned().unwrap_or(json!({}));
    let is_notification = req.get("id").is_none() || method.starts_with("notifications/");
    // Initialization gating (stdio/Conn transports; handle() shim is pre-ready).
    if conn.stage < 2 && !matches!(method, "initialize" | "ping") && !is_notification {
        return Some(err(
            &id,
            -32002,
            "Server not initialized: send initialize, then notifications/initialized",
        ));
    }

    let result: std::result::Result<Value, String> = match method {
        "initialize" => {
            let Some(client_ver) = params.get("protocolVersion").and_then(|v| v.as_str()) else {
                return Some(err(
                    &id,
                    -32602,
                    "initialize requires params.protocolVersion (string)",
                ));
            };
            let protocol = if SUPPORTED_PROTOCOLS.contains(&client_ver) {
                client_ver
            } else {
                LATEST_PROTOCOL
            };
            conn.stage = 1;
            conn.protocol = protocol.to_string();
            Ok(json!({
                "protocolVersion": protocol,
                "capabilities": {
                    "tools": {"listChanged": false},
                    "resources": {"subscribe": false, "listChanged": false},
                    "prompts": {"listChanged": false}
                },
                "serverInfo": {"name": SERVER_NAME, "version": VERSION},
                "instructions": "You are connected to GrokBot Control MCP. Use bot_get_profile and diagnostics first. \
                                 Chat via `chat` or persistent `conversation_*` tools. Manage personas, templates, \
                                 memory, documents, tasks, bot-tools, safety, usage and full state backup from here."
            }))
        }
        "ping" => Ok(json!({})),
        "tools/list" => {
            if params.get("cursor").is_some() {
                return Some(err(
                    &id,
                    -32602,
                    "invalid cursor: this server returns a single page",
                ));
            }
            Ok(json!({"tools": tools::tool_definitions()}))
        }
        "tools/call" => {
            let Some(name) = params
                .get("name")
                .and_then(|v| v.as_str())
                .filter(|n| !n.is_empty())
            else {
                return Some(err(&id, -32602, "tools/call requires params.name (string)"));
            };
            let name = name.to_string();
            let name = name.as_str();
            let args = params.get("arguments").cloned().unwrap_or(json!({}));
            if !args.is_object() {
                return Some(err(&id, -32602, "tools/call arguments must be an object"));
            }
            match tools::call_tool(ctx, name, &args).await {
                Ok(payload) => Ok(text_result(&payload)),
                Err(e) => Ok(json!({
                    "content": [{"type": "text", "text": format!("Error: {e:#}")}],
                    "isError": true
                })),
            }
        }
        "resources/list" => {
            if params.get("cursor").is_some() {
                return Some(err(
                    &id,
                    -32602,
                    "invalid cursor: this server returns a single page",
                ));
            }
            Ok(json!({"resources": tools::resource_definitions()}))
        }
        "resources/templates/list" => Ok(json!({"resourceTemplates": []})),
        "resources/read" => {
            let uri = params.get("uri").and_then(|v| v.as_str()).unwrap_or("");
            match tools::read_resource(ctx, uri).await {
                Ok(text) => Ok(json!({"contents": [
                    {"uri": uri, "mimeType": "application/json", "text": text}]})),
                Err(e) => return Some(err(&id, -32602, &format!("{e:#}"))),
            }
        }
        "prompts/list" => {
            if params.get("cursor").is_some() {
                return Some(err(
                    &id,
                    -32602,
                    "invalid cursor: this server returns a single page",
                ));
            }
            Ok(json!({"prompts": tools::prompt_definitions()}))
        }
        "prompts/get" => {
            let name = params.get("name").and_then(|v| v.as_str()).unwrap_or("");
            let args = params.get("arguments").cloned().unwrap_or(json!({}));
            match tools::get_prompt(name, &args) {
                Ok(p) => Ok(p),
                Err(e) => return Some(err(&id, -32602, &format!("{e:#}"))),
            }
        }
        "logging/setLevel" => {
            conn.log_level = params
                .get("level")
                .and_then(|v| v.as_str())
                .unwrap_or("info")
                .to_string();
            Ok(json!({}))
        }
        "completion/complete" => {
            let arg_val = params["argument"]["value"]
                .as_str()
                .unwrap_or("")
                .to_string();
            let candidates: Vec<String> = {
                let st = ctx.store.lock().await;
                let mut all: Vec<String> =
                    st.state.personas.iter().map(|p| p.name.clone()).collect();
                all.push(st.state.config.default_model.clone());
                all.extend(
                    ["rust", "python", "typescript", "go", "sql"]
                        .iter()
                        .map(|s| s.to_string()),
                );
                all
            };
            let values: Vec<String> = candidates
                .into_iter()
                .filter(|c| c.to_lowercase().starts_with(&arg_val.to_lowercase()))
                .take(50)
                .collect();
            Ok(json!({"completion": {"values": values, "hasMore": false}}))
        }
        _ if is_notification => {
            if method == "notifications/initialized" && conn.stage == 1 {
                conn.stage = 2;
            }
            return None;
        }
        _ => return Some(err(&id, -32601, &format!("method not found: {method}"))),
    };

    if is_notification && method != "initialize" {
        return None;
    }
    match result {
        Ok(r) => Some(ok(&id, r)),
        Err(e) => Some(err(&id, -32603, &e)),
    }
}

/// Handle a single request or a JSON-RPC batch array.
pub async fn handle_any(ctx: &Ctx, req: &Value) -> Option<Value> {
    handle_any_conn(
        ctx,
        &mut Conn {
            stage: 2,
            protocol: LATEST_PROTOCOL.to_string(),
            log_level: String::new(),
        },
        req,
    )
    .await
}

pub async fn handle_any_conn(ctx: &Ctx, conn: &mut Conn, req: &Value) -> Option<Value> {
    if let Some(arr) = req.as_array() {
        if arr.is_empty() || arr.iter().any(|i| !i.is_object()) {
            return Some(err(
                &Value::Null,
                -32600,
                "Invalid Request: batch must be a non-empty array of request objects",
            ));
        }
        if conn.protocol == "2025-06-18" {
            return Some(err(
                &Value::Null,
                -32600,
                "Invalid Request: JSON-RPC batching was removed in protocol 2025-06-18",
            ));
        }
        let mut out = vec![];
        for item in arr {
            if let Some(r) = Box::pin(handle_conn(ctx, conn, item)).await {
                out.push(r);
            }
        }
        if out.is_empty() {
            None
        } else {
            Some(Value::Array(out))
        }
    } else {
        Box::pin(handle_conn(ctx, conn, req)).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::backend::MockBackend;
    use crate::state::Store;
    use std::sync::Arc;

    fn test_ctx() -> Ctx {
        Ctx::new(Store::in_memory(), Arc::new(MockBackend))
    }

    #[tokio::test]
    async fn initialize_ping_and_unknown() {
        let ctx = test_ctx();
        let r = handle(
            &ctx,
            &json!({"jsonrpc": "2.0", "id": 1, "method": "initialize",
            "params": {"protocolVersion": "2024-11-05"}}),
        )
        .await
        .unwrap();
        assert_eq!(r["result"]["protocolVersion"], "2024-11-05");
        assert_eq!(r["result"]["serverInfo"]["name"], SERVER_NAME);

        let r = handle(&ctx, &json!({"jsonrpc": "2.0", "id": 2, "method": "ping"}))
            .await
            .unwrap();
        assert!(r["result"].is_object());

        let r = handle(
            &ctx,
            &json!({"jsonrpc": "2.0", "id": 3, "method": "nope/nope"}),
        )
        .await
        .unwrap();
        assert_eq!(r["error"]["code"], -32601);

        // Notification: no response.
        assert!(handle(
            &ctx,
            &json!({"jsonrpc": "2.0", "method": "notifications/initialized"})
        )
        .await
        .is_none());
    }
}
