//! Comprehensive human/CLI front-end over the exact same tool engine
//! the MCP server exposes. Every MCP tool is callable from the shell:
//!
//! ```text
//! grokbot-control run chat --prompt "hello" --temperature 0.2
//! grokbot-control run conversation_send --id <id> --content "hi"
//! grokbot-control run memory_store --key owner --value Andy --tags person,admin
//! grokbot-control run state_export --args '{"x":1}'   # (extra keys rejected)
//! grokbot-control repl                                # interactive shell
//! ```
//!
//! Arguments are coerced using each tool's own JSON input schema
//! (integer/number/boolean/array/object), so the CLI can never drift
//! from the MCP surface: add a tool and it is instantly CLI-callable.

use crate::tools::{call_tool, tool_definitions, Ctx};
use anyhow::{anyhow, bail, Result};
use serde_json::{json, Map, Value};

pub fn normalize_tool_name(name: &str) -> String {
    name.trim().replace('-', "_")
}

pub fn find_tool(name: &str) -> Option<Value> {
    let want = normalize_tool_name(name);
    tool_definitions()
        .into_iter()
        .find(|t| t["name"].as_str() == Some(want.as_str()))
}

pub fn tool_names() -> Vec<String> {
    tool_definitions()
        .iter()
        .filter_map(|t| t["name"].as_str().map(|s| s.to_string()))
        .collect()
}

fn schema_props(tool: &Value) -> Map<String, Value> {
    tool["inputSchema"]["properties"]
        .as_object()
        .cloned()
        .unwrap_or_default()
}

fn required(tool: &Value) -> Vec<String> {
    tool["inputSchema"]["required"]
        .as_array()
        .map(|a| {
            a.iter()
                .filter_map(|v| v.as_str().map(|s| s.to_string()))
                .collect()
        })
        .unwrap_or_default()
}

/// Coerce one raw CLI string into the JSON type the schema declares.
pub fn coerce(prop_schema: &Value, raw: &str, key: &str) -> Result<Value> {
    let ty = prop_schema["type"].as_str().unwrap_or("");
    match ty {
        "integer" => raw
            .parse::<i64>()
            .map(|n| json!(n))
            .map_err(|_| anyhow!("--{key} expects an integer, got '{raw}'")),
        "number" => raw
            .parse::<f64>()
            .map(|n| json!(n))
            .map_err(|_| anyhow!("--{key} expects a number, got '{raw}'")),
        "boolean" => match raw {
            "true" | "1" | "yes" => Ok(json!(true)),
            "false" | "0" | "no" => Ok(json!(false)),
            _ => bail!("--{key} expects a boolean (true/false), got '{raw}'"),
        },
        "array" => {
            if raw.trim_start().starts_with('[') {
                serde_json::from_str(raw)
                    .map_err(|e| anyhow!("--{key} expects a JSON array or comma list: {e}"))
            } else if raw.is_empty() {
                Ok(json!([]))
            } else {
                Ok(json!(raw
                    .split(',')
                    .map(|s| s.trim().to_string())
                    .collect::<Vec<_>>()))
            }
        }
        "object" => {
            serde_json::from_str(raw).map_err(|e| anyhow!("--{key} expects a JSON object: {e}"))
        }
        _ => Ok(json!(raw)),
    }
}

/// Build a tool-arguments object from `--key value` / `--key=value` pairs,
/// validated and coerced against the tool's schema. Boolean schema keys
/// may be passed as bare `--flag` (true) or `--flag=false`.
pub fn build_args(tool: &Value, pairs: &[String], args_json: Option<&str>) -> Result<Value> {
    let props = schema_props(tool);
    let mut out = Map::new();
    if let Some(raw) = args_json {
        let v: Value =
            serde_json::from_str(raw).map_err(|e| anyhow!("--args is not valid JSON: {e}"))?;
        let obj = v
            .as_object()
            .ok_or_else(|| anyhow!("--args must be a JSON object"))?;
        for (k, val) in obj {
            out.insert(k.clone(), val.clone());
        }
    }
    let mut i = 0;
    while i < pairs.len() {
        let tok = &pairs[i];
        let Some(stripped) = tok.strip_prefix("--") else {
            bail!("unexpected argument '{tok}' (expected --key value)");
        };
        let (key, inline_val) = match stripped.split_once('=') {
            Some((k, v)) => (k.replace('-', "_"), Some(v.to_string())),
            None => (stripped.replace('-', "_"), None),
        };
        let Some(prop_schema) = props.get(&key) else {
            let mut allowed: Vec<&String> = props.keys().collect();
            allowed.sort();
            bail!(
                "unknown argument --{key} for this tool (allowed: {})",
                allowed
                    .iter()
                    .map(|k| format!("--{k}"))
                    .collect::<Vec<_>>()
                    .join(", ")
            );
        };
        let raw = match inline_val {
            Some(v) => v,
            None => {
                if prop_schema["type"].as_str() == Some("boolean") {
                    "true".to_string()
                } else {
                    i += 1;
                    pairs
                        .get(i)
                        .cloned()
                        .ok_or_else(|| anyhow!("--{key} needs a value"))?
                }
            }
        };
        let value = coerce(prop_schema, &raw, &key)?;
        out.insert(key, value);
        i += 1;
    }
    for key in out.keys() {
        if !props.contains_key(key) {
            bail!("unknown argument '{key}' for this tool");
        }
    }
    for req in required(tool) {
        if !out.contains_key(&req) {
            bail!("missing required argument --{req} for this tool");
        }
    }
    Ok(Value::Object(out))
}

/// Run one tool by name with CLI pairs. Returns (payload, success).
pub async fn run_tool(
    ctx: &Ctx,
    name: &str,
    pairs: &[String],
    args_json: Option<&str>,
) -> (Value, bool) {
    let want = normalize_tool_name(name);
    let Some(tool) = find_tool(&want) else {
        let names = tool_names();
        let near: Vec<&String> = names
            .iter()
            .filter(|n| n.contains(&want) || want.contains(n.as_str()))
            .take(8)
            .collect();
        return (
            json!({"ok": false, "error": format!("unknown tool '{want}'"),
                   "did_you_mean": near, "hint": "list all tools: grokbot-control tools --plain"}),
            false,
        );
    };
    let args = match build_args(&tool, pairs, args_json) {
        Ok(a) => a,
        Err(e) => {
            return (
                json!({"ok": false, "error": format!("{e:#}"), "tool": want,
                       "usage": usage_line(&tool)}),
                false,
            )
        }
    };
    match call_tool(ctx, &want, &args).await {
        Ok(payload) => (payload, true),
        Err(e) => (
            json!({"ok": false, "error": format!("{e:#}"), "tool": want}),
            false,
        ),
    }
}

pub fn usage_line(tool: &Value) -> String {
    let props = schema_props(tool);
    let req = required(tool);
    let mut parts = vec![format!("run {}", tool["name"].as_str().unwrap_or("?"))];
    for (k, v) in &props {
        let ty = v["type"].as_str().unwrap_or("string");
        if req.iter().any(|r| r == k) {
            parts.push(format!("--{k} <{ty}>"));
        } else {
            parts.push(format!("[--{k} <{ty}>]"));
        }
    }
    parts.join(" ")
}

pub fn print_result(payload: &Value, compact: bool) {
    if compact {
        println!("{payload}");
    } else {
        println!(
            "{}",
            serde_json::to_string_pretty(payload).unwrap_or_else(|_| payload.to_string())
        );
    }
}

/// Split a REPL line into tokens, honouring single/double quotes.
pub fn tokenize(line: &str) -> Vec<String> {
    let mut out = vec![];
    let mut cur = String::new();
    let mut quote: Option<char> = None;
    let mut has = false;
    for ch in line.chars() {
        match quote {
            Some(q) if ch == q => quote = None,
            Some(_) => cur.push(ch),
            None if ch == '"' || ch == '\'' => {
                quote = Some(ch);
                has = true;
            }
            None if ch.is_whitespace() => {
                if has || !cur.is_empty() {
                    out.push(std::mem::take(&mut cur));
                    has = false;
                }
            }
            None => cur.push(ch),
        }
    }
    if has || !cur.is_empty() {
        out.push(cur);
    }
    out
}

/// Interactive REPL: one tool per line, e.g.
/// `chat --prompt "hello"`, or bare shortcuts `chat hello there`,
/// `tools`, `help`, `quit`.
pub async fn repl(ctx: &Ctx) -> Result<()> {
    use tokio::io::{AsyncBufReadExt, BufReader};
    eprintln!("GrokBot CLI repl — type `help`, `tools`, `<tool> --arg value`, or `quit`.");
    let mut lines = BufReader::new(tokio::io::stdin()).lines();
    loop {
        eprint!("grokbot> ");
        let Some(line) = lines.next_line().await? else {
            break;
        };
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        if matches!(line, "quit" | "exit" | "q") {
            break;
        }
        if line == "help" {
            eprintln!(
                "  tools                      list every tool\n  \
                 <tool> --key value         run a tool (e.g. chat --prompt \"hi\")\n  \
                 chat <text...>             shortcut for the chat tool\n  \
                 status                     shortcut for diagnostics\n  \
                 quit                       exit"
            );
            continue;
        }
        if line == "tools" {
            println!("{}", tool_names().join("\n"));
            continue;
        }
        let tokens = tokenize(line);
        if tokens.is_empty() {
            continue;
        }
        // Bare shortcuts: `chat hello world`, `status`
        if tokens[0] == "status" {
            let (payload, _) = run_tool(ctx, "diagnostics", &[], None).await;
            print_result(&payload, false);
            continue;
        }
        if tokens[0] == "chat" && (tokens.len() == 1 || !tokens[1].starts_with("--")) {
            let text = tokens[1..].join(" ");
            let pairs = if text.is_empty() {
                vec![]
            } else {
                vec!["--prompt".to_string(), text]
            };
            let (payload, _) = run_tool(ctx, "chat", &pairs, None).await;
            print_result(&payload, false);
            continue;
        }
        let (payload, _) = run_tool(ctx, &tokens[0], &tokens[1..], None).await;
        print_result(&payload, false);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn coerce_types() {
        let t = find_tool("conversation_send").unwrap();
        let args = build_args(
            &t,
            &["--id".into(), "abc".into(), "--content".into(), "hi".into()],
            None,
        )
        .unwrap();
        assert_eq!(args["id"], "abc");

        let t = find_tool("bot_tool_set_enabled").unwrap();
        let args = build_args(
            &t,
            &["--tool".into(), "web_search".into(), "--enabled".into()],
            None,
        )
        .unwrap();
        assert_eq!(args["enabled"], true);
        let args = build_args(
            &t,
            &["--tool=web_search".into(), "--enabled=false".into()],
            None,
        )
        .unwrap();
        assert_eq!(args["enabled"], false);

        let t = find_tool("memory_store").unwrap();
        let args = build_args(
            &t,
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
        .unwrap();
        assert_eq!(args["tags"], json!(["a", "b"]));
    }

    #[test]
    fn build_args_rejects_unknown_and_missing() {
        let t = find_tool("chat").unwrap();
        assert!(build_args(&t, &["--nope".into(), "x".into()], None).is_err());
        assert!(build_args(&t, &[], None).is_err()); // missing --prompt
        let ok = build_args(&t, &[], Some("{\"prompt\":\"from json\"}")).unwrap();
        assert_eq!(ok["prompt"], "from json");
    }

    #[test]
    fn tokenize_quotes() {
        assert_eq!(
            tokenize("chat --prompt \"hello world\" --temperature 0.2"),
            vec!["chat", "--prompt", "hello world", "--temperature", "0.2"]
        );
        assert_eq!(tokenize("chat 'a b'"), vec!["chat", "a b"]);
    }
}
