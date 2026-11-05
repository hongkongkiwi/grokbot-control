//! Persistent state for GrokBot: config, personas, conversations,
//! prompt templates, memories, documents, scheduled tasks, usage.

use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use uuid::Uuid;

pub const CURRENT_SCHEMA_VERSION: u32 = 2;
pub const MAX_BACKUPS: usize = 20;
pub const MAX_DOCUMENT_CHARS: usize = 200_000;
pub const MAX_MESSAGE_CHARS: usize = 64_000;
pub const MAX_MEMORY_VALUE_CHARS: usize = 64_000;
pub const MAX_ITEMS: usize = 10_000;

fn default_schema_version() -> u32 {
    0
}
fn default_max_history() -> usize {
    40
}
fn default_model_name() -> String {
    "grok-4".to_string()
}
fn default_temperature() -> f64 {
    0.7
}
fn default_top_p() -> f64 {
    1.0
}
fn default_max_tokens() -> u32 {
    2048
}

pub fn now() -> DateTime<Utc> {
    Utc::now()
}
pub fn new_id() -> String {
    Uuid::new_v4().to_string()
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SafetySettings {
    /// off | standard | strict
    #[serde(default = "default_filter_level")]
    pub filter_level: String,
    #[serde(default)]
    pub blocked_topics: Vec<String>,
    #[serde(default)]
    pub max_response_chars: Option<usize>,
}

fn default_filter_level() -> String {
    "standard".to_string()
}

impl Default for SafetySettings {
    fn default() -> Self {
        Self {
            filter_level: "standard".into(),
            blocked_topics: vec![],
            max_response_chars: None,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BotConfig {
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub description: String,
    #[serde(default)]
    pub system_prompt: String,
    #[serde(default = "default_model_name")]
    pub default_model: String,
    #[serde(default = "default_temperature")]
    pub temperature: f64,
    #[serde(default = "default_top_p")]
    pub top_p: f64,
    #[serde(default = "default_max_tokens")]
    pub max_tokens: u32,
    #[serde(default)]
    pub active_persona_id: Option<String>,
    #[serde(default)]
    pub enabled_tools: Vec<String>,
    #[serde(default)]
    pub safety: SafetySettings,
    /// Operator-defined function tools GrokBot itself may call (M3).
    #[serde(default)]
    pub bot_tool_defs: Vec<BotToolDef>,
    /// Conversation history window: keep last N messages (+ summary).
    #[serde(default = "default_max_history")]
    pub max_history_messages: usize,
    /// Optional daily spend cap in USD (checked before backend calls).
    #[serde(default)]
    pub daily_budget_usd: Option<f64>,
    /// Requests-per-minute cap for backend calls (0 = unlimited).
    #[serde(default)]
    pub rate_limit_per_minute: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BotToolDef {
    pub name: String,
    pub description: String,
    /// JSON Schema object for the tool parameters.
    pub parameters: serde_json::Value,
    #[serde(default = "default_true")]
    pub enabled: bool,
}

fn default_true() -> bool {
    true
}

/// A named bot profile (bridge-style multi-bot registry, M10).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BotProfile {
    pub id: String,
    pub name: String,
    pub description: String,
    pub config: BotConfig,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

impl Default for BotConfig {
    fn default() -> Self {
        Self {
            name: "GrokBot".into(),
            description: "A Grok-powered assistant managed over MCP.".into(),
            system_prompt: "You are GrokBot, a helpful AI assistant built on xAI Grok. \
                            Be accurate, concise, and direct."
                .into(),
            default_model: "grok-4".into(),
            temperature: 0.7,
            top_p: 1.0,
            max_tokens: 2048,
            active_persona_id: None,
            enabled_tools: default_bot_tools(),
            safety: SafetySettings::default(),
            bot_tool_defs: vec![],
            max_history_messages: 40,
            daily_budget_usd: None,
            rate_limit_per_minute: 0,
        }
    }
}

/// Tool names GrokBot itself may use at chat time (distinct from MCP tools).
pub fn default_bot_tools() -> Vec<String> {
    [
        "web_search",
        "code_execution",
        "memory",
        "document_search",
        "calculator",
    ]
    .iter()
    .map(|s| s.to_string())
    .collect()
}
pub const ALL_BOT_TOOLS: &[&str] = &[
    "web_search",
    "x_search",
    "code_execution",
    "memory",
    "document_search",
    "calculator",
    "image_understanding",
    "file_upload",
];

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Persona {
    pub id: String,
    pub name: String,
    pub description: String,
    pub system_prompt: String,
    pub model: Option<String>,
    pub temperature: Option<f64>,
    #[serde(default)]
    pub prompt_versions: Vec<PromptVersion>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PromptVersion {
    pub version: u32,
    pub system_prompt: String,
    pub note: String,
    pub ts: DateTime<Utc>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Message {
    #[serde(default = "new_id")]
    pub id: String,
    /// system | user | assistant | tool
    pub role: String,
    pub content: String,
    pub ts: DateTime<Utc>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub edited_at: Option<DateTime<Utc>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tokens: Option<u64>,
}

impl Message {
    pub fn new(role: &str, content: impl Into<String>) -> Self {
        Self {
            id: new_id(),
            edited_at: None,
            role: role.into(),
            content: content.into(),
            ts: now(),
            model: None,
            tokens: None,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Conversation {
    pub id: String,
    pub title: String,
    pub persona_id: Option<String>,
    pub model: String,
    pub messages: Vec<Message>,
    #[serde(default)]
    pub pinned: bool,
    #[serde(default)]
    pub archived: bool,
    /// Window summary covering messages before summary_up_to (M5).
    #[serde(default)]
    pub summary: Option<String>,
    #[serde(default)]
    pub summary_up_to: usize,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PromptTemplate {
    pub id: String,
    pub name: String,
    pub description: String,
    /// Body with `{{variable}}` placeholders.
    pub template: String,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

impl PromptTemplate {
    pub fn variables(&self) -> Vec<String> {
        let mut out = vec![];
        let mut rest = self.template.as_str();
        while let Some(s) = rest.find("{{") {
            let after = &rest[s + 2..];
            match after.find("}}") {
                Some(e) => {
                    let v = after[..e].trim().to_string();
                    if !v.is_empty() && !out.contains(&v) {
                        out.push(v);
                    }
                    rest = &after[e + 2..];
                }
                None => break,
            }
        }
        out
    }
    pub fn render(&self, vars: &BTreeMap<String, String>) -> String {
        let mut out = self.template.clone();
        for v in self.variables() {
            let val = vars.get(&v).cloned().unwrap_or_default();
            out = out.replace(&format!("{{{{{v}}}}}"), &val);
            out = out.replace(&format!("{{{{ {v} }}}}"), &val);
        }
        out
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MemoryEntry {
    pub id: String,
    pub key: String,
    pub value: String,
    pub tags: Vec<String>,
    #[serde(default)]
    pub embedding: Option<Vec<f32>>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Document {
    pub id: String,
    pub title: String,
    pub content: String,
    pub tags: Vec<String>,
    #[serde(default)]
    pub xai_file_id: Option<String>,
    #[serde(default)]
    pub collection_id: Option<String>,
    #[serde(default)]
    pub embedding: Option<Vec<f32>>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ScheduledTask {
    pub id: String,
    pub name: String,
    /// Cron expression (stored; `task_run` executes on demand / when due by caller).
    pub cron: String,
    pub prompt: String,
    pub persona_id: Option<String>,
    pub enabled: bool,
    pub run_count: u64,
    pub last_run_at: Option<DateTime<Utc>>,
    pub last_result: Option<String>,
    #[serde(default)]
    pub next_run_at: Option<DateTime<Utc>>,
    #[serde(default)]
    pub webhook_url: Option<String>,
    pub created_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct UsageStats {
    pub total_requests: u64,
    pub total_prompt_tokens: u64,
    pub total_completion_tokens: u64,
    pub by_model: BTreeMap<String, u64>,
    pub by_tool: BTreeMap<String, u64>,
    #[serde(default)]
    pub by_model_tokens: BTreeMap<String, ModelTokens>,
    #[serde(default)]
    pub total_images: u64,
    #[serde(default)]
    pub spent_today_usd: f64,
    #[serde(default)]
    pub spent_day: String,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ModelTokens {
    pub requests: u64,
    pub prompt_tokens: u64,
    pub completion_tokens: u64,
    pub images: u64,
}

impl UsageStats {
    pub fn total_tokens(&self) -> u64 {
        self.total_prompt_tokens + self.total_completion_tokens
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RequestLogEntry {
    pub ts: DateTime<Utc>,
    pub tool: String,
    pub backend: String,
    pub model: Option<String>,
    pub ok: bool,
    pub detail: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AppState {
    #[serde(default = "default_schema_version")]
    pub schema_version: u32,
    #[serde(default)]
    pub config: BotConfig,
    /// Named bot profiles; the live bot is config above (M10).
    #[serde(default)]
    pub bots: Vec<BotProfile>,
    #[serde(default)]
    pub active_bot_id: Option<String>,
    #[serde(default)]
    pub personas: Vec<Persona>,
    #[serde(default)]
    pub conversations: Vec<Conversation>,
    #[serde(default)]
    pub templates: Vec<PromptTemplate>,
    #[serde(default)]
    pub memories: Vec<MemoryEntry>,
    #[serde(default)]
    pub documents: Vec<Document>,
    #[serde(default)]
    pub tasks: Vec<ScheduledTask>,
    #[serde(default)]
    pub usage: UsageStats,
    #[serde(default)]
    pub log: Vec<RequestLogEntry>,
}

impl Default for AppState {
    fn default() -> Self {
        let t = now();
        let mk = |name: &str, desc: &str, prompt: &str| Persona {
            id: new_id(),
            name: name.into(),
            description: desc.into(),
            system_prompt: prompt.into(),
            model: None,
            temperature: None,
            prompt_versions: vec![PromptVersion {
                version: 1,
                system_prompt: prompt.into(),
                note: "initial".into(),
                ts: t,
            }],
            created_at: t,
            updated_at: t,
        };
        Self {
            schema_version: CURRENT_SCHEMA_VERSION,
            bots: vec![],
            active_bot_id: None,
            config: BotConfig::default(),
            personas: vec![
                mk("Assistant", "General-purpose helpful assistant",
                   "You are GrokBot, a helpful AI assistant. Be accurate and concise."),
                mk("Coder", "Senior software engineer pair-programmer",
                   "You are GrokBot in Coder mode. Write correct, idiomatic, tested code. Explain trade-offs briefly."),
                mk("Researcher", "Deep-research analyst",
                   "You are GrokBot in Researcher mode. Compare sources, cite evidence, flag uncertainty, and summarise findings."),
            ],
            conversations: vec![],
            templates: vec![
                PromptTemplate { id: new_id(), name: "explain".into(),
                    description: "Explain a topic at a given level".into(),
                    template: "Explain {{topic}} for a {{level}} audience. Use one concrete example.".into(),
                    created_at: t, updated_at: t },
                PromptTemplate { id: new_id(), name: "code-review".into(),
                    description: "Review code for bugs and risks".into(),
                    template: "Review this {{language}} code for bugs, security risks, and simplifications:\n\n{{code}}".into(),
                    created_at: t, updated_at: t },
            ],
            memories: vec![], documents: vec![], tasks: vec![],
            usage: UsageStats::default(), log: vec![],
        }
    }
}

impl AppState {
    pub fn find_persona(&self, id_or_name: &str) -> Option<&Persona> {
        self.personas
            .iter()
            .find(|p| p.id == id_or_name || p.name.eq_ignore_ascii_case(id_or_name))
    }
    pub fn find_conversation(&self, id: &str) -> Option<&Conversation> {
        self.conversations.iter().find(|c| c.id == id)
    }
    pub fn find_template(&self, id_or_name: &str) -> Option<&PromptTemplate> {
        self.templates
            .iter()
            .find(|t| t.id == id_or_name || t.name.eq_ignore_ascii_case(id_or_name))
    }
}

/// Thread-safe persistent store. Persists to a JSON file (atomic replace)
/// when a path is set; in-memory only otherwise (tests).
pub struct Store {
    pub state: AppState,
    path: Option<PathBuf>,
    #[allow(dead_code)]
    lock_file: Option<std::fs::File>,
    last_saved_hash: std::sync::Mutex<u64>,
    pub recovered_from: Option<String>,
}

fn hash_str(s: &str) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut h = std::collections::hash_map::DefaultHasher::new();
    s.hash(&mut h);
    h.finish()
}

pub fn backfill_message_ids(state: &mut AppState) {
    for c in &mut state.conversations {
        for m in &mut c.messages {
            if m.id.is_empty() {
                m.id = new_id();
            }
        }
    }
    if state.schema_version == 0 {
        state.schema_version = CURRENT_SCHEMA_VERSION;
    }
    // Seed prompt version 1 for personas that have none.
    for p in &mut state.personas {
        if p.prompt_versions.is_empty() {
            p.prompt_versions.push(PromptVersion {
                version: 1,
                system_prompt: p.system_prompt.clone(),
                note: "initial".into(),
                ts: p.created_at,
            });
        }
    }
}

pub fn atomic_write_pub(path: &Path, raw: &str) -> Result<()> {
    atomic_write(path, raw)
}

fn atomic_write(path: &Path, raw: &str) -> Result<()> {
    if let Some(parent) = path.parent() {
        if !parent.as_os_str().is_empty() {
            std::fs::create_dir_all(parent)?;
        }
    }
    let tmp = path.with_extension(format!("{}.{}.tmp", std::process::id(), new_id()));
    {
        let mut f = std::fs::File::create(&tmp)?;
        use std::io::Write;
        f.write_all(raw.as_bytes())?;
        f.sync_all()?;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o600));
    }
    std::fs::rename(&tmp, path).with_context(|| format!("persist to {}", path.display()))?;
    #[cfg(unix)]
    if let Some(parent) = path.parent() {
        if let Ok(dir) = std::fs::File::open(parent) {
            let _ = dir.sync_all();
        }
    }
    Ok(())
}

/// Newest parseable backup snapshot for a state path, if any.
fn newest_valid_backup(state_path: &Path) -> Option<(PathBuf, AppState)> {
    let dir = state_path.parent()?.join("backups");
    let mut names: Vec<PathBuf> = std::fs::read_dir(&dir)
        .ok()?
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| {
            p.file_name()
                .map(|n| n.to_string_lossy().starts_with("grokbot-backup-"))
                .unwrap_or(false)
        })
        .collect();
    names.sort();
    for cand in names.iter().rev() {
        if let Ok(raw) = std::fs::read_to_string(cand) {
            if let Ok(mut s) = serde_json::from_str::<AppState>(&raw) {
                backfill_message_ids(&mut s);
                return Some((cand.clone(), s));
            }
        }
    }
    None
}

impl Store {
    pub fn load(path: Option<PathBuf>) -> Result<Self> {
        // Single-writer guard: hold an OS lock on a sibling file for life.
        let mut lock_file = None;
        if let Some(p) = &path {
            if let Some(parent) = p.parent() {
                if !parent.as_os_str().is_empty() {
                    let _ = std::fs::create_dir_all(parent);
                }
            }
            let lock_path = p.with_extension("lock");
            if let Ok(f) = std::fs::OpenOptions::new()
                .create(true)
                .write(true)
                .truncate(false)
                .open(&lock_path)
            {
                use fs2::FileExt;
                if f.try_lock_exclusive().is_err() {
                    anyhow::bail!(
                        "state file {} is already in use by another grokbot-control process (lock {}). Stop it first, or use a different --state-file/--profile.",
                        p.display(), lock_path.display()
                    );
                }
                lock_file = Some(f);
            }
        }
        let mut recovered_from = None;
        let mut needs_save = false;
        let state = match &path {
            Some(p) if p.exists() => {
                let raw = std::fs::read_to_string(p)
                    .with_context(|| format!("read state file {}", p.display()))?;
                match serde_json::from_str::<AppState>(&raw) {
                    Ok(mut s) => {
                        backfill_message_ids(&mut s);
                        s
                    }
                    Err(e) => {
                        let stamp = now().format("%Y%m%d-%H%M%S");
                        let corrupt = p.with_extension(format!("corrupt-{stamp}.json"));
                        let _ = std::fs::rename(p, &corrupt);
                        if let Some((bak, s)) = newest_valid_backup(p) {
                            eprintln!("grokbot-control: state {} was corrupt ({e}); quarantined to {} and restored backup {}", p.display(), corrupt.display(), bak.display());
                            recovered_from = Some(bak.display().to_string());
                            needs_save = true;
                            s
                        } else {
                            eprintln!("grokbot-control: state {} was corrupt ({e}); quarantined to {} and started from defaults", p.display(), corrupt.display());
                            recovered_from = Some(corrupt.display().to_string());
                            needs_save = true;
                            AppState::default()
                        }
                    }
                }
            }
            _ => {
                needs_save = path.is_some();
                AppState::default()
            }
        };
        let store = Self {
            state,
            path,
            lock_file,
            last_saved_hash: std::sync::Mutex::new(0),
            recovered_from,
        };
        if needs_save {
            store.save()?;
        }
        Ok(store)
    }

    pub fn in_memory() -> Self {
        Self {
            state: AppState::default(),
            path: None,
            lock_file: None,
            last_saved_hash: std::sync::Mutex::new(0),
            recovered_from: None,
        }
    }

    pub fn save(&self) -> Result<()> {
        let Some(p) = &self.path else { return Ok(()) };
        let raw = serde_json::to_string_pretty(&self.state)?;
        let h = hash_str(&raw);
        {
            let mut last = self.last_saved_hash.lock().unwrap();
            if *last == h {
                return Ok(());
            }
            *last = h;
        }
        atomic_write(p, &raw)
    }

    pub fn path(&self) -> Option<&Path> {
        self.path.as_deref()
    }

    pub fn record(
        &mut self,
        tool: &str,
        backend: &str,
        model: Option<String>,
        ok: bool,
        detail: impl Into<String>,
    ) {
        *self
            .state
            .usage
            .by_tool
            .entry(tool.to_string())
            .or_insert(0) += 1;
        self.state.log.push(RequestLogEntry {
            ts: now(),
            tool: tool.into(),
            backend: backend.into(),
            model,
            ok,
            detail: detail.into(),
        });
        if self.state.log.len() > 500 {
            let drain = self.state.log.len() - 500;
            self.state.log.drain(..drain);
        }
    }

    pub fn record_usage(&mut self, model: &str, prompt_tokens: u64, completion_tokens: u64) {
        self.state.usage.total_requests += 1;
        self.state.usage.total_prompt_tokens += prompt_tokens;
        self.state.usage.total_completion_tokens += completion_tokens;
        *self
            .state
            .usage
            .by_model
            .entry(model.to_string())
            .or_insert(0) += 1;
        let e = self
            .state
            .usage
            .by_model_tokens
            .entry(model.to_string())
            .or_default();
        e.requests += 1;
        e.prompt_tokens += prompt_tokens;
        e.completion_tokens += completion_tokens;
    }

    pub fn record_image(&mut self, model: &str, count: u64) {
        self.state.usage.total_images += count;
        self.state
            .usage
            .by_model_tokens
            .entry(model.to_string())
            .or_default()
            .images += count;
    }
}

pub fn default_state_path() -> PathBuf {
    if let Ok(p) = std::env::var("GROKBOT_STATE_FILE") {
        if !p.is_empty() {
            return PathBuf::from(p);
        }
    }
    if let Some(home) = std::env::var_os("HOME") {
        return PathBuf::from(home).join(".local/share/grokbot-control/state.json");
    }
    PathBuf::from("grokbot-state.json")
}

/// Pre-rename (grokbot-control-mcp) default state path, for migration.
pub fn legacy_default_state_path() -> Option<PathBuf> {
    let home = std::env::var_os("HOME")?;
    Some(PathBuf::from(home).join(".local/share/grokbot-control-mcp/state.json"))
}

/// Copy the legacy state file to the new path if the new one does not
/// exist yet and the old one parses. Copy (not rename) so the old binary
/// keeps working. Returns true when a migration happened.
pub fn migrate_legacy_state(new: &Path, old: &Path) -> Result<bool> {
    if new.exists() || !old.exists() {
        return Ok(false);
    }
    let raw = std::fs::read_to_string(old)
        .with_context(|| format!("read legacy state {}", old.display()))?;
    let _: AppState = serde_json::from_str(&raw).with_context(|| {
        format!(
            "legacy state {} does not parse; not migrating",
            old.display()
        )
    })?;
    if let Some(parent) = new.parent() {
        if !parent.as_os_str().is_empty() {
            std::fs::create_dir_all(parent)?;
        }
    }
    std::fs::copy(old, new)
        .with_context(|| format!("migrate state {} -> {}", old.display(), new.display()))?;
    eprintln!(
        "grokbot-control: migrated state {} -> {} (legacy file kept)",
        old.display(),
        new.display()
    );
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn template_render_and_vars() {
        let s = AppState::default();
        let t = s.templates.iter().find(|t| t.name == "explain").unwrap();
        assert_eq!(
            t.variables(),
            vec!["topic".to_string(), "level".to_string()]
        );
        let mut vars = BTreeMap::new();
        vars.insert("topic".into(), "MCP".into());
        vars.insert("level".into(), "beginner".into());
        assert!(t.render(&vars).contains("Explain MCP for a beginner"));
    }

    #[test]
    fn store_round_trip() {
        let dir = std::env::temp_dir().join(format!("grokbot-test-{}", new_id()));
        let path = dir.join("state.json");
        let mut store = Store::load(Some(path.clone())).unwrap();
        store.state.config.name = "RoundTrip".into();
        store.save().unwrap();
        drop(store); // release the single-writer lock before reloading
        let again = Store::load(Some(path)).unwrap();
        assert_eq!(again.state.config.name, "RoundTrip");
        assert_eq!(again.state.personas.len(), 3);
        let _ = std::fs::remove_dir_all(dir);
    }
}
