//! All MCP tools for managing GrokBot, plus resources and prompts data.

use crate::backend::{est_cost_usd, est_tokens, pricing, ChatReply, ChatRequest, GrokBackend};
use crate::state::*;
use anyhow::{anyhow, bail, Context, Result};
use serde_json::{json, Map, Value};
use std::collections::BTreeMap;
use std::sync::Arc;
use tokio::sync::Mutex;

#[derive(Debug, Clone)]
pub struct Job {
    pub id: String,
    pub kind: String,
    pub status: String, // running | done | failed | cancelled
    pub result: Option<Value>,
    pub created_at: chrono::DateTime<chrono::Utc>,
}

pub struct Ctx {
    pub store: Arc<Mutex<Store>>,
    pub backend: Arc<dyn GrokBackend>,
    pub started_at: chrono::DateTime<chrono::Utc>,
    /// OAuth/static-token scopes for this caller (all scopes for local).
    pub caller_scopes: Vec<String>,
    pub conv_locks: Arc<Mutex<std::collections::HashMap<String, Arc<Mutex<()>>>>>,
    pub jobs: Arc<Mutex<std::collections::HashMap<String, Job>>>,
    pub cancels: Arc<Mutex<std::collections::HashMap<String, Arc<tokio::sync::Notify>>>>,
}

impl Clone for Ctx {
    fn clone(&self) -> Self {
        Self {
            store: self.store.clone(),
            backend: self.backend.clone(),
            started_at: self.started_at,
            caller_scopes: self.caller_scopes.clone(),
            conv_locks: self.conv_locks.clone(),
            jobs: self.jobs.clone(),
            cancels: self.cancels.clone(),
        }
    }
}

impl Ctx {
    pub fn new(store: Store, backend: Arc<dyn GrokBackend>) -> Self {
        Self {
            store: Arc::new(Mutex::new(store)),
            backend,
            started_at: chrono::Utc::now(),
            caller_scopes: crate::auth::ALL_SCOPES
                .iter()
                .map(|s| s.to_string())
                .collect(),
            conv_locks: Arc::new(Mutex::new(std::collections::HashMap::new())),
            jobs: Arc::new(Mutex::new(std::collections::HashMap::new())),
            cancels: Arc::new(Mutex::new(std::collections::HashMap::new())),
        }
    }
    pub fn with_scopes(mut self, scopes: Vec<String>) -> Self {
        self.caller_scopes = scopes;
        self
    }
    pub async fn conv_lock(&self, id: &str) -> Arc<Mutex<()>> {
        let mut map = self.conv_locks.lock().await;
        map.entry(id.to_string())
            .or_insert_with(|| Arc::new(Mutex::new(())))
            .clone()
    }
    pub fn uptime_seconds(&self) -> i64 {
        (chrono::Utc::now() - self.started_at).num_seconds().max(0)
    }
}

// ---------- argument helpers ----------

pub fn s(args: &Value, key: &str) -> Option<String> {
    args.get(key)
        .and_then(|v| v.as_str())
        .map(|v| v.to_string())
        .filter(|v| !v.is_empty())
}
pub fn req_s(args: &Value, key: &str) -> Result<String> {
    s(args, key).ok_or_else(|| anyhow!("missing required string argument '{key}'"))
}
pub fn b(args: &Value, key: &str, default: bool) -> bool {
    args.get(key).and_then(|v| v.as_bool()).unwrap_or(default)
}
pub fn f(args: &Value, key: &str) -> Option<f64> {
    args.get(key).and_then(|v| v.as_f64())
}
pub fn u(args: &Value, key: &str) -> Option<u64> {
    args.get(key).and_then(|v| v.as_u64())
}
pub fn str_list(args: &Value, key: &str) -> Vec<String> {
    args.get(key)
        .and_then(|v| v.as_array())
        .map(|a| {
            a.iter()
                .filter_map(|v| v.as_str().map(|s| s.to_string()))
                .collect()
        })
        .unwrap_or_default()
}
fn schema(props: Value, required: &[&str]) -> Value {
    json!({"type": "object", "properties": props, "required": required, "additionalProperties": false})
}
fn tool(name: &str, desc: &str, input: Value) -> Value {
    json!({"name": name, "description": desc, "inputSchema": input})
}

/// Shared chat path: central safety, budget/rate gates, usage + log.
/// Every chat-like tool funnels through here, so safety cannot be
/// bypassed by picking a different tool (review finding 6).
pub async fn do_chat(
    ctx: &Ctx,
    tool: &str,
    messages: Vec<Message>,
    model: String,
    temperature: f64,
    max_tokens: u32,
) -> Result<ChatReply> {
    let mut req = ChatRequest::new(model.clone(), messages, temperature, max_tokens);
    {
        let st = ctx.store.lock().await;
        let cfg = &st.state.config;
        // Safety on every outbound user message.
        let joined = req
            .messages
            .iter()
            .filter(|m| m.role == "user")
            .map(|m| m.content.clone())
            .collect::<Vec<_>>()
            .join("\n");
        check_safety(&st.state, &joined)?;
        // Daily budget gate (estimated from recorded per-model usage).
        if let Some(cap) = cfg.daily_budget_usd {
            let spent: f64 = st
                .state
                .usage
                .by_model_tokens
                .iter()
                .map(|(m, tk)| {
                    crate::backend::est_cost_usd(m, tk.prompt_tokens, tk.completion_tokens)
                        + crate::backend::image_cost_usd(m, tk.images)
                })
                .sum();
            if spent >= cap {
                bail!("daily budget reached: estimated ${spent:.4} >= cap ${cap:.2} (usage_cost for detail; raise via bot config daily_budget_usd)");
            }
        }
        req.top_p = Some(cfg.top_p);
        req.bot_tools = cfg.bot_tool_defs.clone();
        if let Some(p) = crate::tools::active_persona_system(&st.state) {
            let _ = p;
        }
    }
    let mut reply = ctx.backend.chat(req).await;
    {
        let mut store = ctx.store.lock().await;
        // Truncate over-long replies when a cap is configured.
        if let Ok(r) = &mut reply {
            if let Some(cap) = store.state.config.safety.max_response_chars {
                if r.content.chars().count() > cap {
                    r.content = r.content.chars().take(cap).collect();
                }
            }
        }
        match &reply {
            Ok(r) => {
                store.record_usage(&r.model, r.prompt_tokens, r.completion_tokens);
                store.record(
                    tool,
                    ctx.backend.name(),
                    Some(r.model.clone()),
                    true,
                    format!("{}+{} tokens", r.prompt_tokens, r.completion_tokens),
                );
            }
            Err(e) => store.record(tool, ctx.backend.name(), Some(model), false, e.to_string()),
        }
        if let Err(e) = store.save() {
            if reply.is_ok() {
                return Err(e);
            }
        }
    }
    reply
}

/// Chat with a fully-built request (search/tools/format already set).
pub async fn do_chat_req(ctx: &Ctx, tool: &str, req: ChatRequest) -> Result<ChatReply> {
    let model = req.model.clone();
    let mut req = req;
    {
        let st = ctx.store.lock().await;
        let joined = req
            .messages
            .iter()
            .filter(|m| m.role == "user")
            .map(|m| m.content.clone())
            .collect::<Vec<_>>()
            .join("\n");
        check_safety(&st.state, &joined)?;
        if req.top_p.is_none() {
            req.top_p = Some(st.state.config.top_p);
        }
        if req.bot_tools.is_empty() {
            req.bot_tools = st.state.config.bot_tool_defs.clone();
        }
    }
    let mut reply = ctx.backend.chat(req).await;
    let mut store = ctx.store.lock().await;
    if let Ok(r) = &mut reply {
        if let Some(cap) = store.state.config.safety.max_response_chars {
            if r.content.chars().count() > cap {
                r.content = r.content.chars().take(cap).collect();
            }
        }
    }
    match &reply {
        Ok(r) => {
            store.record_usage(&r.model, r.prompt_tokens, r.completion_tokens);
            store.record(
                tool,
                ctx.backend.name(),
                Some(r.model.clone()),
                true,
                "chat_req".to_string(),
            );
        }
        Err(e) => store.record(tool, ctx.backend.name(), Some(model), false, e.to_string()),
    }
    if let Err(e) = store.save() {
        if reply.is_ok() {
            return Err(e);
        }
    }
    reply
}

#[derive(Debug, Clone)]
pub struct Resolved {
    pub system: String,
    pub model: String,
    pub temperature: f64,
    pub top_p: f64,
    pub max_tokens: u32,
    pub persona_name: Option<String>,
}

/// One precedence for every entry point (review finding: resolve split):
/// explicit arg > conversation persona > named persona > active
/// persona > bot config.
pub fn resolve_for(
    state: &AppState,
    args: &Value,
    conv_persona_id: Option<&str>,
    conv_model: Option<&str>,
) -> Resolved {
    let persona = s(args, "persona_id")
        .and_then(|id| state.find_persona(&id).cloned())
        .or_else(|| conv_persona_id.and_then(|id| state.find_persona(id).cloned()))
        .or_else(|| {
            state
                .config
                .active_persona_id
                .as_ref()
                .and_then(|id| state.find_persona(id).cloned())
        });
    let system = s(args, "system")
        .or_else(|| persona.as_ref().map(|p| p.system_prompt.clone()))
        .unwrap_or_else(|| state.config.system_prompt.clone());
    let model = s(args, "model")
        .or_else(|| persona.as_ref().and_then(|p| p.model.clone()))
        .or_else(|| conv_model.map(|m| m.to_string()))
        .unwrap_or_else(|| state.config.default_model.clone());
    let temperature = f(args, "temperature")
        .or_else(|| persona.as_ref().and_then(|p| p.temperature))
        .unwrap_or(state.config.temperature);
    let max_tokens = u(args, "max_tokens")
        .map(|v| v as u32)
        .unwrap_or(state.config.max_tokens);
    Resolved {
        system,
        model,
        temperature,
        top_p: state.config.top_p,
        max_tokens,
        persona_name: persona.map(|p| p.name),
    }
}

/// Resolve (system_prompt, model, temperature, max_tokens) from args + persona + config.
pub fn resolve(ctx_state: &AppState, args: &Value) -> (String, String, f64, u32) {
    let r = resolve_for(ctx_state, args, None, None);
    (r.system, r.model, r.temperature, r.max_tokens)
}

pub fn active_persona_system(state: &AppState) -> Option<String> {
    state
        .config
        .active_persona_id
        .as_ref()
        .and_then(|id| state.find_persona(id))
        .map(|p| p.system_prompt.clone())
}

pub fn tool_definitions() -> Vec<Value> {
    let empty = schema(json!({}), &[]);
    let id_name = schema(
        json!({"id": {"type": "string", "description": "ID or name"}}),
        &["id"],
    );
    vec![
        // ---- bot identity / config / provider ----
        tool("bot_get_profile", "Get GrokBot's full profile: name, description, system prompt, default model, sampling, active persona, enabled bot-tools, safety settings.", empty.clone()),
        tool("bot_update_config", "Update any subset of GrokBot config: name, description, system_prompt, default_model (validated), temperature (0-2), top_p, max_tokens, daily_budget_usd, rate_limit_per_minute, max_history_messages.", schema(json!({
            "name": {"type": "string"}, "description": {"type": "string"}, "system_prompt": {"type": "string"},
            "default_model": {"type": "string"}, "temperature": {"type": "number"}, "top_p": {"type": "number"},
            "max_tokens": {"type": "integer"}, "daily_budget_usd": {"type": "number"},
            "rate_limit_per_minute": {"type": "integer"}, "max_history_messages": {"type": "integer"}}), &[])),
        tool("bot_list_models", "List Grok models available to GrokBot (live from xAI when connected, otherwise the known catalogue).", empty.clone()),
        tool("bot_provider_status", "Show backend status: mock vs live xAI, base URL mode, whether XAI_API_KEY is set, state file path.", empty.clone()),
        tool("bot_test_connection", "Send a tiny ping chat to the configured backend and report latency/model. Proves the Grok path works.", schema(json!({"prompt": {"type": "string"}}), &[])),
        // ---- chat ----
        tool("chat", "One-shot chat with GrokBot. Uses active/given persona, system prompt, model and sampling from args or bot config.", schema(json!({
            "prompt": {"type": "string"}, "system": {"type": "string"}, "persona_id": {"type": "string", "description": "Persona ID or name"},
            "model": {"type": "string"}, "temperature": {"type": "number"}, "max_tokens": {"type": "integer"}}), &["prompt"])),
        // ---- conversations ----
        tool("conversation_create", "Create a persistent multi-turn conversation (optionally bound to a persona).", schema(json!({
            "title": {"type": "string"}, "persona_id": {"type": "string"}, "model": {"type": "string"}}), &[])),
        tool("conversation_list", "List conversations with message counts and last update (no full history).", empty.clone()),
        tool("conversation_get", "Get one conversation with full message history by ID.", schema(json!({"id": {"type": "string"}}), &["id"])),
        tool("conversation_send", "Append a user message to a conversation and get GrokBot's reply (full history is sent as context).", schema(json!({
            "id": {"type": "string"}, "content": {"type": "string"}, "model": {"type": "string"},
            "temperature": {"type": "number"}, "max_tokens": {"type": "integer"}}), &["id", "content"])),
        tool("conversation_add_message", "Append a raw message (any role) to a conversation without generating a reply.", schema(json!({
            "id": {"type": "string"}, "role": {"type": "string", "enum": ["system", "user", "assistant", "tool"]},
            "content": {"type": "string"}}), &["id", "role", "content"])),
        tool("conversation_fork", "Fork a conversation into a new one, optionally keeping only the first N messages.", schema(json!({
            "id": {"type": "string"}, "title": {"type": "string"}, "up_to": {"type": "integer"}}), &["id"])),
        tool("conversation_summarize", "Summarise a conversation with GrokBot and store the summary as an assistant message (store=false to skip).", schema(json!({
            "id": {"type": "string"}, "store": {"type": "boolean"}}), &["id"])),
        tool("conversation_export", "Export a conversation as markdown or json.", schema(json!({
            "id": {"type": "string"}, "format": {"type": "string", "enum": ["markdown", "json"]}}), &["id"])),
        tool("conversation_delete", "Delete a conversation permanently by ID.", schema(json!({"id": {"type": "string"}}), &["id"])),
        // ---- personas ----
        tool("persona_list", "List all personas (id, name, description, model/temperature overrides).", empty.clone()),
        tool("persona_create", "Create a persona: named system prompt + optional model/temperature overrides.", schema(json!({
            "name": {"type": "string"}, "description": {"type": "string"}, "system_prompt": {"type": "string"},
            "model": {"type": "string"}, "temperature": {"type": "number"}}), &["name", "system_prompt"])),
        tool("persona_update", "Update fields of an existing persona (by ID or name).", schema(json!({
            "id": {"type": "string"}, "name": {"type": "string"}, "description": {"type": "string"},
            "system_prompt": {"type": "string"}, "model": {"type": "string"}, "temperature": {"type": "number"}}), &["id"])),
        tool("persona_delete", "Delete a persona (by ID or name). Clears it as active persona if needed.", id_name.clone()),
        tool("persona_activate", "Make a persona GrokBot's active default persona (by ID or name).", id_name.clone()),
        // ---- prompt templates ----
        tool("template_list", "List prompt templates with their {{variables}}.", empty.clone()),
        tool("template_create", "Create a prompt template using {{variable}} placeholders.", schema(json!({
            "name": {"type": "string"}, "description": {"type": "string"}, "template": {"type": "string"}}), &["name", "template"])),
        tool("template_update", "Update a template's name/description/body (by ID or name).", schema(json!({
            "id": {"type": "string"}, "name": {"type": "string"}, "description": {"type": "string"}, "template": {"type": "string"}}), &["id"])),
        tool("template_delete", "Delete a prompt template (by ID or name).", id_name.clone()),
        tool("template_render", "Render a template with variables; optionally send the rendered prompt to GrokBot (send=true).", schema(json!({
            "id": {"type": "string"}, "variables": {"type": "object"}, "send": {"type": "boolean"},
            "persona_id": {"type": "string"}, "model": {"type": "string"}}), &["id"])),
        // ---- memory ----
        tool("memory_store", "Store a long-term memory entry under a key (upserts by key), with optional tags.", schema(json!({
            "key": {"type": "string"}, "value": {"type": "string"}, "tags": {"type": "array", "items": {"type": "string"}}}), &["key", "value"])),
        tool("memory_recall", "Recall memory by exact key, or search keys/values/tags with query.", schema(json!({
            "key": {"type": "string"}, "query": {"type": "string"}}), &[])),
        tool("memory_list", "List all memory entries (optionally filter by tag).", schema(json!({"tag": {"type": "string"}}), &[])),
        tool("memory_delete", "Delete a memory entry by ID or key.", schema(json!({"id": {"type": "string", "description": "Entry ID or key"}}), &["id"])),
        // ---- documents / knowledge ----
        tool("document_add", "Add a document to GrokBot's local knowledge base.", schema(json!({
            "title": {"type": "string"}, "content": {"type": "string"}, "tags": {"type": "array", "items": {"type": "string"}}}), &["title", "content"])),
        tool("document_list", "List knowledge-base documents (titles/tags, no full content).", empty.clone()),
        tool("document_get", "Get one document with full content by ID.", schema(json!({"id": {"type": "string"}}), &["id"])),
        tool("document_search", "Full-text search over document titles/content/tags.", schema(json!({"query": {"type": "string"}}), &["query"])),
        tool("document_delete", "Delete a knowledge-base document by ID.", schema(json!({"id": {"type": "string"}}), &["id"])),
        // ---- scheduled tasks ----
        tool("task_create", "Create a scheduled task: a prompt + cron expression GrokBot should run (execute via task_run).", schema(json!({
            "name": {"type": "string"}, "cron": {"type": "string"}, "prompt": {"type": "string"}, "persona_id": {"type": "string"}}), &["name", "cron", "prompt"])),
        tool("task_list", "List scheduled tasks with run counts and last results.", empty.clone()),
        tool("task_delete", "Delete a scheduled task by ID.", schema(json!({"id": {"type": "string"}}), &["id"])),
        tool("task_run", "Run a scheduled task now: sends its prompt to GrokBot and stores the result on the task.", schema(json!({"id": {"type": "string"}}), &["id"])),
        // ---- bot tools & safety ----
        tool("bot_tools_list", "List GrokBot chat-time tools (web_search, code_execution, ...) and which are enabled.", empty.clone()),
        tool("bot_tool_set_enabled", "Enable or disable one of GrokBot's chat-time tools.", schema(json!({
            "tool": {"type": "string"}, "enabled": {"type": "boolean"}}), &["tool", "enabled"])),
        tool("bot_safety_get", "Get GrokBot safety settings: filter level, blocked topics, max response chars.", empty.clone()),
        tool("bot_safety_update", "Update safety settings: filter_level (off|standard|strict), blocked_topics, max_response_chars.", schema(json!({
            "filter_level": {"type": "string", "enum": ["off", "standard", "strict"]},
            "blocked_topics": {"type": "array", "items": {"type": "string"}}, "max_response_chars": {"type": "integer"}}), &[])),
        // ---- observability & admin ----
        tool("usage_stats", "Token/request usage totals, broken down by model and by MCP tool.", empty.clone()),
        tool("request_log", "Recent request log (newest last). limit defaults to 50.", schema(json!({"limit": {"type": "integer"}}), &[])),
        tool("diagnostics", "Full health report: backend, state counts, config summary, uptime-ish checks. Start here when something seems wrong.", empty.clone()),
        tool("state_export", "Export the entire GrokBot state (config, personas, conversations, templates, memory, docs, tasks, usage) as JSON.", empty.clone()),
        tool("state_import", "Replace GrokBot state from a previous state_export JSON object. Destructive — export first.", schema(json!({"state": {"type": "object"}}), &["state"])),
        tool("state_reset", "Reset GrokBot to factory defaults (keeps nothing). Requires confirm=true.", schema(json!({"confirm": {"type": "boolean"}}), &["confirm"])),
        // ---- models, cost, media, batch ----
        tool("bot_get_model", "Get details for one Grok model: description, context window, approximate pricing, whether it is live.", schema(json!({"model": {"type": "string"}}), &["model"])),
        tool("bot_set_default_model", "Set GrokBot's default model (validated against the available model list).", schema(json!({"model": {"type": "string"}}), &["model"])),
        tool("token_count", "Estimate token count for a text (or an array of {role, content} messages). Same estimator the mock backend uses.", schema(json!({"text": {"type": "string"}, "messages": {"type": "array"}}), &[])),
        tool("usage_cost", "Estimated USD cost from recorded usage, using approximate per-model pricing. Estimates only.", empty.clone()),
        tool("usage_reset", "Reset usage counters to zero. Requires confirm=true.", schema(json!({"confirm": {"type": "boolean"}}), &["confirm"])),
        tool("image_generate", "Generate image(s) with Grok image models (mock returns mock:// URLs offline; live uses xAI images API).", schema(json!({"prompt": {"type": "string"}, "model": {"type": "string"}, "n": {"type": "integer"}}), &["prompt"])),
        tool("image_understand", "Ask GrokBot about an image URL (vision). Mock describes intent offline; live sends the image to a vision model.", schema(json!({"prompt": {"type": "string"}, "image_url": {"type": "string"}, "model": {"type": "string"}, "max_tokens": {"type": "integer"}}), &["prompt", "image_url"])),
        tool("chat_batch", "Run several one-shot prompts in sequence and return every reply. Optional shared persona_id/model.", schema(json!({"prompts": {"type": "array", "items": {"type": "string"}}, "persona_id": {"type": "string"}, "model": {"type": "string"}}), &["prompts"])),
        tool("model_compare", "Send the same prompt to several models and compare replies, tokens, and estimated cost side by side.", schema(json!({"prompt": {"type": "string"}, "models": {"type": "array", "items": {"type": "string"}}}), &["prompt", "models"])),
        // ---- conversation management extras ----
        tool("conversation_rename", "Rename a conversation.", schema(json!({"id": {"type": "string"}, "title": {"type": "string"}}), &["id", "title"])),
        tool("conversation_set_flags", "Set pinned and/or archived flags on a conversation.", schema(json!({"id": {"type": "string"}, "pinned": {"type": "boolean"}, "archived": {"type": "boolean"}}), &["id"])),
        tool("conversation_search", "Search conversation titles and message contents.", schema(json!({"query": {"type": "string"}}), &["query"])),
        tool("conversation_clear", "Delete all messages from a conversation but keep the thread itself.", schema(json!({"id": {"type": "string"}}), &["id"])),
        tool("conversation_stats", "Message/token statistics: globally, or for one conversation with id.", schema(json!({"id": {"type": "string"}}), &[])),
        // ---- persona/template sharing ----
        tool("persona_duplicate", "Duplicate a persona under a new name (by ID or name).", schema(json!({"id": {"type": "string"}, "name": {"type": "string"}}), &["id"])),
        tool("persona_export", "Export personas as portable JSON (one by id/name, or all).", schema(json!({"id": {"type": "string"}}), &[])),
        tool("persona_import", "Import personas from persona_export JSON (array, or {personas: [...]}). New IDs are assigned.", schema(json!({"personas": {}}), &["personas"])),
        tool("template_duplicate", "Duplicate a prompt template under a new name (by ID or name).", schema(json!({"id": {"type": "string"}, "name": {"type": "string"}}), &["id"])),
        // ---- memory/document/task extras ----
        tool("memory_export", "Export all memory entries as portable JSON.", empty.clone()),
        tool("memory_clear", "Delete memory entries — all of them, or only one tag (tag arg). Requires confirm=true.", schema(json!({"confirm": {"type": "boolean"}, "tag": {"type": "string"}}), &["confirm"])),
        tool("memory_stats", "Memory statistics: entry count, distinct tags, tag counts.", empty.clone()),
        tool("document_update", "Update a document's title/content/tags by ID.", schema(json!({"id": {"type": "string"}, "title": {"type": "string"}, "content": {"type": "string"}, "tags": {"type": "array", "items": {"type": "string"}}}), &["id"])),
        tool("task_update", "Update a task's name/cron/prompt/persona/enabled (by ID).", schema(json!({"id": {"type": "string"}, "name": {"type": "string"}, "cron": {"type": "string"}, "prompt": {"type": "string"}, "persona_id": {"type": "string"}, "enabled": {"type": "boolean"}}), &["id"])),
        tool("task_set_enabled", "Enable or disable a scheduled task without deleting it.", schema(json!({"id": {"type": "string"}, "enabled": {"type": "boolean"}}), &["id", "enabled"])),
        // ---- server & state admin extras ----
        tool("server_info", "This MCP server's info: name, version, transports, protocol versions, tool/resource/prompt counts, backend, uptime seconds.", empty.clone()),
        tool("state_backup", "Snapshot current state: returns it as JSON and, when a state file is configured, also writes a timestamped backup file beside it.", empty.clone()),
        tool("state_validate", "Validate state integrity: duplicate IDs, dangling active persona / conversation personas, counts. Returns problems found (empty = healthy).", empty.clone()),
        tool("request_log_clear", "Clear the request log. Requires confirm=true.", schema(json!({"confirm": {"type": "boolean"}}), &["confirm"])),
        tool("text_summarize", "Summarise arbitrary text with GrokBot (stateless helper; max_words guides length).", schema(json!({"text": {"type": "string"}, "max_words": {"type": "integer"}, "model": {"type": "string"}}), &["text"])),
        tool("text_translate", "Translate text into a target language with GrokBot (stateless helper).", schema(json!({"text": {"type": "string"}, "target_language": {"type": "string"}, "model": {"type": "string"}}), &["text", "target_language"])),
        // ---- bots registry (bridge-inspired multi-bot) ----
        tool("list_bots", "List named bot profiles in the registry, marking the active one. Each bot is a full config profile; activate_bot swaps it in.", empty.clone()),
        tool("create_bot", "Create a named bot profile in the registry (clones current config unless fields override). Does not switch to it unless activate=true.", schema(json!({"name": {"type": "string"}, "description": {"type": "string"}, "system_prompt": {"type": "string"}, "model": {"type": "string"}, "activate": {"type": "boolean"}}), &["name"])),
        tool("update_bot", "Update a bot profile's name/description/config fields (by ID or name).", schema(json!({"id": {"type": "string"}, "name": {"type": "string"}, "description": {"type": "string"}, "system_prompt": {"type": "string"}, "model": {"type": "string"}}), &["id"])),
        tool("delete_bot", "Delete a bot profile from the registry (by ID or name). Requires confirm matching the bot name.", schema(json!({"id": {"type": "string"}, "confirm": {"type": "string"}}), &["id", "confirm"])),
        tool("activate_bot", "Switch the live bot to a registry profile (current config is saved back to the previously active profile).", schema(json!({"id": {"type": "string"}}), &["id"])),
        tool("bot_status", "Bridge-style status: active bot, whether a job is working, unread job results, usage snapshot.", empty.clone()),
        // ---- routines (bridge parity over scheduled tasks) ----
        tool("routines", "Unified routines tool (like grokbot_routines): action list|add|run|enable|disable|remove. add needs name+cron+prompt; run/enable/disable/remove need id.", schema(json!({"action": {"type": "string", "enum": ["list", "add", "run", "enable", "disable", "remove"]}, "id": {"type": "string"}, "name": {"type": "string"}, "cron": {"type": "string"}, "prompt": {"type": "string"}, "persona_id": {"type": "string"}, "webhook_url": {"type": "string"}}), &["action"])),
        // ---- async jobs: ask/wait/stop ----
        tool("chat_async", "Start a chat in the background and return a job_id immediately. Poll with job_status/job_wait; stop with bot_stop.", schema(json!({"prompt": {"type": "string"}, "persona_id": {"type": "string"}, "model": {"type": "string"}}), &["prompt"])),
        tool("job_status", "Get an async job's status/result by job_id.", schema(json!({"id": {"type": "string"}}), &["id"])),
        tool("job_wait", "Wait up to timeout_seconds for a job to finish and return its result.", schema(json!({"id": {"type": "string"}, "timeout_seconds": {"type": "integer"}}), &["id"])),
        tool("job_list", "List recent async jobs with statuses.", empty.clone()),
        tool("bot_stop", "Stop a running async job (by job_id, or the latest running job). Cooperative cancellation for in-flight chats/tasks.", schema(json!({"id": {"type": "string"}}), &[])),
        // ---- streaming, search, structured ----
        tool("chat_stream", "Chat using the backend streaming path; returns the assembled reply plus how many deltas arrived (live: token deltas; mock: one delta).", schema(json!({"prompt": {"type": "string"}, "persona_id": {"type": "string"}, "model": {"type": "string"}, "response_format": {"type": "object"}, "reasoning_effort": {"type": "string"}}), &["prompt"])),
        tool("web_search", "Search the web with Grok (live backends send a web_search tool to xAI; mock searches local documents instead and says so).", schema(json!({"query": {"type": "string"}, "model": {"type": "string"}}), &["query"])),
        tool("x_search", "Search X/Twitter with Grok (live backends send an x_search tool to xAI; mock says no live results).", schema(json!({"query": {"type": "string"}, "model": {"type": "string"}}), &["query"])),
        // ---- media & files ----
        tool("video_generate", "Generate a video with Grok imagine-video models (mock returns a mock:// URL offline).", schema(json!({"prompt": {"type": "string"}, "model": {"type": "string"}}), &["prompt"])),
        tool("file_upload", "Upload a file to xAI Files (content as base64) or register it locally in mock mode. Optionally also store as a document.", schema(json!({"filename": {"type": "string"}, "content_base64": {"type": "string"}, "store_as_document": {"type": "boolean"}}), &["filename", "content_base64"])),
        tool("file_list", "List files on the backend (xAI Files live; mock lists locally registered uploads from documents).", empty.clone()),
        tool("file_delete", "Delete a file by backend file ID.", schema(json!({"id": {"type": "string"}}), &["id"])),
        tool("audio_transcribe", "Transcribe base64 audio (live: xAI audio API; mock returns a labelled mock transcript).", schema(json!({"audio_base64": {"type": "string"}, "format": {"type": "string"}, "model": {"type": "string"}}), &["audio_base64"])),
        tool("audio_speak", "Synthesise speech from text; returns base64 audio (mock returns a short silent WAV placeholder).", schema(json!({"text": {"type": "string"}, "voice": {"type": "string"}, "model": {"type": "string"}}), &["text"])),
        // ---- semantic search & embeddings ----
        tool("embed_text", "Get embedding vectors for one or more texts (mock: deterministic local vectors; live: xAI embeddings).", schema(json!({"texts": {"type": "array", "items": {"type": "string"}}, "model": {"type": "string"}}), &["texts"])),
        tool("semantic_search", "Semantic (embedding cosine) search over documents and memories. Embeddings are computed and cached on first use.", schema(json!({"query": {"type": "string"}, "limit": {"type": "integer"}}), &["query"])),
        // ---- conversation message-level control ----
        tool("conversation_message_edit", "Edit one message in a conversation by message_id (ids are returned by conversation_get).", schema(json!({"id": {"type": "string"}, "message_id": {"type": "string"}, "content": {"type": "string"}}), &["id", "message_id", "content"])),
        tool("conversation_message_delete", "Delete one message from a conversation by message_id.", schema(json!({"id": {"type": "string"}, "message_id": {"type": "string"}}), &["id", "message_id"])),
        // ---- persona versioning & bot tool defs ----
        tool("persona_prompt_history", "List a persona's system-prompt version history (by ID or name).", schema(json!({"id": {"type": "string"}}), &["id"])),
        tool("persona_rollback", "Roll a persona back to a previous prompt version (appends a new version; history is never rewritten).", schema(json!({"id": {"type": "string"}, "version": {"type": "integer"}}), &["id", "version"])),
        tool("persona_ab_test", "A/B test: send one prompt to two personas (by ID or name) and compare replies/tokens/cost.", schema(json!({"prompt": {"type": "string"}, "persona_a": {"type": "string"}, "persona_b": {"type": "string"}}), &["prompt", "persona_a", "persona_b"])),
        tool("bot_tool_define", "Define a function tool GrokBot itself may call (sent to xAI as a function tool on chat). Upserts by name.", schema(json!({"name": {"type": "string"}, "description": {"type": "string"}, "parameters": {"type": "object"}, "enabled": {"type": "boolean"}}), &["name", "parameters"])),
        tool("bot_tool_defs_list", "List operator-defined GrokBot function tools.", empty.clone()),
        tool("bot_tool_delete", "Delete an operator-defined GrokBot function tool by name.", schema(json!({"name": {"type": "string"}}), &["name"])),
        // ---- backup restore & quota ----
        tool("list_backups", "List timestamped state backups available for restore.", empty.clone()),
        tool("state_restore", "Restore state from a backup file name (from list_backups). Auto-backs-up current state first. Requires confirm=true.", schema(json!({"backup": {"type": "string"}, "confirm": {"type": "boolean"}}), &["backup", "confirm"])),
        tool("usage_quota", "Budget/quota status: daily budget cap, rate limit, estimated spend today, per-model spend.", empty.clone()),
    ]
}

fn check_safety(state: &AppState, text: &str) -> Result<()> {
    if state.config.safety.filter_level == "off" {
        return Ok(());
    }
    let lower = text.to_lowercase();
    for topic in &state.config.safety.blocked_topics {
        if !topic.is_empty() && lower.contains(&topic.to_lowercase()) {
            bail!(
                "blocked by GrokBot safety settings: topic '{topic}' is blocked (filter_level={})",
                state.config.safety.filter_level
            );
        }
    }
    Ok(())
}

pub fn ensure_unique_name(
    existing: &[String],
    name: &str,
    exclude_id: Option<&str>,
    all: &[(String, String)],
) -> Result<()> {
    for (id, n) in all {
        if n.eq_ignore_ascii_case(name) && exclude_id != Some(id.as_str()) {
            bail!("a record named '{name}' already exists (id {id}) — names must be unique (case-insensitive); use the ID or pick another name");
        }
    }
    let _ = existing;
    Ok(())
}

fn persona_json(p: &Persona) -> Value {
    json!({"id": p.id, "name": p.name, "description": p.description, "system_prompt": p.system_prompt,
           "model": p.model, "temperature": p.temperature,
           "created_at": p.created_at, "updated_at": p.updated_at})
}
fn conv_summary(c: &Conversation) -> Value {
    json!({"id": c.id, "title": c.title, "persona_id": c.persona_id, "model": c.model,
           "pinned": c.pinned, "archived": c.archived,
           "message_count": c.messages.len(), "created_at": c.created_at, "updated_at": c.updated_at})
}
fn conv_json(c: &Conversation) -> Value {
    json!({"id": c.id, "title": c.title, "persona_id": c.persona_id, "model": c.model,
           "pinned": c.pinned, "archived": c.archived,
           "created_at": c.created_at, "updated_at": c.updated_at,
           "messages": c.messages.iter().map(|m| json!({"id": m.id, "role": m.role, "content": m.content, "ts": m.ts, "edited_at": m.edited_at, "model": m.model, "tokens": m.tokens})).collect::<Vec<_>>()})
}

/// Dispatch one tool call. Returns the JSON payload (mcp.rs wraps it as text content).
pub async fn call_tool(ctx: &Ctx, name: &str, args: &Value) -> Result<Value> {
    let required = crate::auth::required_scope(name);
    if !crate::auth::scope_allows(&ctx.caller_scopes, required) {
        bail!(
            "insufficient scope for tool '{name}': requires '{required}', caller has [{}]",
            ctx.caller_scopes.join(", ")
        );
    }
    match name {
        "bot_get_profile" => {
            let st = ctx.store.lock().await;
            Ok(
                json!({"bot": serde_json::to_value(&st.state.config)?, "backend": ctx.backend.name()}),
            )
        }
        "bot_update_config" => {
            // Validate before mutating anything (finding 8).
            if let Some(m) = s(args, "default_model") {
                let models = ctx.backend.list_models().await?;
                if !models.iter().any(|x| x.id == m) {
                    bail!(
                        "unknown model '{m}' (available: {})",
                        models
                            .iter()
                            .map(|x| x.id.as_str())
                            .collect::<Vec<_>>()
                            .join(", ")
                    );
                }
            }
            if let Some(v) = f(args, "temperature") {
                if !(0.0..=2.0).contains(&v) {
                    bail!("temperature must be within 0..=2, got {v}");
                }
            }
            if let Some(v) = f(args, "top_p") {
                if !(0.0..=1.0).contains(&v) {
                    bail!("top_p must be within 0..=1, got {v}");
                }
            }
            if let Some(v) = u(args, "max_tokens") {
                if v == 0 || v > 1_000_000 {
                    bail!("max_tokens must be within 1..=1000000, got {v}");
                }
            }
            let mut st = ctx.store.lock().await;
            let c = &mut st.state.config;
            if let Some(v) = s(args, "name") {
                c.name = v;
            }
            if let Some(v) = s(args, "description") {
                c.description = v;
            }
            if let Some(v) = s(args, "system_prompt") {
                c.system_prompt = v;
            }
            if let Some(v) = s(args, "default_model") {
                c.default_model = v;
            }
            if let Some(v) = f(args, "temperature") {
                c.temperature = v;
            }
            if let Some(v) = f(args, "top_p") {
                c.top_p = v;
            }
            if let Some(v) = u(args, "max_tokens") {
                c.max_tokens = v as u32;
            }
            if let Some(v) = args.get("daily_budget_usd") {
                c.daily_budget_usd = v.as_f64();
            }
            if let Some(v) = u(args, "max_history_messages") {
                c.max_history_messages = (v as usize).clamp(1, 10_000);
            }
            if let Some(v) = u(args, "rate_limit_per_minute") {
                c.rate_limit_per_minute = v as u32;
            }
            let out = serde_json::to_value(&*c)?;
            st.record(name, ctx.backend.name(), None, true, "config updated");
            st.save()?;
            Ok(json!({"bot": out}))
        }
        "bot_list_models" => {
            let models = ctx.backend.list_models().await?;
            Ok(json!({"backend": ctx.backend.name(), "models": models}))
        }
        "bot_provider_status" => {
            let st = ctx.store.lock().await;
            let (cred_kind, cred_detail) = ctx.backend.credential_info();
            Ok(json!({
                "backend": ctx.backend.name(), "live": ctx.backend.is_live(),
                "credential": {"kind": cred_kind, "detail": cred_detail},
                "xai_api_key_set": !std::env::var("XAI_API_KEY").unwrap_or_default().trim().is_empty(),
                "xai_base_url": std::env::var("XAI_BASE_URL").unwrap_or_else(|_| "https://api.x.ai/v1".into()),
                "state_file": st.path().map(|p| p.display().to_string()),
                "default_model": st.state.config.default_model,
            }))
        }
        "bot_test_connection" => {
            let prompt = s(args, "prompt").unwrap_or_else(|| "ping — reply with pong".into());
            let (system, model, temp, max) = {
                let st = ctx.store.lock().await;
                let c = &st.state.config;
                (
                    c.system_prompt.clone(),
                    c.default_model.clone(),
                    c.temperature,
                    64u32.min(c.max_tokens.max(1)),
                )
            };
            let started = std::time::Instant::now();
            let reply = do_chat(
                ctx,
                name,
                vec![Message::new("system", system), Message::new("user", prompt)],
                model,
                temp,
                max,
            )
            .await?;
            Ok(
                json!({"ok": true, "backend": reply.backend, "model": reply.model,
                      "latency_ms": started.elapsed().as_millis() as u64, "reply": reply.content}),
            )
        }
        "chat" => {
            let prompt = req_s(args, "prompt")?;
            let (system, model, temp, max, persona_name) = {
                let st = ctx.store.lock().await;
                check_safety(&st.state, &prompt)?;
                let persona =
                    s(args, "persona_id").and_then(|id| st.state.find_persona(&id).cloned());
                let (sys, m, t, mx) = resolve(&st.state, args);
                let sys = if s(args, "persona_id").is_none() && s(args, "system").is_none() {
                    active_persona_system(&st.state).unwrap_or(sys)
                } else {
                    sys
                };
                (sys, m, t, mx, persona.map(|p| p.name))
            };
            let reply = do_chat(
                ctx,
                name,
                vec![
                    Message::new("system", system),
                    Message::new("user", prompt.clone()),
                ],
                model,
                temp,
                max,
            )
            .await?;
            Ok(
                json!({"reply": reply.content, "model": reply.model, "backend": reply.backend,
                      "persona": persona_name, "prompt_tokens": reply.prompt_tokens,
                      "completion_tokens": reply.completion_tokens}),
            )
        }
        _ => dispatch_crud(ctx, name, args).await,
    }
}

async fn dispatch_crud(ctx: &Ctx, name: &str, args: &Value) -> Result<Value> {
    match name {
        // ---------- conversations ----------
        "conversation_create" => {
            let mut st = ctx.store.lock().await;
            let persona_id = s(args, "persona_id")
                .and_then(|id| st.state.find_persona(&id).map(|p| p.id.clone()));
            let c = Conversation {
                id: new_id(),
                title: s(args, "title").unwrap_or_else(|| "Untitled conversation".into()),
                persona_id,
                model: s(args, "model").unwrap_or_else(|| st.state.config.default_model.clone()),
                messages: vec![],
                pinned: false,
                archived: false,
                summary: None,
                summary_up_to: 0,
                created_at: now(),
                updated_at: now(),
            };
            let out = conv_json(&c);
            st.state.conversations.push(c);
            st.record(name, ctx.backend.name(), None, true, "conversation created");
            st.save()?;
            Ok(json!({"conversation": out}))
        }
        "conversation_list" => {
            let st = ctx.store.lock().await;
            Ok(
                json!({"conversations": st.state.conversations.iter().map(conv_summary).collect::<Vec<_>>()}),
            )
        }
        "conversation_get" => {
            let id = req_s(args, "id")?;
            let st = ctx.store.lock().await;
            let c = st
                .state
                .find_conversation(&id)
                .ok_or_else(|| anyhow!("conversation not found: {id}"))?;
            Ok(json!({"conversation": conv_json(c)}))
        }
        "conversation_delete" => {
            let id = req_s(args, "id")?;
            let mut st = ctx.store.lock().await;
            let before = st.state.conversations.len();
            st.state.conversations.retain(|c| c.id != id);
            if st.state.conversations.len() == before {
                bail!("conversation not found: {id}");
            }
            st.record(name, ctx.backend.name(), None, true, "conversation deleted");
            st.save()?;
            Ok(json!({"deleted": id}))
        }
        "conversation_add_message" => {
            let id = req_s(args, "id")?;
            let role = req_s(args, "role")?;
            let content = req_s(args, "content")?;
            let mut st = ctx.store.lock().await;
            let c = st
                .state
                .conversations
                .iter_mut()
                .find(|c| c.id == id)
                .ok_or_else(|| anyhow!("conversation not found: {id}"))?;
            c.messages.push(Message::new(&role, content));
            c.updated_at = now();
            let out = conv_json(c);
            st.save()?;
            Ok(json!({"conversation": out}))
        }
        "conversation_send" => {
            let id = req_s(args, "id")?;
            let content = req_s(args, "content")?;
            if content.chars().count() > MAX_MESSAGE_CHARS {
                bail!("message too long (max {MAX_MESSAGE_CHARS} chars)");
            }
            // Per-conversation turn lock: one full turn at a time (finding 7).
            let turn_lock = ctx.conv_lock(&id).await;
            let _turn = turn_lock.lock().await;
            let (history, model, temp, max, resolved) = {
                let st = ctx.store.lock().await;
                let c = st
                    .state
                    .find_conversation(&id)
                    .ok_or_else(|| anyhow!("conversation not found: {id}"))?;
                let conv_persona = c.persona_id.clone();
                let conv_model = c.model.clone();
                let r = resolve_for(&st.state, args, conv_persona.as_deref(), Some(&conv_model));
                // Windowed history: optional stored summary + last N (M5).
                let cap = st.state.config.max_history_messages.max(1);
                let start = c.messages.len().saturating_sub(cap);
                let mut msgs = vec![Message::new("system", r.system.clone())];
                if let Some(sum) = &c.summary {
                    msgs.push(Message::new(
                        "system",
                        format!("Summary of earlier messages: {sum}"),
                    ));
                }
                msgs.extend(c.messages[start..].iter().cloned());
                msgs.push(Message::new("user", content.clone()));
                (msgs, r.model.clone(), r.temperature, r.max_tokens, r)
            };
            let _ = resolved;
            // Backend failure leaves the conversation untouched (atomic turn).
            let reply = do_chat(ctx, name, history, model, temp, max).await?;
            let mut st = ctx.store.lock().await;
            let c = st
                .state
                .conversations
                .iter_mut()
                .find(|c| c.id == id)
                .ok_or_else(|| anyhow!("conversation not found: {id}"))?;
            c.messages.push(Message::new("user", content.clone()));
            let mut m = Message::new("assistant", reply.content.clone());
            m.model = Some(reply.model.clone());
            m.tokens = Some(reply.completion_tokens);
            c.messages.push(m);
            c.updated_at = now();
            let out = conv_json(c);
            st.save()?;
            Ok(
                json!({"reply": reply.content, "model": reply.model, "backend": reply.backend,
                      "prompt_tokens": reply.prompt_tokens, "completion_tokens": reply.completion_tokens,
                      "conversation": out}),
            )
        }
        "conversation_fork" => {
            let id = req_s(args, "id")?;
            let mut st = ctx.store.lock().await;
            let src = st
                .state
                .find_conversation(&id)
                .cloned()
                .ok_or_else(|| anyhow!("conversation not found: {id}"))?;
            let up_to = u(args, "up_to")
                .map(|v| v as usize)
                .unwrap_or(src.messages.len());
            let mut c = src.clone();
            c.id = new_id();
            c.archived = false;
            c.pinned = false;
            c.title = s(args, "title").unwrap_or_else(|| format!("{} (fork)", src.title));
            c.messages.truncate(up_to);
            c.created_at = now();
            c.updated_at = now();
            let out = conv_json(&c);
            st.state.conversations.push(c);
            st.save()?;
            Ok(json!({"conversation": out}))
        }
        "conversation_summarize" => {
            let id = req_s(args, "id")?;
            let (transcript, model) = {
                let st = ctx.store.lock().await;
                let c = st
                    .state
                    .find_conversation(&id)
                    .ok_or_else(|| anyhow!("conversation not found: {id}"))?;
                let t = c
                    .messages
                    .iter()
                    .map(|m| format!("{}: {}", m.role, m.content))
                    .collect::<Vec<_>>()
                    .join("\n");
                (t, c.model.clone())
            };
            if transcript.is_empty() {
                bail!("conversation has no messages to summarise");
            }
            let reply = do_chat(ctx, name, vec![
                Message::new("system", "Summarise conversations faithfully in a short paragraph plus bullet key points."),
                Message::new("user", format!("Summarise this conversation:\n\n{transcript}")),
            ], model, 0.3, 1024).await?;
            if b(args, "store", true) {
                let mut st = ctx.store.lock().await;
                if let Some(c) = st.state.conversations.iter_mut().find(|c| c.id == id) {
                    c.messages.push(Message::new(
                        "assistant",
                        format!("[summary] {}", reply.content),
                    ));
                    c.updated_at = now();
                }
                st.save()?;
            }
            Ok(json!({"summary": reply.content, "model": reply.model}))
        }
        "conversation_export" => {
            let id = req_s(args, "id")?;
            let format = s(args, "format").unwrap_or_else(|| "markdown".into());
            let st = ctx.store.lock().await;
            let c = st
                .state
                .find_conversation(&id)
                .ok_or_else(|| anyhow!("conversation not found: {id}"))?;
            if format == "json" {
                Ok(json!({"format": "json", "conversation": conv_json(c)}))
            } else {
                let mut md = format!("# {}\n\n- id: {}\n- model: {}\n\n", c.title, c.id, c.model);
                for m in &c.messages {
                    md.push_str(&format!("**{}**: {}\n\n", m.role, m.content));
                }
                Ok(json!({"format": "markdown", "markdown": md}))
            }
        }
        _ => dispatch_rest(ctx, name, args).await,
    }
}

async fn dispatch_rest(ctx: &Ctx, name: &str, args: &Value) -> Result<Value> {
    match name {
        // ---------- personas ----------
        "persona_list" => {
            let st = ctx.store.lock().await;
            Ok(
                json!({"personas": st.state.personas.iter().map(persona_json).collect::<Vec<_>>(),
                      "active_persona_id": st.state.config.active_persona_id}),
            )
        }
        "persona_create" => {
            let new_name = req_s(args, "name")?;
            let mut st = ctx.store.lock().await;
            if st
                .state
                .personas
                .iter()
                .any(|x| x.name.eq_ignore_ascii_case(&new_name))
            {
                bail!("a persona named '{new_name}' already exists — names must be unique (case-insensitive)");
            }
            let p = Persona {
                id: new_id(),
                name: req_s(args, "name")?,
                description: s(args, "description").unwrap_or_default(),
                system_prompt: req_s(args, "system_prompt")?,
                model: s(args, "model"),
                temperature: f(args, "temperature"),
                prompt_versions: vec![],
                created_at: now(),
                updated_at: now(),
            };
            let out = persona_json(&p);
            st.state.personas.push(p);
            st.record(name, ctx.backend.name(), None, true, "persona created");
            st.save()?;
            Ok(json!({"persona": out}))
        }
        "persona_update" => {
            let id = req_s(args, "id")?;
            let mut st = ctx.store.lock().await;
            let p = st
                .state
                .personas
                .iter_mut()
                .find(|p| p.id == id || p.name.eq_ignore_ascii_case(&id))
                .ok_or_else(|| anyhow!("persona not found: {id}"))?;
            if let Some(v) = s(args, "name") {
                p.name = v;
            }
            if let Some(v) = s(args, "description") {
                p.description = v;
            }
            if let Some(v) = s(args, "system_prompt") {
                if v != p.system_prompt {
                    let next = p
                        .prompt_versions
                        .iter()
                        .map(|x| x.version)
                        .max()
                        .unwrap_or(0)
                        + 1;
                    p.prompt_versions.push(PromptVersion {
                        version: next,
                        system_prompt: v.clone(),
                        note: "update".into(),
                        ts: now(),
                    });
                }
                p.system_prompt = v;
            }
            if let Some(v) = s(args, "model") {
                p.model = Some(v);
            }
            if let Some(v) = f(args, "temperature") {
                p.temperature = Some(v);
            }
            p.updated_at = now();
            let out = persona_json(p);
            st.save()?;
            Ok(json!({"persona": out}))
        }
        "persona_delete" => {
            let id = req_s(args, "id")?;
            let mut st = ctx.store.lock().await;
            let target = st
                .state
                .find_persona(&id)
                .map(|p| p.id.clone())
                .ok_or_else(|| anyhow!("persona not found: {id}"))?;
            st.state.personas.retain(|p| p.id != target);
            if st.state.config.active_persona_id.as_deref() == Some(target.as_str()) {
                st.state.config.active_persona_id = None;
            }
            st.save()?;
            Ok(json!({"deleted": target}))
        }
        "persona_activate" => {
            let id = req_s(args, "id")?;
            let mut st = ctx.store.lock().await;
            let p = st
                .state
                .find_persona(&id)
                .cloned()
                .ok_or_else(|| anyhow!("persona not found: {id}"))?;
            st.state.config.active_persona_id = Some(p.id.clone());
            st.save()?;
            Ok(json!({"active_persona": persona_json(&p)}))
        }
        // ---------- templates ----------
        "template_list" => {
            let st = ctx.store.lock().await;
            Ok(
                json!({"templates": st.state.templates.iter().map(|t| json!({
                "id": t.id, "name": t.name, "description": t.description, "template": t.template,
                "variables": t.variables() })).collect::<Vec<_>>()}),
            )
        }
        "template_create" => {
            let new_name = req_s(args, "name")?;
            let mut st = ctx.store.lock().await;
            if st
                .state
                .templates
                .iter()
                .any(|x| x.name.eq_ignore_ascii_case(&new_name))
            {
                bail!("a template named '{new_name}' already exists — names must be unique (case-insensitive)");
            }
            let t = PromptTemplate {
                id: new_id(),
                name: req_s(args, "name")?,
                description: s(args, "description").unwrap_or_default(),
                template: req_s(args, "template")?,
                created_at: now(),
                updated_at: now(),
            };
            let out = json!({"id": t.id, "name": t.name, "variables": t.variables()});
            st.state.templates.push(t);
            st.save()?;
            Ok(json!({"template": out}))
        }
        "template_update" => {
            let id = req_s(args, "id")?;
            let mut st = ctx.store.lock().await;
            let t = st
                .state
                .templates
                .iter_mut()
                .find(|t| t.id == id || t.name.eq_ignore_ascii_case(&id))
                .ok_or_else(|| anyhow!("template not found: {id}"))?;
            if let Some(v) = s(args, "name") {
                t.name = v;
            }
            if let Some(v) = s(args, "description") {
                t.description = v;
            }
            if let Some(v) = s(args, "template") {
                t.template = v;
            }
            t.updated_at = now();
            let out = json!({"id": t.id, "name": t.name, "template": t.template, "variables": t.variables()});
            st.save()?;
            Ok(json!({"template": out}))
        }
        "template_delete" => {
            let id = req_s(args, "id")?;
            let mut st = ctx.store.lock().await;
            let target = st
                .state
                .find_template(&id)
                .map(|t| t.id.clone())
                .ok_or_else(|| anyhow!("template not found: {id}"))?;
            st.state.templates.retain(|t| t.id != target);
            st.save()?;
            Ok(json!({"deleted": target}))
        }
        "template_render" => {
            let id = req_s(args, "id")?;
            let (rendered, template_name) = {
                let st = ctx.store.lock().await;
                let t = st
                    .state
                    .find_template(&id)
                    .ok_or_else(|| anyhow!("template not found: {id}"))?;
                let mut vars = BTreeMap::new();
                if let Some(obj) = args.get("variables").and_then(|v| v.as_object()) {
                    for (k, v) in obj {
                        vars.insert(
                            k.clone(),
                            v.as_str()
                                .map(|s| s.to_string())
                                .unwrap_or_else(|| v.to_string()),
                        );
                    }
                }
                (t.render(&vars), t.name.clone())
            };
            if b(args, "send", false) {
                let (system, model, temp, max) = {
                    let st = ctx.store.lock().await;
                    resolve(&st.state, args)
                };
                let reply = do_chat(
                    ctx,
                    name,
                    vec![
                        Message::new("system", system),
                        Message::new("user", rendered.clone()),
                    ],
                    model,
                    temp,
                    max,
                )
                .await?;
                Ok(
                    json!({"template": template_name, "rendered": rendered, "reply": reply.content, "model": reply.model}),
                )
            } else {
                Ok(json!({"template": template_name, "rendered": rendered}))
            }
        }
        _ => dispatch_admin(ctx, name, args).await,
    }
}

async fn dispatch_admin(ctx: &Ctx, name: &str, args: &Value) -> Result<Value> {
    match name {
        // ---------- memory ----------
        "memory_store" => {
            let key = req_s(args, "key")?;
            let value = req_s(args, "value")?;
            if value.chars().count() > MAX_MEMORY_VALUE_CHARS {
                bail!("memory value too long (max {MAX_MEMORY_VALUE_CHARS} chars)");
            }
            {
                let st0 = ctx.store.lock().await;
                if st0.state.memories.len() >= MAX_ITEMS {
                    bail!("too many memory entries (max {MAX_ITEMS})");
                }
            }
            let mut st = ctx.store.lock().await;
            if let Some(e) = st.state.memories.iter_mut().find(|e| e.key == key) {
                e.value = value;
                if args.get("tags").is_some() {
                    e.tags = str_list(args, "tags");
                }
                e.updated_at = now();
                let out = serde_json::to_value(&*e)?;
                st.save()?;
                return Ok(json!({"memory": out, "upserted": true}));
            }
            let e = MemoryEntry {
                id: new_id(),
                key,
                value,
                tags: str_list(args, "tags"),
                embedding: None,
                created_at: now(),
                updated_at: now(),
            };
            let out = serde_json::to_value(&e)?;
            st.state.memories.push(e);
            st.save()?;
            Ok(json!({"memory": out, "upserted": false}))
        }
        "memory_recall" => {
            let st = ctx.store.lock().await;
            if let Some(key) = s(args, "key") {
                let hits: Vec<Value> = st
                    .state
                    .memories
                    .iter()
                    .filter(|e| e.key == key)
                    .map(|e| serde_json::to_value(e).unwrap())
                    .collect();
                return Ok(json!({"memories": hits}));
            }
            let q = s(args, "query").unwrap_or_default().to_lowercase();
            let hits: Vec<Value> = st
                .state
                .memories
                .iter()
                .filter(|e| {
                    q.is_empty()
                        || e.key.to_lowercase().contains(&q)
                        || e.value.to_lowercase().contains(&q)
                        || e.tags.iter().any(|t| t.to_lowercase().contains(&q))
                })
                .map(|e| serde_json::to_value(e).unwrap())
                .collect();
            Ok(json!({"memories": hits}))
        }
        "memory_list" => {
            let st = ctx.store.lock().await;
            let tag = s(args, "tag");
            let hits: Vec<Value> = st
                .state
                .memories
                .iter()
                .filter(|e| tag.as_ref().map(|t| e.tags.contains(t)).unwrap_or(true))
                .map(|e| serde_json::to_value(e).unwrap())
                .collect();
            Ok(json!({"memories": hits}))
        }
        "memory_delete" => {
            let id = req_s(args, "id")?;
            let mut st = ctx.store.lock().await;
            let before = st.state.memories.len();
            st.state.memories.retain(|e| e.id != id && e.key != id);
            if st.state.memories.len() == before {
                bail!("memory not found: {id}");
            }
            st.save()?;
            Ok(json!({"deleted": id}))
        }
        // ---------- documents ----------
        "document_add" => {
            if req_s(args, "content")?.chars().count() > MAX_DOCUMENT_CHARS {
                bail!("document too large (max {MAX_DOCUMENT_CHARS} chars)");
            }
            let mut st = ctx.store.lock().await;
            if st.state.documents.len() >= MAX_ITEMS {
                bail!("too many documents (max {MAX_ITEMS})");
            }
            let d = Document {
                id: new_id(),
                title: req_s(args, "title")?,
                content: req_s(args, "content")?,
                tags: str_list(args, "tags"),
                xai_file_id: None,
                collection_id: None,
                embedding: None,
                created_at: now(),
                updated_at: now(),
            };
            let out = serde_json::to_value(&d)?;
            st.state.documents.push(d);
            st.save()?;
            Ok(json!({"document": out}))
        }
        "document_list" => {
            let st = ctx.store.lock().await;
            Ok(
                json!({"documents": st.state.documents.iter().map(|d| json!({
                "id": d.id, "title": d.title, "tags": d.tags, "chars": d.content.chars().count(),
                "updated_at": d.updated_at })).collect::<Vec<_>>()}),
            )
        }
        "document_get" => {
            let id = req_s(args, "id")?;
            let st = ctx.store.lock().await;
            let d = st
                .state
                .documents
                .iter()
                .find(|d| d.id == id)
                .ok_or_else(|| anyhow!("document not found: {id}"))?;
            Ok(json!({"document": serde_json::to_value(d)?}))
        }
        "document_search" => {
            let raw_q = req_s(args, "query")?;
            let terms: Vec<String> = raw_q
                .to_lowercase()
                .split_whitespace()
                .map(|s| s.to_string())
                .collect();
            let st = ctx.store.lock().await;
            let mut hits: Vec<Value> = st
                .state
                .documents
                .iter()
                .filter_map(|d| {
                    let hay = format!("{} {} {}", d.title, d.content, d.tags.join(" ")).to_lowercase();
                    let score = terms.iter().filter(|term| hay.contains(term.as_str())).count();
                    if score == 0 {
                        return None;
                    }
                    let lower = d.content.to_lowercase();
                    let pos = terms.iter().filter_map(|term| lower.find(term.as_str())).min().unwrap_or(0);
                    let start = pos.saturating_sub(80);
                    let snippet: String = d.content.chars().skip(start).take(240).collect();
                    Some(json!({"id": d.id, "title": d.title, "tags": d.tags, "snippet": snippet, "matched_terms": score}))
                })
                .collect();
            hits.sort_by(|a, b| {
                b["matched_terms"]
                    .as_u64()
                    .cmp(&a["matched_terms"].as_u64())
            });
            Ok(json!({"documents": hits, "query": raw_q}))
        }
        "document_delete" => {
            let id = req_s(args, "id")?;
            let mut st = ctx.store.lock().await;
            let before = st.state.documents.len();
            st.state.documents.retain(|d| d.id != id);
            if st.state.documents.len() == before {
                bail!("document not found: {id}");
            }
            st.save()?;
            Ok(json!({"deleted": id}))
        }
        // ---------- tasks ----------
        "task_create" => {
            let mut st = ctx.store.lock().await;
            let t = ScheduledTask {
                id: new_id(),
                name: req_s(args, "name")?,
                cron: {
                    let c = req_s(args, "cron")?;
                    validate_cron(&c)?;
                    c
                },
                prompt: req_s(args, "prompt")?,
                persona_id: s(args, "persona_id"),
                enabled: true,
                run_count: 0,
                last_run_at: None,
                last_result: None,
                next_run_at: None,
                webhook_url: s(args, "webhook_url"),
                created_at: now(),
            };
            let out = serde_json::to_value(&t)?;
            st.state.tasks.push(t);
            st.save()?;
            Ok(json!({"task": out}))
        }
        "task_list" => {
            let st = ctx.store.lock().await;
            Ok(
                json!({"tasks": st.state.tasks.iter().map(|t| serde_json::to_value(t).unwrap()).collect::<Vec<_>>()}),
            )
        }
        "task_delete" => {
            let id = req_s(args, "id")?;
            let mut st = ctx.store.lock().await;
            let before = st.state.tasks.len();
            st.state.tasks.retain(|t| t.id != id);
            if st.state.tasks.len() == before {
                bail!("task not found: {id}");
            }
            st.save()?;
            Ok(json!({"deleted": id}))
        }
        "task_run" => {
            let id = req_s(args, "id")?;
            let (prompt, persona_id) = {
                let st = ctx.store.lock().await;
                let t = st
                    .state
                    .tasks
                    .iter()
                    .find(|t| t.id == id)
                    .ok_or_else(|| anyhow!("task not found: {id}"))?;
                (t.prompt.clone(), t.persona_id.clone())
            };
            let (system, model, temp, max) = {
                let st = ctx.store.lock().await;
                let mut a = Map::new();
                if let Some(p) = &persona_id {
                    a.insert("persona_id".into(), Value::String(p.clone()));
                }
                resolve(&st.state, &Value::Object(a))
            };
            let reply = do_chat(
                ctx,
                name,
                vec![Message::new("system", system), Message::new("user", prompt)],
                model,
                temp,
                max,
            )
            .await?;
            let mut st = ctx.store.lock().await;
            if let Some(t) = st.state.tasks.iter_mut().find(|t| t.id == id) {
                t.run_count += 1;
                t.last_run_at = Some(now());
                t.last_result = Some(reply.content.clone());
            }
            st.save()?;
            Ok(json!({"result": reply.content, "model": reply.model}))
        }
        _ => dispatch_final(ctx, name, args).await,
    }
}

async fn dispatch_final(ctx: &Ctx, name: &str, args: &Value) -> Result<Value> {
    match name {
        "bot_tools_list" => {
            let st = ctx.store.lock().await;
            Ok(json!({"tools": ALL_BOT_TOOLS.iter().map(|t| json!({
                "tool": t, "enabled": st.state.config.enabled_tools.iter().any(|e| e == t) })).collect::<Vec<_>>()}))
        }
        "bot_tool_set_enabled" => {
            let tool_name = req_s(args, "tool")?;
            if !ALL_BOT_TOOLS.contains(&tool_name.as_str()) {
                bail!(
                    "unknown bot tool '{tool_name}' (known: {})",
                    ALL_BOT_TOOLS.join(", ")
                );
            }
            let enabled = b(args, "enabled", true);
            let mut st = ctx.store.lock().await;
            if enabled {
                if !st.state.config.enabled_tools.contains(&tool_name) {
                    st.state.config.enabled_tools.push(tool_name.clone());
                }
            } else {
                st.state.config.enabled_tools.retain(|t| t != &tool_name);
            }
            st.save()?;
            Ok(
                json!({"tool": tool_name, "enabled": enabled, "enabled_tools": st.state.config.enabled_tools}),
            )
        }
        "bot_safety_get" => {
            let st = ctx.store.lock().await;
            Ok(json!({"safety": serde_json::to_value(&st.state.config.safety)?}))
        }
        "bot_safety_update" => {
            let mut st = ctx.store.lock().await;
            if let Some(v) = s(args, "filter_level") {
                if !["off", "standard", "strict"].contains(&v.as_str()) {
                    bail!("filter_level must be off|standard|strict");
                }
                st.state.config.safety.filter_level = v;
            }
            if args.get("blocked_topics").is_some() {
                st.state.config.safety.blocked_topics = str_list(args, "blocked_topics");
            }
            if let Some(v) = args.get("max_response_chars") {
                st.state.config.safety.max_response_chars = v.as_u64().map(|n| n as usize);
            }
            let out = serde_json::to_value(&st.state.config.safety)?;
            st.save()?;
            Ok(json!({"safety": out}))
        }
        "usage_stats" => {
            let st = ctx.store.lock().await;
            let usage = &st.state.usage;
            Ok(json!({"total_requests": usage.total_requests,
                      "total_prompt_tokens": usage.total_prompt_tokens,
                      "total_completion_tokens": usage.total_completion_tokens,
                      "total_tokens": usage.total_tokens(),
                      "by_model": usage.by_model, "by_tool": usage.by_tool, "backend": ctx.backend.name()}))
        }
        "request_log" => {
            let limit = u(args, "limit").unwrap_or(50) as usize;
            let st = ctx.store.lock().await;
            let entries: Vec<Value> = st
                .state
                .log
                .iter()
                .rev()
                .take(limit)
                .rev()
                .map(|e| serde_json::to_value(e).unwrap())
                .collect();
            Ok(json!({"log": entries}))
        }
        "diagnostics" => {
            let st = ctx.store.lock().await;
            let a = &st.state;
            Ok(json!({
                "server": crate::SERVER_NAME, "version": crate::VERSION,
                "backend": ctx.backend.name(), "live": ctx.backend.is_live(),
                "xai_api_key_set": !std::env::var("XAI_API_KEY").unwrap_or_default().trim().is_empty(),
                "state_file": st.path().map(|p| p.display().to_string()),
                "bot_name": a.config.name, "default_model": a.config.default_model,
                "active_persona_id": a.config.active_persona_id,
                "counts": {"personas": a.personas.len(), "conversations": a.conversations.len(),
                           "templates": a.templates.len(), "memories": a.memories.len(),
                           "documents": a.documents.len(), "tasks": a.tasks.len(),
                           "log_entries": a.log.len()},
                "usage": {"total_requests": a.usage.total_requests, "total_tokens": a.usage.total_tokens()},
                "tools_available": tool_definitions().len(),
            }))
        }
        "state_export" => {
            let st = ctx.store.lock().await;
            Ok(json!({"state": serde_json::to_value(&st.state)?}))
        }
        "state_import" => {
            // Auto-backup first so a bad import is always reversible.
            let backup = call_tool_boxed(ctx, "state_backup", &json!({})).await.ok();
            let raw = args
                .get("state")
                .cloned()
                .ok_or_else(|| anyhow!("missing 'state' object"))?;
            let mut new_state: AppState = serde_json::from_value(raw)?;
            crate::state::backfill_message_ids(&mut new_state);
            // Validate before replacing.
            {
                let mut problems: Vec<String> = vec![];
                if let Some(pid) = &new_state.config.active_persona_id {
                    if new_state.find_persona(pid).is_none() {
                        problems.push(format!("active_persona_id {pid} does not exist"));
                    }
                }
                let mut seen = std::collections::HashSet::new();
                for per in &new_state.personas {
                    if !seen.insert(per.id.clone()) {
                        problems.push(format!("duplicate persona id {}", per.id));
                    }
                }
                if !problems.is_empty() {
                    bail!("state_import rejected (current state unchanged; pre-import backup taken): {}", problems.join("; "));
                }
            }
            let mut st = ctx.store.lock().await;
            st.state = new_state;
            st.record(name, ctx.backend.name(), None, true, "state imported");
            st.save()?;
            Ok(
                json!({"imported": true, "pre_import_backup": backup.and_then(|b| b.get("backup_file").cloned())}),
            )
        }
        "state_reset" => {
            if !b(args, "confirm", false) {
                bail!("state_reset is destructive: pass confirm=true to proceed");
            }
            let mut st = ctx.store.lock().await;
            st.state = AppState::default();
            st.save()?;
            Ok(json!({"reset": true}))
        }
        _ => dispatch_extra(ctx, name, args).await,
    }
}

async fn dispatch_extra(ctx: &Ctx, name: &str, args: &Value) -> Result<Value> {
    match name {
        // ---------- models / cost / tokens ----------
        "bot_get_model" => {
            let model = req_s(args, "model")?;
            let models = ctx.backend.list_models().await?;
            let found = models.iter().find(|m| m.id == model);
            let (p, c) = pricing(&model);
            Ok(
                json!({"model": model, "found": found.is_some(), "info": found,
                      "pricing_usd_per_1m_tokens": {"prompt": p, "completion": c},
                      "backend": ctx.backend.name()}),
            )
        }
        "bot_set_default_model" => {
            let model = req_s(args, "model")?;
            let models = ctx.backend.list_models().await?;
            if !models.iter().any(|m| m.id == model) {
                bail!(
                    "unknown model '{model}' (available: {})",
                    models
                        .iter()
                        .map(|m| m.id.as_str())
                        .collect::<Vec<_>>()
                        .join(", ")
                );
            }
            let mut st = ctx.store.lock().await;
            st.state.config.default_model = model.clone();
            st.save()?;
            Ok(json!({"default_model": model}))
        }
        "token_count" => {
            if let Some(arr) = args.get("messages").and_then(|v| v.as_array()) {
                let mut total = 0u64;
                let per: Vec<Value> = arr
                    .iter()
                    .map(|m| {
                        let content = m.get("content").and_then(|v| v.as_str()).unwrap_or("");
                        let t = est_tokens(content);
                        total += t;
                        json!({"role": m.get("role"), "tokens": t})
                    })
                    .collect();
                return Ok(
                    json!({"total_tokens": total, "messages": per, "estimator": "chars/4 ceil"}),
                );
            }
            let text = s(args, "text").unwrap_or_default();
            Ok(
                json!({"tokens": est_tokens(&text), "chars": text.chars().count(), "estimator": "chars/4 ceil"}),
            )
        }
        "usage_cost" => {
            let st = ctx.store.lock().await;
            let usage = &st.state.usage;
            let mut per_model: Vec<Value> = vec![];
            let mut total = 0.0;
            if !usage.by_model_tokens.is_empty() {
                for (m, tk) in &usage.by_model_tokens {
                    let c = est_cost_usd(m, tk.prompt_tokens, tk.completion_tokens)
                        + crate::backend::image_cost_usd(m, tk.images);
                    total += c;
                    per_model.push(json!({"model": m, "requests": tk.requests, "prompt_tokens": tk.prompt_tokens,
                                          "completion_tokens": tk.completion_tokens, "images": tk.images,
                                          "estimated_usd": c,
                                          "pricing_known": crate::backend::pricing_for(m) != crate::backend::Pricing::Unknown}));
                }
            } else {
                // Legacy usage (requests only): price totals as default, labelled.
                let default = st.state.config.default_model.clone();
                total = est_cost_usd(
                    &default,
                    usage.total_prompt_tokens,
                    usage.total_completion_tokens,
                );
                per_model.push(json!({"model": default, "legacy_pricing_as_default": true,
                                      "estimated_usd": total}));
            }
            Ok(json!({"estimated_total_usd": total, "per_model": per_model,
                      "total_prompt_tokens": usage.total_prompt_tokens,
                      "total_completion_tokens": usage.total_completion_tokens,
                      "total_images": usage.total_images,
                      "pricing_table_note": "approximate per-model rates (bot_get_model); unknown models price as $0 and are flagged; estimates only"}))
        }
        "usage_reset" => {
            if !b(args, "confirm", false) {
                bail!("usage_reset requires confirm=true");
            }
            let mut st = ctx.store.lock().await;
            st.state.usage = UsageStats::default();
            st.save()?;
            Ok(json!({"reset": true}))
        }
        // ---------- media ----------
        "image_generate" => {
            let prompt = req_s(args, "prompt")?;
            let model = s(args, "model").unwrap_or_else(|| "grok-2-image-1212".into());
            let n = (u(args, "n").unwrap_or(1) as u32).clamp(1, 10);
            let images = ctx.backend.image_generate(&prompt, &model, n).await?;
            let mut st = ctx.store.lock().await;
            st.record_image(&model, n as u64);
            st.record(
                name,
                ctx.backend.name(),
                Some(model),
                true,
                format!("{n} image(s)"),
            );
            st.save()?;
            Ok(json!({"images": images}))
        }
        "image_understand" => {
            let prompt = req_s(args, "prompt")?;
            let image_url = req_s(args, "image_url")?;
            {
                let parsed = reqwest::Url::parse(&image_url)
                    .map_err(|_| anyhow!("image_url is not a valid URL"))?;
                let host = parsed.host_str().unwrap_or_default();
                let local = matches!(host, "127.0.0.1" | "localhost" | "::1");
                if !(parsed.scheme() == "https" || (parsed.scheme() == "http" && local)) {
                    bail!("image_url must be https (http only for localhost) — refusing non-public target '{host}'");
                }
                if host == "169.254.169.254" {
                    bail!("image_url points at a link-local metadata address — refused");
                }
            }
            let (model, max) = {
                let st = ctx.store.lock().await;
                (
                    s(args, "model").unwrap_or_else(|| "grok-4".into()),
                    u(args, "max_tokens")
                        .map(|v| v as u32)
                        .unwrap_or(st.state.config.max_tokens),
                )
            };
            let reply = ctx
                .backend
                .vision_chat(&prompt, &image_url, &model, max)
                .await?;
            let mut st = ctx.store.lock().await;
            st.record_usage(&reply.model, reply.prompt_tokens, reply.completion_tokens);
            st.record(
                name,
                ctx.backend.name(),
                Some(reply.model.clone()),
                true,
                "vision",
            );
            st.save()?;
            Ok(json!({"reply": reply.content, "model": reply.model, "backend": reply.backend}))
        }
        // ---------- batch / compare / text helpers ----------
        "chat_batch" => {
            let prompts = str_list(args, "prompts");
            if prompts.is_empty() {
                bail!("prompts must be a non-empty array of strings");
            }
            if prompts.len() > 20 {
                bail!("max 20 prompts per chat_batch");
            }
            let (system, model, temp, max) = {
                let st = ctx.store.lock().await;
                resolve(&st.state, args)
            };
            let mut results = vec![];
            for prompt in &prompts {
                let reply_result = do_chat(
                    ctx,
                    name,
                    vec![
                        Message::new("system", system.clone()),
                        Message::new("user", prompt.clone()),
                    ],
                    model.clone(),
                    temp,
                    max,
                )
                .await;
                match reply_result {
                    Ok(reply) => results.push(json!({"prompt": prompt, "ok": true, "reply": reply.content, "model": reply.model,
                                    "prompt_tokens": reply.prompt_tokens, "completion_tokens": reply.completion_tokens})),
                    Err(e) => results.push(json!({"prompt": prompt, "ok": false, "error": format!("{e:#}")})),
                }
            }
            Ok(json!({"results": results, "count": results.len()}))
        }
        "model_compare" => {
            let prompt = req_s(args, "prompt")?;
            let models = str_list(args, "models");
            if models.is_empty() {
                bail!("models must be a non-empty array");
            }
            if models.len() > 8 {
                bail!("max 8 models per model_compare");
            }
            let (system, temp, max) = {
                let st = ctx.store.lock().await;
                let c = &st.state.config;
                (c.system_prompt.clone(), c.temperature, c.max_tokens)
            };
            let mut results = vec![];
            for model in &models {
                let reply_result = do_chat(
                    ctx,
                    name,
                    vec![
                        Message::new("system", system.clone()),
                        Message::new("user", prompt.clone()),
                    ],
                    model.clone(),
                    temp,
                    max,
                )
                .await;
                match reply_result {
                    Ok(reply) => results.push(json!({"model": reply.model, "ok": true, "reply": reply.content,
                                    "prompt_tokens": reply.prompt_tokens, "completion_tokens": reply.completion_tokens,
                                    "estimated_cost_usd": est_cost_usd(&reply.model, reply.prompt_tokens, reply.completion_tokens)})),
                    Err(e) => results.push(json!({"model": model, "ok": false, "error": format!("{e:#}")})),
                }
            }
            Ok(json!({"prompt": prompt, "results": results}))
        }
        "text_summarize" => {
            let text = req_s(args, "text")?;
            let max_words = u(args, "max_words").unwrap_or(120);
            let (model, max) = {
                let st = ctx.store.lock().await;
                (
                    s(args, "model").unwrap_or_else(|| st.state.config.default_model.clone()),
                    st.state.config.max_tokens,
                )
            };
            let reply = do_chat(
                ctx,
                name,
                vec![
                    Message::new("system", "Summarise faithfully. No preamble."),
                    Message::new(
                        "user",
                        format!("Summarise in at most {max_words} words:\n\n{text}"),
                    ),
                ],
                model,
                0.3,
                max,
            )
            .await?;
            Ok(json!({"summary": reply.content, "model": reply.model}))
        }
        "text_translate" => {
            let text = req_s(args, "text")?;
            let target = req_s(args, "target_language")?;
            let (model, max) = {
                let st = ctx.store.lock().await;
                (
                    s(args, "model").unwrap_or_else(|| st.state.config.default_model.clone()),
                    st.state.config.max_tokens,
                )
            };
            let reply = do_chat(
                ctx,
                name,
                vec![
                    Message::new(
                        "system",
                        "Translate faithfully. Output only the translation.",
                    ),
                    Message::new("user", format!("Translate into {target}:\n\n{text}")),
                ],
                model,
                0.3,
                max,
            )
            .await?;
            Ok(
                json!({"translation": reply.content, "target_language": target, "model": reply.model}),
            )
        }
        _ => dispatch_extra2(ctx, name, args).await,
    }
}

async fn dispatch_extra2(ctx: &Ctx, name: &str, args: &Value) -> Result<Value> {
    match name {
        // ---------- conversation extras ----------
        "conversation_rename" => {
            let id = req_s(args, "id")?;
            let title = req_s(args, "title")?;
            let mut st = ctx.store.lock().await;
            let c = st
                .state
                .conversations
                .iter_mut()
                .find(|c| c.id == id)
                .ok_or_else(|| anyhow!("conversation not found: {id}"))?;
            c.title = title;
            c.updated_at = now();
            let out = conv_summary(c);
            st.save()?;
            Ok(json!({"conversation": out}))
        }
        "conversation_set_flags" => {
            let id = req_s(args, "id")?;
            let mut st = ctx.store.lock().await;
            let c = st
                .state
                .conversations
                .iter_mut()
                .find(|c| c.id == id)
                .ok_or_else(|| anyhow!("conversation not found: {id}"))?;
            if let Some(v) = args.get("pinned").and_then(|v| v.as_bool()) {
                c.pinned = v;
            }
            if let Some(v) = args.get("archived").and_then(|v| v.as_bool()) {
                c.archived = v;
            }
            c.updated_at = now();
            let out = conv_summary(c);
            st.save()?;
            Ok(json!({"conversation": out}))
        }
        "conversation_search" => {
            let q = req_s(args, "query")?.to_lowercase();
            let st = ctx.store.lock().await;
            let hits: Vec<Value> = st
                .state
                .conversations
                .iter()
                .filter(|c| {
                    c.title.to_lowercase().contains(&q)
                        || c.messages
                            .iter()
                            .any(|m| m.content.to_lowercase().contains(&q))
                })
                .map(|c| {
                    let mut v = conv_summary(c);
                    v["matched_messages"] = json!(c
                        .messages
                        .iter()
                        .filter(|m| m.content.to_lowercase().contains(&q))
                        .count());
                    v
                })
                .collect();
            Ok(json!({"conversations": hits, "query": q}))
        }
        "conversation_clear" => {
            let id = req_s(args, "id")?;
            let mut st = ctx.store.lock().await;
            let c = st
                .state
                .conversations
                .iter_mut()
                .find(|c| c.id == id)
                .ok_or_else(|| anyhow!("conversation not found: {id}"))?;
            let removed = c.messages.len();
            c.messages.clear();
            c.updated_at = now();
            st.save()?;
            Ok(json!({"cleared": removed, "id": id}))
        }
        "conversation_stats" => {
            let st = ctx.store.lock().await;
            if let Some(id) = s(args, "id") {
                let c = st
                    .state
                    .find_conversation(&id)
                    .ok_or_else(|| anyhow!("conversation not found: {id}"))?;
                let by_role: BTreeMap<String, u64> =
                    c.messages.iter().fold(BTreeMap::new(), |mut acc, m| {
                        *acc.entry(m.role.clone()).or_insert(0) += 1;
                        acc
                    });
                return Ok(
                    json!({"id": id, "messages": c.messages.len(), "by_role": by_role,
                                 "est_tokens": c.messages.iter().map(|m| est_tokens(&m.content)).sum::<u64>(),
                                 "pinned": c.pinned, "archived": c.archived}),
                );
            }
            let total_messages: usize = st
                .state
                .conversations
                .iter()
                .map(|c| c.messages.len())
                .sum();
            Ok(json!({"conversations": st.state.conversations.len(),
                      "archived": st.state.conversations.iter().filter(|c| c.archived).count(),
                      "pinned": st.state.conversations.iter().filter(|c| c.pinned).count(),
                      "total_messages": total_messages,
                      "est_tokens": st.state.conversations.iter()
                          .flat_map(|c| c.messages.iter()).map(|m| est_tokens(&m.content)).sum::<u64>()}))
        }
        // ---------- persona / template sharing ----------
        "persona_duplicate" => {
            let id = req_s(args, "id")?;
            let mut st = ctx.store.lock().await;
            let src = st
                .state
                .find_persona(&id)
                .cloned()
                .ok_or_else(|| anyhow!("persona not found: {id}"))?;
            let mut p = src.clone();
            p.id = new_id();
            p.name = s(args, "name").unwrap_or_else(|| format!("{} copy", src.name));
            p.created_at = now();
            p.updated_at = now();
            let out = persona_json(&p);
            st.state.personas.push(p);
            st.save()?;
            Ok(json!({"persona": out}))
        }
        "persona_export" => {
            let st = ctx.store.lock().await;
            let list: Vec<Value> = match s(args, "id") {
                Some(id) => {
                    let p = st
                        .state
                        .find_persona(&id)
                        .ok_or_else(|| anyhow!("persona not found: {id}"))?;
                    vec![persona_json(p)]
                }
                None => st.state.personas.iter().map(persona_json).collect(),
            };
            Ok(json!({"personas": list}))
        }
        "persona_import" => {
            let raw = args
                .get("personas")
                .cloned()
                .ok_or_else(|| anyhow!("missing 'personas'"))?;
            let arr: Vec<Value> = if let Some(a) = raw.as_array() {
                a.clone()
            } else if let Some(a) = raw.get("personas").and_then(|v| v.as_array()) {
                a.clone()
            } else {
                bail!("personas must be an array or {{\"personas\": [...]}}");
            };
            let mut st = ctx.store.lock().await;
            let mut imported = 0;
            let mut skipped: Vec<Value> = vec![];
            for item in &arr {
                let name = item
                    .get("name")
                    .and_then(|v| v.as_str())
                    .unwrap_or("Imported persona");
                let system_prompt = item
                    .get("system_prompt")
                    .and_then(|v| v.as_str())
                    .unwrap_or("");
                if system_prompt.is_empty() {
                    skipped.push(json!({"name": name, "reason": "missing system_prompt"}));
                    continue;
                }
                if st
                    .state
                    .personas
                    .iter()
                    .any(|x| x.name.eq_ignore_ascii_case(name))
                {
                    skipped.push(json!({"name": name, "reason": "duplicate name"}));
                    continue;
                }
                st.state.personas.push(Persona {
                    id: new_id(),
                    name: name.to_string(),
                    description: item
                        .get("description")
                        .and_then(|v| v.as_str())
                        .unwrap_or("")
                        .to_string(),
                    system_prompt: system_prompt.to_string(),
                    model: item
                        .get("model")
                        .and_then(|v| v.as_str())
                        .map(|s| s.to_string()),
                    temperature: item.get("temperature").and_then(|v| v.as_f64()),
                    prompt_versions: vec![],
                    created_at: now(),
                    updated_at: now(),
                });
                imported += 1;
            }
            st.save()?;
            if imported == 0 && !arr.is_empty() {
                bail!("persona_import imported nothing ({} skipped) — see reasons in a retry with valid entries", skipped.len());
            }
            Ok(json!({"imported": imported, "skipped": skipped}))
        }
        "template_duplicate" => {
            let id = req_s(args, "id")?;
            let mut st = ctx.store.lock().await;
            let src = st
                .state
                .find_template(&id)
                .cloned()
                .ok_or_else(|| anyhow!("template not found: {id}"))?;
            let mut t = src.clone();
            t.id = new_id();
            t.name = s(args, "name").unwrap_or_else(|| format!("{} copy", src.name));
            t.created_at = now();
            t.updated_at = now();
            let out = json!({"id": t.id, "name": t.name, "variables": t.variables()});
            st.state.templates.push(t);
            st.save()?;
            Ok(json!({"template": out}))
        }
        _ => dispatch_extra3(ctx, name, args).await,
    }
}

async fn dispatch_extra3(ctx: &Ctx, name: &str, args: &Value) -> Result<Value> {
    match name {
        "memory_export" => {
            let st = ctx.store.lock().await;
            Ok(json!({"memories": st.state.memories.iter()
                .map(|e| serde_json::to_value(e).unwrap()).collect::<Vec<_>>()}))
        }
        "memory_clear" => {
            if !b(args, "confirm", false) {
                bail!("memory_clear requires confirm=true");
            }
            let tag = s(args, "tag");
            let mut st = ctx.store.lock().await;
            let before = st.state.memories.len();
            match &tag {
                Some(t) => st.state.memories.retain(|e| !e.tags.contains(t)),
                None => st.state.memories.clear(),
            }
            let removed = before - st.state.memories.len();
            st.save()?;
            Ok(json!({"removed": removed, "tag": tag}))
        }
        "memory_stats" => {
            let st = ctx.store.lock().await;
            let mut tags: BTreeMap<String, u64> = BTreeMap::new();
            for e in &st.state.memories {
                for t in &e.tags {
                    *tags.entry(t.clone()).or_insert(0) += 1;
                }
            }
            Ok(json!({"entries": st.state.memories.len(), "tags": tags}))
        }
        "document_update" => {
            let id = req_s(args, "id")?;
            let mut st = ctx.store.lock().await;
            let d = st
                .state
                .documents
                .iter_mut()
                .find(|d| d.id == id)
                .ok_or_else(|| anyhow!("document not found: {id}"))?;
            if let Some(v) = s(args, "title") {
                d.title = v;
            }
            if let Some(v) = s(args, "content") {
                d.content = v;
            }
            if args.get("tags").is_some() {
                d.tags = str_list(args, "tags");
            }
            d.updated_at = now();
            let out = serde_json::to_value(&*d)?;
            st.save()?;
            Ok(json!({"document": out}))
        }
        "task_update" => {
            let id = req_s(args, "id")?;
            let mut st = ctx.store.lock().await;
            let t = st
                .state
                .tasks
                .iter_mut()
                .find(|t| t.id == id)
                .ok_or_else(|| anyhow!("task not found: {id}"))?;
            if let Some(v) = s(args, "name") {
                t.name = v;
            }
            if let Some(v) = s(args, "cron") {
                t.cron = v;
            }
            if let Some(v) = s(args, "prompt") {
                t.prompt = v;
            }
            if let Some(v) = s(args, "persona_id") {
                t.persona_id = Some(v);
            }
            if let Some(v) = args.get("enabled").and_then(|v| v.as_bool()) {
                t.enabled = v;
            }
            let out = serde_json::to_value(&*t)?;
            st.save()?;
            Ok(json!({"task": out}))
        }
        "task_set_enabled" => {
            let id = req_s(args, "id")?;
            let enabled = b(args, "enabled", true);
            let mut st = ctx.store.lock().await;
            // A disabled task may still be run explicitly via task_run (operator override).
            if let Some(t) = st.state.tasks.iter_mut().find(|t| t.id == id) {
                t.enabled = enabled;
            } else {
                bail!("task not found: {id}");
            }
            st.save()?;
            Ok(json!({"id": id, "enabled": enabled}))
        }
        "server_info" => {
            let st = ctx.store.lock().await;
            Ok(json!({
                "name": crate::SERVER_NAME, "version": crate::VERSION,
                "transports": ["stdio", "http", "sse"],
                "protocol_versions": crate::SUPPORTED_PROTOCOLS,
                "backend": ctx.backend.name(), "live": ctx.backend.is_live(),
                "started_at": ctx.started_at, "uptime_seconds": ctx.uptime_seconds(),
                "tools": tool_definitions().len(),
                "resources": resource_definitions().len(),
                "prompts": prompt_definitions().len(),
                "state_file": st.path().map(|p| p.display().to_string()),
            }))
        }
        "state_backup" => {
            let st = ctx.store.lock().await;
            let snapshot = serde_json::to_value(&st.state)?;
            let mut backup_file = None;
            if let Some(path) = st.path() {
                let stamp = format!("{}-{}", now().format("%Y%m%d-%H%M%S%.3f"), &new_id()[..8]);
                let dir = path.parent().map(|p| p.join("backups")).unwrap_or_default();
                std::fs::create_dir_all(&dir)?;
                let file = dir.join(format!("grokbot-backup-{stamp}.json"));
                crate::state::atomic_write_pub(&file, &serde_json::to_string_pretty(&snapshot)?)?;
                backup_file = Some(file.display().to_string());
                // Rotation: keep newest MAX_BACKUPS.
                if let Ok(rd) = std::fs::read_dir(&dir) {
                    let mut names: Vec<std::path::PathBuf> = rd
                        .flatten()
                        .map(|e| e.path())
                        .filter(|p| {
                            p.file_name()
                                .map(|n| n.to_string_lossy().starts_with("grokbot-backup-"))
                                .unwrap_or(false)
                        })
                        .collect();
                    names.sort();
                    while names.len() > MAX_BACKUPS {
                        let old = names.remove(0);
                        let _ = std::fs::remove_file(old);
                    }
                }
            }
            Ok(json!({"backup_file": backup_file, "state": snapshot}))
        }
        "state_validate" => {
            let st = ctx.store.lock().await;
            let a = &st.state;
            let mut problems: Vec<String> = vec![];
            let mut seen = std::collections::HashSet::new();
            for p in &a.personas {
                if !seen.insert(p.id.clone()) {
                    problems.push(format!("duplicate persona id {}", p.id));
                }
            }
            seen.clear();
            for c in &a.conversations {
                if !seen.insert(c.id.clone()) {
                    problems.push(format!("duplicate conversation id {}", c.id));
                }
                if let Some(pid) = &c.persona_id {
                    if a.find_persona(pid).is_none() {
                        problems.push(format!(
                            "conversation {} references missing persona {pid}",
                            c.id
                        ));
                    }
                }
            }
            if let Some(pid) = &a.config.active_persona_id {
                if a.find_persona(pid).is_none() {
                    problems.push(format!("active_persona_id {pid} does not exist"));
                }
            }
            if a.config.temperature < 0.0 || a.config.temperature > 2.0 {
                problems.push(format!(
                    "temperature {} out of range 0..2",
                    a.config.temperature
                ));
            }
            Ok(json!({"valid": problems.is_empty(), "problems": problems,
                      "counts": {"personas": a.personas.len(), "conversations": a.conversations.len(),
                                 "templates": a.templates.len(), "memories": a.memories.len(),
                                 "documents": a.documents.len(), "tasks": a.tasks.len()}}))
        }
        "request_log_clear" => {
            if !b(args, "confirm", false) {
                bail!("request_log_clear requires confirm=true");
            }
            let mut st = ctx.store.lock().await;
            let removed = st.state.log.len();
            st.state.log.clear();
            st.save()?;
            Ok(json!({"cleared": removed}))
        }
        _ => dispatch_new(ctx, name, args).await,
    }
}

// ---------- resources & prompts (read by mcp.rs) ----------

pub fn resource_definitions() -> Vec<Value> {
    [
        (
            "grokbot://config",
            "GrokBot config",
            "Current bot configuration as JSON",
        ),
        ("grokbot://personas", "Personas", "All personas as JSON"),
        (
            "grokbot://conversations",
            "Conversations",
            "Conversation summaries as JSON",
        ),
        (
            "grokbot://models",
            "Models",
            "Available Grok models as JSON",
        ),
        (
            "grokbot://usage",
            "Usage",
            "Token/request usage stats as JSON",
        ),
        ("grokbot://log", "Request log", "Recent request log as JSON"),
    ]
    .iter()
    .map(|(uri, name, desc)| {
        json!({
            "uri": uri, "name": name, "description": desc, "mimeType": "application/json"
        })
    })
    .collect()
}

pub async fn read_resource(ctx: &Ctx, uri: &str) -> Result<String> {
    let st = ctx.store.lock().await;
    let v = match uri {
        "grokbot://config" => serde_json::to_value(&st.state.config)?,
        "grokbot://personas" => json!(st
            .state
            .personas
            .iter()
            .map(persona_json)
            .collect::<Vec<_>>()),
        "grokbot://conversations" => json!(st
            .state
            .conversations
            .iter()
            .map(conv_summary)
            .collect::<Vec<_>>()),
        "grokbot://usage" => {
            json!({"total_requests": st.state.usage.total_requests, "total_tokens": st.state.usage.total_tokens(), "by_model": st.state.usage.by_model})
        }
        "grokbot://log" => json!(st
            .state
            .log
            .iter()
            .rev()
            .take(50)
            .map(|e| serde_json::to_value(e).unwrap())
            .collect::<Vec<_>>()),
        "grokbot://models" => {
            drop(st);
            return Ok(serde_json::to_string_pretty(
                &ctx.backend.list_models().await?,
            )?);
        }
        other => bail!("unknown resource: {other}"),
    };
    Ok(serde_json::to_string_pretty(&v)?)
}

pub fn prompt_definitions() -> Vec<Value> {
    vec![
        json!({"name": "chat_with_persona", "description": "Chat with GrokBot as a named persona",
               "arguments": [{"name": "persona", "description": "Persona name or ID", "required": true},
                             {"name": "prompt", "description": "User prompt", "required": true}]}),
        json!({"name": "summarize_conversation", "description": "Summarise a stored conversation",
               "arguments": [{"name": "conversation_id", "description": "Conversation ID", "required": true}]}),
        json!({"name": "code_review", "description": "Review code with GrokBot's Coder persona",
               "arguments": [{"name": "code", "description": "Code to review", "required": true},
                             {"name": "language", "description": "Language", "required": false}]}),
    ]
}

pub fn get_prompt(name: &str, args: &Value) -> Result<Value> {
    let arg = |k: &str| {
        args.get(k)
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string()
    };
    let (desc, text) = match name {
        "chat_with_persona" => ("Chat with a GrokBot persona",
            format!("Act as the GrokBot persona '{}'. System instructions come from that persona.\n\nUser: {}", arg("persona"), arg("prompt"))),
        "summarize_conversation" => ("Summarise a conversation",
            format!("Use conversation_summarize / conversation_get for conversation {} and return a faithful summary with key points.", arg("conversation_id"))),
        "code_review" => ("Code review",
            format!("Review this {} code for bugs, security risks, and simplifications. Be specific and cite line-level issues:\n\n{}", arg("language"), arg("code"))),
        other => bail!("unknown prompt: {other}"),
    };
    Ok(json!({"description": desc, "messages": [
        {"role": "user", "content": {"type": "text", "text": text}}]}))
}

pub async fn call_tool_boxed(ctx: &Ctx, name: &str, args: &Value) -> Result<Value> {
    Box::pin(call_tool(ctx, name, args)).await
}

// ---------- new surface: bots, routines, jobs, media, semantic, admin ----------

fn bot_profile_json(b: &BotProfile, active: bool) -> Value {
    json!({"id": b.id, "name": b.name, "description": b.description, "active": active,
           "model": b.config.default_model, "created_at": b.created_at, "updated_at": b.updated_at})
}

fn find_bot(state: &AppState, id_or_name: &str) -> Option<BotProfile> {
    state
        .bots
        .iter()
        .find(|b| b.id == id_or_name || b.name.eq_ignore_ascii_case(id_or_name))
        .cloned()
}

pub fn validate_cron(cron: &str) -> Result<()> {
    let fields: Vec<&str> = cron.split_whitespace().collect();
    if fields.len() != 5 {
        bail!("cron must have 5 fields (minute hour day-of-month month day-of-week), got '{cron}'");
    }
    let ranges = [(0, 59), (0, 23), (1, 31), (1, 12), (0, 7)];
    for (f, (lo, hi)) in fields.iter().zip(ranges.iter()) {
        validate_cron_field(f, *lo, *hi)?;
    }
    Ok(())
}

fn validate_cron_field(field: &str, lo: i64, hi: i64) -> Result<()> {
    for part in field.split(',') {
        let base = part.split('/').next().unwrap_or("");
        if base == "*" || base.is_empty() {
            continue;
        }
        if let Some((a, b)) = base.split_once('-') {
            let (a, b) = (
                a.parse::<i64>()
                    .map_err(|_| anyhow!("bad cron value '{a}'"))?,
                b.parse::<i64>()
                    .map_err(|_| anyhow!("bad cron value '{b}'"))?,
            );
            if a < lo || b > hi || a > b {
                bail!("cron range {a}-{b} out of bounds {lo}-{hi}");
            }
        } else {
            let v: i64 = base
                .parse()
                .map_err(|_| anyhow!("bad cron value '{base}'"))?;
            if v < lo || v > hi {
                bail!("cron value {v} out of bounds {lo}-{hi}");
            }
        }
    }
    Ok(())
}

fn cron_field_matches(field: &str, value: i64, _is_dow: bool) -> bool {
    field.split(',').any(|part| {
        let (base, step) = match part.split_once('/') {
            Some((b, s)) => (b, s.parse::<i64>().unwrap_or(1).max(1)),
            None => (part, 1),
        };
        if base == "*" || base.is_empty() {
            return value % step == 0;
        }
        if let Some((a, b)) = base.split_once('-') {
            if let (Ok(a), Ok(b)) = (a.parse::<i64>(), b.parse::<i64>()) {
                return value >= a && value <= b && (value - a) % step == 0;
            }
            return false;
        }
        base.parse::<i64>().map(|v| v == value).unwrap_or(false)
    })
}

pub fn cron_matches(cron: &str, dt: chrono::DateTime<chrono::Utc>) -> bool {
    use chrono::{Datelike, Timelike};
    let f: Vec<&str> = cron.split_whitespace().collect();
    if f.len() != 5 {
        return false;
    }
    let dow = dt.weekday().num_days_from_sunday() as i64;
    cron_field_matches(f[0], dt.minute() as i64, false)
        && cron_field_matches(f[1], dt.hour() as i64, false)
        && cron_field_matches(f[2], dt.day() as i64, false)
        && cron_field_matches(f[3], dt.month() as i64, false)
        && (cron_field_matches(f[4], dow, true) || (dow == 0 && cron_field_matches(f[4], 7, true)))
}

pub fn next_run_after(
    cron: &str,
    from: chrono::DateTime<chrono::Utc>,
) -> Option<chrono::DateTime<chrono::Utc>> {
    let start_ts = (from.timestamp() / 60 + 1) * 60;
    let mut t = chrono::DateTime::<chrono::Utc>::from_timestamp(start_ts, 0)?;
    for _ in 0..(366 * 24 * 60) {
        if cron_matches(cron, t) {
            return Some(t);
        }
        t += chrono::Duration::minutes(1);
    }
    None
}

/// Run one scheduled task by id (shared by task_run, routines, scheduler).
pub async fn run_task_by_id(ctx: &Ctx, id: &str) -> Result<Value> {
    call_tool_boxed(ctx, "task_run", &json!({"id": id})).await
}

async fn dispatch_new(ctx: &Ctx, name: &str, args: &Value) -> Result<Value> {
    match name {
        // ---------- bots registry ----------
        "list_bots" => {
            let st = ctx.store.lock().await;
            let active = st.state.active_bot_id.clone();
            Ok(
                json!({"bots": st.state.bots.iter().map(|b| bot_profile_json(b, active.as_deref() == Some(b.id.as_str()))).collect::<Vec<_>>(),
                      "live_bot_name": st.state.config.name}),
            )
        }
        "create_bot" => {
            let bot_name = req_s(args, "name")?;
            let mut st = ctx.store.lock().await;
            if st
                .state
                .bots
                .iter()
                .any(|b| b.name.eq_ignore_ascii_case(&bot_name))
            {
                bail!("a bot named '{bot_name}' already exists");
            }
            let mut config = st.state.config.clone();
            if let Some(v) = s(args, "system_prompt") {
                config.system_prompt = v;
            }
            if let Some(v) = s(args, "model") {
                config.default_model = v;
            }
            config.name = bot_name.clone();
            let bot = BotProfile {
                id: new_id(),
                name: bot_name,
                description: s(args, "description").unwrap_or_default(),
                config,
                created_at: now(),
                updated_at: now(),
            };
            let out = bot_profile_json(&bot, false);
            let bid = bot.id.clone();
            st.state.bots.push(bot);
            st.save()?;
            drop(st);
            if b(args, "activate", false) {
                return call_tool_boxed(ctx, "activate_bot", &json!({"id": bid})).await;
            }
            Ok(json!({"bot": out}))
        }
        "update_bot" => {
            let id = req_s(args, "id")?;
            let mut st = ctx.store.lock().await;
            let bot_id = find_bot(&st.state, &id)
                .ok_or_else(|| anyhow!("bot not found: {id}"))?
                .id;
            if let Some(v) = s(args, "name") {
                if st
                    .state
                    .bots
                    .iter()
                    .any(|b| b.id != bot_id && b.name.eq_ignore_ascii_case(&v))
                {
                    bail!("a bot named '{v}' already exists");
                }
            }
            let is_active = st.state.active_bot_id.as_deref() == Some(bot_id.as_str());
            let bot = st
                .state
                .bots
                .iter_mut()
                .find(|b| b.id == bot_id)
                .ok_or_else(|| anyhow!("bot not found: {id}"))?;
            if let Some(v) = s(args, "name") {
                bot.name = v.clone();
                bot.config.name = v;
            }
            if let Some(v) = s(args, "description") {
                bot.description = v;
            }
            if let Some(v) = s(args, "system_prompt") {
                bot.config.system_prompt = v;
            }
            if let Some(v) = s(args, "model") {
                bot.config.default_model = v;
            }
            bot.updated_at = now();
            let out = bot_profile_json(bot, is_active);
            st.save()?;
            Ok(json!({"bot": out}))
        }
        "delete_bot" => {
            let id = req_s(args, "id")?;
            let confirm = req_s(args, "confirm")?;
            let mut st = ctx.store.lock().await;
            let bot = find_bot(&st.state, &id).ok_or_else(|| anyhow!("bot not found: {id}"))?;
            if confirm != bot.name {
                bail!("confirm must exactly repeat the bot name '{}'", bot.name);
            }
            st.state.bots.retain(|b| b.id != bot.id);
            if st.state.active_bot_id.as_deref() == Some(bot.id.as_str()) {
                st.state.active_bot_id = None;
            }
            st.save()?;
            Ok(json!({"deleted": bot.id, "name": bot.name}))
        }
        "activate_bot" => {
            let id = req_s(args, "id")?;
            let mut st = ctx.store.lock().await;
            let target = find_bot(&st.state, &id).ok_or_else(|| anyhow!("bot not found: {id}"))?;
            // Save the live config back into the previously active profile.
            let live_config = st.state.config.clone();
            if let Some(prev_id) = st.state.active_bot_id.clone() {
                if let Some(prev) = st.state.bots.iter_mut().find(|b| b.id == prev_id) {
                    prev.config = live_config;
                    prev.updated_at = now();
                }
            }
            st.state.config = target.config.clone();
            st.state.active_bot_id = Some(target.id.clone());
            st.save()?;
            Ok(
                json!({"active_bot": bot_profile_json(&target, true), "live_bot_name": st.state.config.name}),
            )
        }
        "bot_status" => {
            let jobs = ctx.jobs.lock().await;
            let working = jobs.values().filter(|j| j.status == "running").count();
            let done = jobs.values().filter(|j| j.status == "done").count();
            drop(jobs);
            let st = ctx.store.lock().await;
            Ok(json!({
                "bot": st.state.config.name, "status": if working > 0 { "working" } else { "idle" },
                "active_bot_id": st.state.active_bot_id, "bots_registered": st.state.bots.len(),
                "jobs_running": working, "jobs_done": done,
                "backend": ctx.backend.name(),
                "usage": {"requests": st.state.usage.total_requests, "total_tokens": st.state.usage.total_tokens(), "images": st.state.usage.total_images},
            }))
        }
        _ => dispatch_new2(ctx, name, args).await,
    }
}

async fn dispatch_new2(ctx: &Ctx, name: &str, args: &Value) -> Result<Value> {
    match name {
        // ---------- routines (unified, bridge parity) ----------
        "routines" => {
            let action = req_s(args, "action")?;
            match action.as_str() {
                "list" => call_tool_boxed(ctx, "task_list", &json!({})).await,
                "add" => {
                    let cron = req_s(args, "cron")?;
                    validate_cron(&cron)?;
                    let mut a = json!({"name": req_s(args, "name")?, "cron": cron, "prompt": req_s(args, "prompt")?});
                    if let Some(p) = s(args, "persona_id") {
                        a["persona_id"] = json!(p);
                    }
                    if let Some(w) = s(args, "webhook_url") {
                        a["webhook_url"] = json!(w);
                    }
                    let out = call_tool_boxed(ctx, "task_create", &a).await?;
                    Ok(json!({"ok": true, "routine": out["task"], "action": "add"}))
                }
                "run" => {
                    let id = req_s(args, "id")?;
                    let out = run_task_by_id(ctx, &id).await?;
                    Ok(
                        json!({"ok": true, "routine_id": id, "result": out["result"], "action": "run"}),
                    )
                }
                "enable" | "disable" => {
                    let id = req_s(args, "id")?;
                    let out = call_tool_boxed(
                        ctx,
                        "task_set_enabled",
                        &json!({"id": id, "enabled": action == "enable"}),
                    )
                    .await?;
                    Ok(json!({"ok": true, "routine": out, "action": action}))
                }
                "remove" => {
                    let id = req_s(args, "id")?;
                    call_tool_boxed(ctx, "task_delete", &json!({"id": id})).await?;
                    Ok(json!({"ok": true, "removed": id, "action": "remove"}))
                }
                other => {
                    bail!("unknown routines action '{other}' (list|add|run|enable|disable|remove)")
                }
            }
        }
        // ---------- async jobs ----------
        "chat_async" => {
            let prompt = req_s(args, "prompt")?;
            let job_id = new_id();
            let notify = Arc::new(tokio::sync::Notify::new());
            ctx.cancels
                .lock()
                .await
                .insert(job_id.clone(), notify.clone());
            ctx.jobs.lock().await.insert(
                job_id.clone(),
                Job {
                    id: job_id.clone(),
                    kind: "chat".into(),
                    status: "running".into(),
                    result: None,
                    created_at: now(),
                },
            );
            let ctx2 = ctx.clone();
            let args2 = args.clone();
            let job_id2 = job_id.clone();
            // spawn_blocking + own runtime: call_tool futures are not
            // required to be Send on this path.
            tokio::task::spawn_blocking(move || {
                let rt = match tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                {
                    Ok(r) => r,
                    Err(_) => return,
                };
                rt.block_on(async move {
                    let mut a = json!({"prompt": prompt}).as_object().unwrap().clone();
                    for k in ["persona_id", "model"] {
                        if let Some(v) = args2.get(k) {
                            a.insert(k.into(), v.clone());
                        }
                    }
                    let call_args = Value::Object(a);
                    let result = tokio::select! {
                        r = call_tool(&ctx2, "chat", &call_args) => Some(r),
                        _ = notify.notified() => None,
                    };
                    let mut jobs = ctx2.jobs.lock().await;
                    if let Some(j) = jobs.get_mut(&job_id2) {
                        match result {
                            Some(Ok(v)) => {
                                j.status = "done".into();
                                j.result = Some(v);
                            }
                            Some(Err(e)) => {
                                j.status = "failed".into();
                                j.result = Some(json!({"error": format!("{e:#}")}));
                            }
                            None => {
                                j.status = "cancelled".into();
                            }
                        }
                    }
                });
            });
            Ok(json!({"job_id": job_id, "status": "running"}))
        }
        "job_status" => {
            let id = req_s(args, "id")?;
            let jobs = ctx.jobs.lock().await;
            let j = jobs
                .get(&id)
                .ok_or_else(|| anyhow!("job not found: {id}"))?;
            Ok(
                json!({"job": {"id": j.id, "kind": j.kind, "status": j.status, "result": j.result, "created_at": j.created_at}}),
            )
        }
        "job_wait" => {
            let id = req_s(args, "id")?;
            let timeout = u(args, "timeout_seconds").unwrap_or(60).min(3600);
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(timeout);
            loop {
                {
                    let jobs = ctx.jobs.lock().await;
                    let j = jobs
                        .get(&id)
                        .ok_or_else(|| anyhow!("job not found: {id}"))?;
                    if j.status != "running" {
                        return Ok(
                            json!({"job": {"id": j.id, "kind": j.kind, "status": j.status, "result": j.result}}),
                        );
                    }
                }
                if std::time::Instant::now() > deadline {
                    return Ok(
                        json!({"job": {"id": id, "status": "running"}, "timed_out": true,
                                     "hint": "still working — call job_wait again or bot_stop"}),
                    );
                }
                tokio::time::sleep(std::time::Duration::from_millis(200)).await;
            }
        }
        "job_list" => {
            let jobs = ctx.jobs.lock().await;
            let mut list: Vec<Value> = jobs.values().map(|j| json!({"id": j.id, "kind": j.kind, "status": j.status, "created_at": j.created_at})).collect();
            list.sort_by(|a, b| a["created_at"].as_str().cmp(&b["created_at"].as_str()));
            Ok(json!({"jobs": list}))
        }
        "bot_stop" => {
            let target = match s(args, "id") {
                Some(id) => id,
                None => {
                    let jobs = ctx.jobs.lock().await;
                    jobs.values()
                        .filter(|j| j.status == "running")
                        .max_by_key(|j| j.created_at)
                        .map(|j| j.id.clone())
                        .ok_or_else(|| anyhow!("no running job to stop"))?
                }
            };
            if let Some(n) = ctx.cancels.lock().await.get(&target) {
                n.notify_waiters();
            }
            // Give the task a moment to observe cancellation.
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
            let mut jobs = ctx.jobs.lock().await;
            if let Some(j) = jobs.get_mut(&target) {
                if j.status == "running" {
                    j.status = "cancelled".into();
                }
                return Ok(json!({"stopped": target, "status": j.status}));
            }
            bail!("job not found: {target}")
        }
        _ => dispatch_new3(ctx, name, args).await,
    }
}

async fn dispatch_new3(ctx: &Ctx, name: &str, args: &Value) -> Result<Value> {
    match name {
        "chat_stream" => {
            let prompt = req_s(args, "prompt")?;
            let (resolved, mut req) = {
                let st = ctx.store.lock().await;
                let r = resolve_for(&st.state, args, None, None);
                let mut q = ChatRequest::new(
                    r.model.clone(),
                    vec![
                        Message::new("system", r.system.clone()),
                        Message::new("user", prompt.clone()),
                    ],
                    r.temperature,
                    r.max_tokens,
                );
                q.top_p = Some(r.top_p);
                q.response_format = args.get("response_format").cloned();
                q.reasoning_effort = s(args, "reasoning_effort");
                (r, q)
            };
            let _ = &mut req;
            let mut deltas = 0usize;
            let mut assembled = String::new();
            let reply = {
                let mut sink = |d: String| {
                    deltas += 1;
                    assembled.push_str(&d);
                };
                ctx.backend.chat_stream(req, &mut sink).await?
            };
            let mut st = ctx.store.lock().await;
            st.record_usage(&reply.model, reply.prompt_tokens, reply.completion_tokens);
            st.record(
                name,
                ctx.backend.name(),
                Some(reply.model.clone()),
                true,
                format!("{deltas} deltas"),
            );
            st.save()?;
            Ok(
                json!({"reply": reply.content, "model": reply.model, "backend": reply.backend,
                      "deltas": deltas, "assembled_matches": assembled == reply.content,
                      "persona": resolved.persona_name}),
            )
        }
        "web_search" | "x_search" => {
            let query = req_s(args, "query")?;
            let mode = if name == "web_search" {
                crate::backend::SearchMode::Web
            } else {
                crate::backend::SearchMode::X
            };
            if !ctx.backend.is_live() {
                // Honest mock: search local documents instead, labelled.
                let hits =
                    call_tool_boxed(ctx, "document_search", &json!({"query": query})).await?;
                return Ok(
                    json!({"query": query, "backend": "mock", "live_search": false,
                                 "note": "mock backend cannot search the live web/X — returning local document matches instead",
                                 "documents": hits["documents"]}),
                );
            }
            let (model, max) = {
                let st = ctx.store.lock().await;
                (
                    s(args, "model").unwrap_or_else(|| st.state.config.default_model.clone()),
                    st.state.config.max_tokens,
                )
            };
            let mut req = ChatRequest::new(
                model,
                vec![
                    Message::new("system", "Search and answer with sources. Cite URLs."),
                    Message::new("user", query.clone()),
                ],
                0.7,
                max,
            );
            req.search = mode;
            let reply = do_chat_req(ctx, name, req).await?;
            Ok(
                json!({"query": query, "reply": reply.content, "model": reply.model,
                      "citations": reply.citations, "backend": reply.backend}),
            )
        }
        "video_generate" => {
            let prompt = req_s(args, "prompt")?;
            let model = s(args, "model").unwrap_or_else(|| "grok-imagine-video".into());
            let v = ctx.backend.video_generate(&prompt, &model).await?;
            let mut st = ctx.store.lock().await;
            st.record(name, ctx.backend.name(), Some(model), true, "video");
            st.save()?;
            Ok(json!({"video": v}))
        }
        "file_upload" => {
            use base64::Engine;
            let filename = req_s(args, "filename")?;
            if filename.chars().count() > 255 {
                bail!("filename too long");
            }
            let b64 = req_s(args, "content_base64")?;
            let bytes = base64::engine::general_purpose::STANDARD
                .decode(&b64)
                .map_err(|e| anyhow!("content_base64 is not valid base64: {e}"))?;
            if bytes.len() > 10 * 1024 * 1024 {
                bail!("file too large (max 10 MiB)");
            }
            let info = ctx.backend.file_upload(&filename, bytes.clone()).await?;
            if b(args, "store_as_document", false) {
                let text = String::from_utf8_lossy(&bytes).to_string();
                call_tool_boxed(
                    ctx,
                    "document_add",
                    &json!({"title": filename, "content": text}),
                )
                .await?;
            }
            Ok(json!({"file": info}))
        }
        "file_list" => {
            if ctx.backend.is_live() {
                let files = ctx.backend.file_list().await?;
                return Ok(json!({"files": files, "backend": "xai-live"}));
            }
            let st = ctx.store.lock().await;
            let files: Vec<Value> = st
                .state
                .documents
                .iter()
                .filter(|d| d.xai_file_id.is_some())
                .map(|d| json!({"id": d.xai_file_id, "filename": d.title, "backend": "mock"}))
                .collect();
            Ok(json!({"files": files, "backend": "mock",
                      "note": "mock mode lists documents registered with an xai_file_id; uploads via file_upload return mock ids"}))
        }
        "file_delete" => {
            let id = req_s(args, "id")?;
            ctx.backend.file_delete(&id).await?;
            Ok(json!({"deleted": id}))
        }
        "audio_transcribe" => {
            let audio = req_s(args, "audio_base64")?;
            if audio.len() > 34 * 1024 * 1024 {
                bail!("audio too large (max ~25 MiB decoded)");
            }
            let format = s(args, "format").unwrap_or_else(|| "mp3".into());
            let model = s(args, "model").unwrap_or_else(|| "grok-4".into());
            let text = ctx
                .backend
                .audio_transcribe(&audio, &format, &model)
                .await?;
            Ok(json!({"text": text, "backend": ctx.backend.name()}))
        }
        "audio_speak" => {
            let text = req_s(args, "text")?;
            if text.chars().count() > 4096 {
                bail!("text too long for speech (max 4096 chars)");
            }
            let voice = s(args, "voice").unwrap_or_else(|| "alloy".into());
            let model = s(args, "model").unwrap_or_else(|| "grok-4".into());
            let audio = ctx.backend.audio_speak(&text, &voice, &model).await?;
            Ok(json!({"audio_base64": audio, "voice": voice, "backend": ctx.backend.name()}))
        }
        _ => dispatch_new4(ctx, name, args).await,
    }
}

fn cosine(a: &[f32], b: &[f32]) -> f64 {
    if a.len() != b.len() || a.is_empty() {
        return 0.0;
    }
    let dot: f64 = a
        .iter()
        .zip(b.iter())
        .map(|(x, y)| (*x as f64) * (*y as f64))
        .sum();
    let na: f64 = a.iter().map(|x| (*x as f64).powi(2)).sum::<f64>().sqrt();
    let nb: f64 = b.iter().map(|x| (*x as f64).powi(2)).sum::<f64>().sqrt();
    if na == 0.0 || nb == 0.0 {
        0.0
    } else {
        dot / (na * nb)
    }
}

async fn dispatch_new4(ctx: &Ctx, name: &str, args: &Value) -> Result<Value> {
    match name {
        "embed_text" => {
            let texts = str_list(args, "texts");
            if texts.is_empty() {
                bail!("texts must be a non-empty array");
            }
            if texts.len() > 100 {
                bail!("max 100 texts per call");
            }
            let model = s(args, "model")
                .or_else(|| std::env::var("GROK_EMBED_MODEL").ok())
                .unwrap_or_else(|| "mock-embed-32".into());
            let vectors = ctx.backend.embed(&texts, &model).await?;
            Ok(
                json!({"model": model, "dimensions": vectors.first().map(|v| v.len()).unwrap_or(0),
                      "embeddings": vectors, "backend": ctx.backend.name()}),
            )
        }
        "semantic_search" => {
            let query = req_s(args, "query")?;
            let limit = u(args, "limit").unwrap_or(10).min(50) as usize;
            let model =
                std::env::var("GROK_EMBED_MODEL").unwrap_or_else(|_| "mock-embed-32".into());
            // Ensure embeddings exist for documents/memories (cache in state).
            type IdText = Vec<(String, String)>;
            let (mut need_docs, mut need_mems): (IdText, IdText) = (vec![], vec![]);
            {
                let st = ctx.store.lock().await;
                for d in &st.state.documents {
                    if d.embedding.is_none() {
                        need_docs.push((d.id.clone(), format!("{} {}", d.title, d.content)));
                    }
                }
                for m in &st.state.memories {
                    if m.embedding.is_none() {
                        need_mems.push((m.id.clone(), format!("{} {}", m.key, m.value)));
                    }
                }
            }
            let mut embed_and_store = |_is_doc: bool| {};
            let _ = &mut embed_and_store;
            if !need_docs.is_empty() || !need_mems.is_empty() {
                let mut all: Vec<String> = need_docs.iter().map(|(_, t)| t.clone()).collect();
                all.extend(need_mems.iter().map(|(_, t)| t.clone()));
                let vectors = ctx.backend.embed(&all, &model).await?;
                let mut st = ctx.store.lock().await;
                let mut vi = vectors.into_iter();
                for (id, _) in &need_docs {
                    if let Some(v) = vi.next() {
                        if let Some(d) = st.state.documents.iter_mut().find(|d| &d.id == id) {
                            d.embedding = Some(v);
                        }
                    }
                }
                for (id, _) in &need_mems {
                    if let Some(v) = vi.next() {
                        if let Some(m) = st.state.memories.iter_mut().find(|m| &m.id == id) {
                            m.embedding = Some(v);
                        }
                    }
                }
                st.save()?;
            }
            let qv = ctx
                .backend
                .embed(std::slice::from_ref(&query), &model)
                .await?
                .into_iter()
                .next()
                .ok_or_else(|| anyhow!("no query embedding"))?;
            let st = ctx.store.lock().await;
            let mut hits: Vec<Value> = vec![];
            for d in &st.state.documents {
                if let Some(e) = &d.embedding {
                    hits.push(json!({"kind": "document", "id": d.id, "title": d.title,
                                     "score": cosine(&qv, e),
                                     "snippet": d.content.chars().take(200).collect::<String>()}));
                }
            }
            for m in &st.state.memories {
                if let Some(e) = &m.embedding {
                    hits.push(json!({"kind": "memory", "id": m.id, "title": m.key,
                                     "score": cosine(&qv, e),
                                     "snippet": m.value.chars().take(200).collect::<String>()}));
                }
            }
            hits.sort_by(|a, b| {
                b["score"]
                    .as_f64()
                    .partial_cmp(&a["score"].as_f64())
                    .unwrap_or(std::cmp::Ordering::Equal)
            });
            hits.truncate(limit);
            Ok(json!({"query": query, "hits": hits, "backend": ctx.backend.name()}))
        }
        "conversation_message_edit" => {
            let id = req_s(args, "id")?;
            let mid = req_s(args, "message_id")?;
            let content = req_s(args, "content")?;
            if content.chars().count() > MAX_MESSAGE_CHARS {
                bail!("message too long (max {MAX_MESSAGE_CHARS} chars)");
            }
            let mut st = ctx.store.lock().await;
            let c = st
                .state
                .conversations
                .iter_mut()
                .find(|c| c.id == id)
                .ok_or_else(|| anyhow!("conversation not found: {id}"))?;
            let m = c
                .messages
                .iter_mut()
                .find(|m| m.id == mid)
                .ok_or_else(|| anyhow!("message not found: {mid}"))?;
            m.content = content;
            m.edited_at = Some(now());
            c.updated_at = now();
            let out = conv_json(c);
            st.save()?;
            Ok(json!({"conversation": out}))
        }
        "conversation_message_delete" => {
            let id = req_s(args, "id")?;
            let mid = req_s(args, "message_id")?;
            let mut st = ctx.store.lock().await;
            let c = st
                .state
                .conversations
                .iter_mut()
                .find(|c| c.id == id)
                .ok_or_else(|| anyhow!("conversation not found: {id}"))?;
            let before = c.messages.len();
            c.messages.retain(|m| m.id != mid);
            if c.messages.len() == before {
                bail!("message not found: {mid}");
            }
            c.updated_at = now();
            st.save()?;
            Ok(json!({"deleted": mid, "conversation_id": id}))
        }
        "persona_prompt_history" => {
            let id = req_s(args, "id")?;
            let st = ctx.store.lock().await;
            let p = st
                .state
                .find_persona(&id)
                .ok_or_else(|| anyhow!("persona not found: {id}"))?;
            Ok(json!({"persona": p.name, "versions": p.prompt_versions}))
        }
        "persona_rollback" => {
            let id = req_s(args, "id")?;
            let version = u(args, "version").ok_or_else(|| anyhow!("missing version"))? as u32;
            let mut st = ctx.store.lock().await;
            let p = st
                .state
                .personas
                .iter_mut()
                .find(|p| p.id == id || p.name.eq_ignore_ascii_case(&id))
                .ok_or_else(|| anyhow!("persona not found: {id}"))?;
            let target = p
                .prompt_versions
                .iter()
                .find(|v| v.version == version)
                .cloned()
                .ok_or_else(|| anyhow!("prompt version {version} not found"))?;
            let next = p
                .prompt_versions
                .iter()
                .map(|v| v.version)
                .max()
                .unwrap_or(0)
                + 1;
            p.system_prompt = target.system_prompt.clone();
            p.prompt_versions.push(PromptVersion {
                version: next,
                system_prompt: target.system_prompt.clone(),
                note: format!("rollback to v{version}"),
                ts: now(),
            });
            p.updated_at = now();
            let out = persona_json(p);
            st.save()?;
            Ok(json!({"persona": out, "rolled_back_to": version, "new_version": next}))
        }
        "persona_ab_test" => {
            let prompt = req_s(args, "prompt")?;
            let pa = req_s(args, "persona_a")?;
            let pb = req_s(args, "persona_b")?;
            let mut results = vec![];
            for (label, pid) in [("a", pa), ("b", pb)] {
                let a2 = json!({"prompt": prompt, "persona_id": pid});
                match call_tool_boxed(ctx, "chat", &a2).await {
                    Ok(v) => results.push(json!({"side": label, "persona": pid, "ok": true, "reply": v["reply"],
                                                 "prompt_tokens": v["prompt_tokens"], "completion_tokens": v["completion_tokens"]})),
                    Err(e) => results.push(json!({"side": label, "persona": pid, "ok": false, "error": format!("{e:#}")})),
                }
            }
            Ok(json!({"prompt": prompt, "results": results}))
        }
        "bot_tool_define" => {
            let def_name = req_s(args, "name")?;
            let params = args
                .get("parameters")
                .cloned()
                .ok_or_else(|| anyhow!("missing parameters (JSON Schema object)"))?;
            if !params.is_object() {
                bail!("parameters must be a JSON Schema object");
            }
            let mut st = ctx.store.lock().await;
            let def = BotToolDef {
                name: def_name.clone(),
                description: s(args, "description").unwrap_or_default(),
                parameters: params,
                enabled: b(args, "enabled", true),
            };
            if let Some(existing) = st
                .state
                .config
                .bot_tool_defs
                .iter_mut()
                .find(|d| d.name == def_name)
            {
                *existing = def;
            } else {
                st.state.config.bot_tool_defs.push(def);
            }
            st.save()?;
            Ok(json!({"defined": def_name, "tools": st.state.config.bot_tool_defs.len()}))
        }
        "bot_tool_defs_list" => {
            let st = ctx.store.lock().await;
            Ok(json!({"tools": st.state.config.bot_tool_defs}))
        }
        "bot_tool_delete" => {
            let def_name = req_s(args, "name")?;
            let mut st = ctx.store.lock().await;
            let before = st.state.config.bot_tool_defs.len();
            st.state.config.bot_tool_defs.retain(|d| d.name != def_name);
            if st.state.config.bot_tool_defs.len() == before {
                bail!("bot tool not found: {def_name}");
            }
            st.save()?;
            Ok(json!({"deleted": def_name}))
        }
        _ => dispatch_new5(ctx, name, args).await,
    }
}

async fn dispatch_new5(ctx: &Ctx, name: &str, args: &Value) -> Result<Value> {
    match name {
        "list_backups" => {
            let st = ctx.store.lock().await;
            let Some(path) = st.path().map(|p| p.to_path_buf()) else {
                return Ok(json!({"backups": [], "note": "in-memory store has no backup files"}));
            };
            drop(st);
            let dir = path.parent().map(|p| p.join("backups")).unwrap_or_default();
            let mut names: Vec<Value> = vec![];
            if let Ok(rd) = std::fs::read_dir(&dir) {
                for e in rd.flatten() {
                    let n = e.file_name().to_string_lossy().to_string();
                    if n.starts_with("grokbot-backup-") {
                        let size = e.metadata().map(|m| m.len()).unwrap_or(0);
                        names.push(json!({"backup": n, "bytes": size}));
                    }
                }
            }
            names.sort_by(|a, b| a["backup"].as_str().cmp(&b["backup"].as_str()));
            Ok(json!({"backups": names, "dir": dir.display().to_string()}))
        }
        "state_restore" => {
            if !b(args, "confirm", false) {
                bail!("state_restore requires confirm=true");
            }
            let backup = req_s(args, "backup")?;
            if backup.contains('/') || backup.contains('\\') || backup.starts_with('.') {
                bail!("backup must be a plain file name from list_backups");
            }
            // Auto-backup current state first.
            let _ = call_tool_boxed(ctx, "state_backup", &json!({})).await?;
            let raw_path = {
                let st = ctx.store.lock().await;
                let base = st
                    .path()
                    .ok_or_else(|| anyhow!("no state file configured"))?;
                base.parent()
                    .map(|p| p.join("backups").join(&backup))
                    .ok_or_else(|| anyhow!("cannot resolve backup path"))?
            };
            let raw = std::fs::read_to_string(&raw_path)
                .with_context(|| format!("read backup {}", raw_path.display()))?;
            let mut new_state: AppState = serde_json::from_str(&raw)?;
            crate::state::backfill_message_ids(&mut new_state);
            let mut st = ctx.store.lock().await;
            st.state = new_state;
            st.save()?;
            Ok(json!({"restored": backup}))
        }
        "usage_quota" => {
            let st = ctx.store.lock().await;
            let cfg = &st.state.config;
            let mut per_model: Vec<Value> = vec![];
            let mut total_est = 0.0;
            for (m, tk) in &st.state.usage.by_model_tokens {
                let cost = crate::backend::est_cost_usd(m, tk.prompt_tokens, tk.completion_tokens)
                    + crate::backend::image_cost_usd(m, tk.images);
                total_est += cost;
                per_model.push(json!({"model": m, "requests": tk.requests, "prompt_tokens": tk.prompt_tokens,
                                      "completion_tokens": tk.completion_tokens, "images": tk.images,
                                      "estimated_usd": cost, "pricing_known": crate::backend::pricing_for(m) != crate::backend::Pricing::Unknown}));
            }
            Ok(json!({
                "daily_budget_usd": cfg.daily_budget_usd,
                "rate_limit_per_minute": cfg.rate_limit_per_minute,
                "estimated_spend_usd": total_est,
                "budget_remaining_usd": cfg.daily_budget_usd.map(|cap| (cap - total_est).max(0.0)),
                "per_model": per_model,
                "note": "estimates from recorded usage and approximate pricing; unknown models price as $0 and are flagged",
            }))
        }
        other => bail!("unknown tool: {other}"),
    }
}

/// Cron scheduler: every 30s, run due enabled tasks, stamp next run,
/// fire optional completion webhooks. Spawned in serve modes only.
pub async fn scheduler_loop(ctx: Ctx) {
    let mut tick = tokio::time::interval(std::time::Duration::from_secs(30));
    loop {
        tick.tick().await;
        let due: Vec<String> = {
            let mut st = ctx.store.lock().await;
            let now_ts = now();
            let mut due = vec![];
            for task in st.state.tasks.iter_mut() {
                if !task.enabled {
                    continue;
                }
                if task.next_run_at.is_none() {
                    task.next_run_at = next_run_after(&task.cron, task.created_at);
                }
                if let Some(next) = task.next_run_at {
                    if next <= now_ts {
                        due.push(task.id.clone());
                        task.next_run_at = next_run_after(&task.cron, now_ts);
                    }
                }
            }
            if !due.is_empty() {
                let _ = st.save();
            }
            due
        };
        for id in due {
            match run_task_by_id(&ctx, &id).await {
                Ok(_) => {
                    // Fire webhook if configured (best effort, never fails task).
                    let hook = {
                        let st = ctx.store.lock().await;
                        st.state
                            .tasks
                            .iter()
                            .find(|t| t.id == id)
                            .and_then(|t| t.webhook_url.clone())
                    };
                    if let Some(url) = hook {
                        let _ = reqwest::Client::new()
                            .post(&url)
                            .json(&json!({"event": "routine_completed", "routine_id": id, "ts": now()}))
                            .timeout(std::time::Duration::from_secs(10))
                            .send()
                            .await;
                    }
                }
                Err(e) => {
                    tracing::warn!("scheduled task {id} failed: {e:#}");
                }
            }
        }
    }
}
