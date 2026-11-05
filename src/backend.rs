//! Grok backends: a deterministic offline mock (default) and the live
//! xAI API (`https://api.x.ai/v1`, OpenAI-compatible) via reqwest.
//! Credentials: xAI API key, or an OAuth token set from
//! `grokbot-control login` (sent as Bearer; refreshed lazily).

use crate::auth::{self, TokenSet};
use crate::state::{BotToolDef, Message};
use anyhow::{bail, Context, Result};
use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use std::sync::Arc;
use tokio::sync::Mutex;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModelInfo {
    pub id: String,
    pub description: String,
    pub context_window: Option<u64>,
    pub live: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum SearchMode {
    #[default]
    Off,
    Web,
    X,
}

#[derive(Debug, Clone)]
pub struct ChatRequest {
    pub model: String,
    pub messages: Vec<Message>,
    pub temperature: f64,
    pub top_p: Option<f64>,
    pub max_tokens: u32,
    pub response_format: Option<serde_json::Value>,
    pub reasoning_effort: Option<String>,
    pub search: SearchMode,
    pub bot_tools: Vec<BotToolDef>,
    pub previous_response_id: Option<String>,
}

impl ChatRequest {
    pub fn new(model: String, messages: Vec<Message>, temperature: f64, max_tokens: u32) -> Self {
        Self {
            model,
            messages,
            temperature,
            top_p: None,
            max_tokens,
            response_format: None,
            reasoning_effort: None,
            search: SearchMode::Off,
            bot_tools: vec![],
            previous_response_id: None,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChatReply {
    pub content: String,
    pub model: String,
    pub prompt_tokens: u64,
    pub completion_tokens: u64,
    pub backend: String,
    #[serde(default)]
    pub citations: Vec<String>,
    #[serde(default)]
    pub tool_calls: Option<serde_json::Value>,
    #[serde(default)]
    pub response_id: Option<String>,
    #[serde(default)]
    pub finish_reason: Option<String>,
}

impl ChatReply {
    pub fn simple(content: String, model: String, p: u64, c: u64, backend: &str) -> Self {
        Self {
            content,
            model,
            prompt_tokens: p,
            completion_tokens: c,
            backend: backend.into(),
            citations: vec![],
            tool_calls: None,
            response_id: None,
            finish_reason: None,
        }
    }
}

pub fn est_tokens(text: &str) -> u64 {
    ((text.chars().count() as f64) / 4.0).ceil().max(1.0) as u64
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Pricing {
    Token(f64, f64),
    Image(f64),
    Unknown,
}

/// Approximate pricing. Longest-prefix match wins; unknown models are
/// reported as Unknown (never a made-up rate). Estimates only — xAI
/// rates change; table dated 2026-11.
pub fn pricing_for(model: &str) -> Pricing {
    let m = model;
    if m.contains("image") || m.contains("imagine") {
        return Pricing::Image(0.07);
    }
    if m.starts_with("grok-4-1-fast") || m.starts_with("grok-4-fast") {
        return Pricing::Token(0.20, 0.50);
    }
    if m.starts_with("grok-code") {
        return Pricing::Token(0.20, 1.50);
    }
    if m.starts_with("grok-3-mini") {
        return Pricing::Token(0.30, 0.50);
    }
    if m.starts_with("grok-4") {
        return Pricing::Token(3.00, 15.00);
    }
    if m.starts_with("grok-3") {
        return Pricing::Token(3.00, 15.00);
    }
    if m.contains("vision") {
        return Pricing::Token(2.00, 10.00);
    }
    Pricing::Unknown
}

pub fn pricing(model: &str) -> (f64, f64) {
    match pricing_for(model) {
        Pricing::Token(p, c) => (p, c),
        _ => (0.0, 0.0),
    }
}

pub fn est_cost_usd(model: &str, prompt_tokens: u64, completion_tokens: u64) -> f64 {
    match pricing_for(model) {
        Pricing::Token(p, c) => {
            (prompt_tokens as f64 * p + completion_tokens as f64 * c) / 1_000_000.0
        }
        _ => 0.0,
    }
}

pub fn image_cost_usd(model: &str, count: u64) -> f64 {
    match pricing_for(model) {
        Pricing::Image(rate) => rate * count as f64,
        _ => 0.0,
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ImageResult {
    pub prompt: String,
    pub model: String,
    pub url: Option<String>,
    pub b64_json: Option<String>,
    pub revised_prompt: Option<String>,
    pub backend: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FileInfo {
    pub id: String,
    pub filename: String,
    pub bytes: Option<u64>,
    pub backend: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VideoResult {
    pub prompt: String,
    pub model: String,
    pub url: Option<String>,
    pub backend: String,
}

/// How the live backend proves itself to xAI.
#[derive(Clone)]
pub enum Credential {
    ApiKey(String),
    OAuth(Arc<Mutex<TokenSet>>),
}

impl Credential {
    pub fn kind(&self) -> &'static str {
        match self {
            Credential::ApiKey(_) => "api_key",
            Credential::OAuth(_) => "oauth",
        }
    }
    pub async fn bearer(&self) -> Result<String> {
        match self {
            Credential::ApiKey(k) => Ok(k.clone()),
            Credential::OAuth(cell) => {
                let mut tokens = cell.lock().await;
                if tokens.expired() {
                    let next = auth::refresh(&tokens).await?;
                    *tokens = next;
                    let snapshot = tokens.clone();
                    tokio::spawn(async move {
                        let _ = auth::save_auth(&snapshot);
                    });
                }
                Ok(tokens.access_token.clone())
            }
        }
    }
}

pub fn validate_base_url(raw: &str) -> Result<String> {
    let trimmed = raw.trim().trim_end_matches('/');
    if trimmed.is_empty() {
        bail!("XAI_BASE_URL is empty");
    }
    let url = reqwest::Url::parse(trimmed)
        .with_context(|| format!("XAI_BASE_URL '{raw}' is not a valid URL"))?;
    let host = url.host_str().unwrap_or_default();
    let local = matches!(host, "127.0.0.1" | "localhost" | "::1");
    match url.scheme() {
        "https" => {}
        "http" if local => {}
        other => bail!(
            "XAI_BASE_URL must be https (http allowed only for localhost); got {other}://{host} — refusing to send credentials there"
        ),
    }
    if !url.username().is_empty() || url.password().is_some() {
        bail!("XAI_BASE_URL must not embed credentials");
    }
    Ok(trimmed.to_string())
}

#[async_trait]
pub trait GrokBackend: Send + Sync {
    fn name(&self) -> &str;
    fn is_live(&self) -> bool;
    /// (kind, human detail) e.g. ("api_key", "XAI_API_KEY") / ("oauth", issuer).
    fn credential_info(&self) -> (String, String) {
        (self.name().to_string(), String::new())
    }
    async fn chat(&self, req: ChatRequest) -> Result<ChatReply>;
    /// Streaming chat: calls `sink` with each content delta, returns the
    /// assembled reply. Default: one delta with the full reply.
    async fn chat_stream(
        &self,
        req: ChatRequest,
        sink: &mut (dyn FnMut(String) + Send),
    ) -> Result<ChatReply> {
        let reply = self.chat(req).await?;
        if !reply.content.is_empty() {
            sink(reply.content.clone());
        }
        Ok(reply)
    }
    async fn list_models(&self) -> Result<Vec<ModelInfo>>;
    async fn image_generate(&self, prompt: &str, model: &str, n: u32) -> Result<Vec<ImageResult>>;
    async fn vision_chat(
        &self,
        prompt: &str,
        image_url: &str,
        model: &str,
        max_tokens: u32,
    ) -> Result<ChatReply>;
    async fn video_generate(&self, _prompt: &str, _model: &str) -> Result<VideoResult> {
        bail!(
            "video generation is not supported by the {} backend",
            self.name()
        )
    }
    async fn file_upload(&self, _filename: &str, _bytes: Vec<u8>) -> Result<FileInfo> {
        bail!(
            "file upload is not supported by the {} backend",
            self.name()
        )
    }
    async fn file_list(&self) -> Result<Vec<FileInfo>> {
        bail!("file list is not supported by the {} backend", self.name())
    }
    async fn file_delete(&self, _file_id: &str) -> Result<()> {
        bail!(
            "file delete is not supported by the {} backend",
            self.name()
        )
    }
    async fn embed(&self, _texts: &[String], _model: &str) -> Result<Vec<Vec<f32>>> {
        bail!(
            "embeddings are not supported by the {} backend",
            self.name()
        )
    }
    async fn audio_transcribe(
        &self,
        _audio_b64: &str,
        _format: &str,
        _model: &str,
    ) -> Result<String> {
        bail!(
            "audio transcription is not supported by the {} backend",
            self.name()
        )
    }
    async fn audio_speak(&self, _text: &str, _voice: &str, _model: &str) -> Result<String> {
        bail!(
            "audio speech is not supported by the {} backend",
            self.name()
        )
    }
}

/// Deterministic offline backend. Never touches the network.
pub struct MockBackend;

fn mock_embedding(text: &str) -> Vec<f32> {
    // Deterministic 32-dim pseudo-embedding from byte hashes.
    let mut v = [0f32; 32];
    for (i, b) in text.bytes().enumerate() {
        v[i % 32] += (b as f32) / 255.0 + ((i * 7 % 13) as f32) / 97.0;
    }
    let norm = v.iter().map(|x| x * x).sum::<f32>().sqrt().max(1e-6);
    v.iter().map(|x| x / norm).collect()
}

#[async_trait]
impl GrokBackend for MockBackend {
    fn name(&self) -> &str {
        "mock"
    }
    fn is_live(&self) -> bool {
        false
    }
    fn credential_info(&self) -> (String, String) {
        ("mock".into(), "offline mock, no credential".into())
    }

    async fn chat(&self, req: ChatRequest) -> Result<ChatReply> {
        let last_user = req
            .messages
            .iter()
            .rev()
            .find(|m| m.role == "user")
            .map(|m| m.content.clone())
            .unwrap_or_default();
        let system = req
            .messages
            .iter()
            .find(|m| m.role == "system")
            .map(|m| m.content.clone());
        let prompt_tokens: u64 = req.messages.iter().map(|m| est_tokens(&m.content)).sum();
        let sys_note = match &system {
            Some(s) => format!(" (system: {}…)", s.chars().take(60).collect::<String>()),
            None => String::new(),
        };
        let search_note = match req.search {
            SearchMode::Off => String::new(),
            SearchMode::Web => " [web search requested: mock returns no live results]".into(),
            SearchMode::X => " [x search requested: mock returns no live results]".into(),
        };
        let mut content = format!(
            "[mock grok · {model}] I received: \"{q}\"{sys_note}{search_note}\n\n\
             This is the offline mock backend — no xAI call was made. \
             Log in (`grokbot-control login`) or set XAI_API_KEY for real Grok replies. \
             Your message had {n} message(s) in context.",
            model = req.model,
            q = last_user.chars().take(300).collect::<String>(),
            n = req.messages.len(),
        );
        if req.response_format.is_some() {
            content = serde_json::json!({"mock": true, "reply": content}).to_string();
        }
        let completion_tokens = est_tokens(&content);
        Ok(ChatReply::simple(
            content,
            req.model.clone(),
            prompt_tokens,
            completion_tokens,
            "mock",
        ))
    }

    async fn list_models(&self) -> Result<Vec<ModelInfo>> {
        Ok(mock_models())
    }

    async fn image_generate(&self, prompt: &str, model: &str, n: u32) -> Result<Vec<ImageResult>> {
        Ok((0..n.max(1))
            .map(|i| ImageResult {
                prompt: prompt.to_string(),
                model: model.to_string(),
                url: Some(format!("mock://grokbot-image/{i}")),
                b64_json: None,
                revised_prompt: Some(format!("{prompt} (mock revision {i})")),
                backend: "mock".into(),
            })
            .collect())
    }

    async fn vision_chat(
        &self,
        prompt: &str,
        image_url: &str,
        model: &str,
        _max_tokens: u32,
    ) -> Result<ChatReply> {
        let content = format!(
            "[mock grok vision · {model}] I would inspect the image at {image_url} and answer: \"{prompt}\". \
             Mock backend — log in or set XAI_API_KEY for real vision analysis."
        );
        Ok(ChatReply::simple(
            content.clone(),
            model.to_string(),
            est_tokens(prompt) + 85,
            est_tokens(&content),
            "mock",
        ))
    }

    async fn video_generate(&self, prompt: &str, model: &str) -> Result<VideoResult> {
        Ok(VideoResult {
            prompt: prompt.to_string(),
            model: model.to_string(),
            url: Some("mock://grokbot-video/0".into()),
            backend: "mock".into(),
        })
    }

    async fn file_upload(&self, filename: &str, bytes: Vec<u8>) -> Result<FileInfo> {
        Ok(FileInfo {
            id: format!("mock-file-{}", crate::state::new_id()),
            filename: filename.to_string(),
            bytes: Some(bytes.len() as u64),
            backend: "mock".into(),
        })
    }

    async fn file_list(&self) -> Result<Vec<FileInfo>> {
        Ok(vec![])
    }

    async fn file_delete(&self, _file_id: &str) -> Result<()> {
        Ok(())
    }

    async fn embed(&self, texts: &[String], _model: &str) -> Result<Vec<Vec<f32>>> {
        Ok(texts.iter().map(|t| mock_embedding(t)).collect())
    }

    async fn audio_transcribe(
        &self,
        _audio_b64: &str,
        _format: &str,
        _model: &str,
    ) -> Result<String> {
        Ok("[mock transcription] Mock backend received audio but cannot transcribe. Log in or set XAI_API_KEY for live transcription.".into())
    }

    async fn audio_speak(&self, text: &str, _voice: &str, _model: &str) -> Result<String> {
        // 0.1s of silent 8kHz mono WAV, base64 — a real, playable placeholder.
        let mut wav: Vec<u8> = Vec::new();
        let samples: u32 = 800;
        wav.extend_from_slice(b"RIFF");
        wav.extend_from_slice(&(36 + samples * 2).to_le_bytes());
        wav.extend_from_slice(b"WAVEfmt ");
        wav.extend_from_slice(&16u32.to_le_bytes());
        wav.extend_from_slice(&1u16.to_le_bytes());
        wav.extend_from_slice(&1u16.to_le_bytes());
        wav.extend_from_slice(&8000u32.to_le_bytes());
        wav.extend_from_slice(&16000u32.to_le_bytes());
        wav.extend_from_slice(&2u16.to_le_bytes());
        wav.extend_from_slice(&16u16.to_le_bytes());
        wav.extend_from_slice(b"data");
        wav.extend_from_slice(&(samples * 2).to_le_bytes());
        wav.resize(wav.len() + (samples * 2) as usize, 0);
        let _ = text;
        use base64::Engine;
        Ok(base64::engine::general_purpose::STANDARD.encode(wav))
    }
}

pub fn mock_models() -> Vec<ModelInfo> {
    [
        ("grok-4", "Flagship Grok reasoning model", 256_000),
        (
            "grok-4-1-fast-reasoning",
            "Fast reasoning variant",
            2_000_000,
        ),
        (
            "grok-4-fast-reasoning",
            "Fast reasoning variant (earlier)",
            2_000_000,
        ),
        (
            "grok-4-fast-non-reasoning",
            "Fast non-reasoning variant",
            2_000_000,
        ),
        ("grok-3", "Grok 3 general model", 131_072),
        ("grok-3-mini", "Small, fast, cheap Grok 3", 131_072),
        ("grok-code-fast-1", "Optimised for code", 256_000),
        ("grok-2-vision-1212", "Vision-capable Grok 2", 32_768),
        ("grok-2-image-1212", "Image generation model", 0),
        ("grok-imagine-video", "Video generation model", 0),
    ]
    .iter()
    .map(|(id, d, cw)| ModelInfo {
        id: id.to_string(),
        description: d.to_string(),
        context_window: Some(*cw),
        live: false,
    })
    .collect()
}

/// Live xAI API backend. Credential is an API key or OAuth token set.
pub struct XaiBackend {
    credential: Credential,
    base_url: String,
    client: reqwest::Client,
}

impl XaiBackend {
    pub fn new(credential: Credential, base_url: Option<String>) -> Result<Self> {
        let raw = base_url
            .or_else(|| std::env::var("XAI_BASE_URL").ok().filter(|s| !s.is_empty()))
            .unwrap_or_else(|| "https://api.x.ai/v1".to_string());
        let base_url = validate_base_url(&raw)?;
        let client = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(120))
            .build()?;
        Ok(Self {
            credential,
            base_url,
            client,
        })
    }

    async fn send_with_retry(
        &self,
        build: impl Fn() -> reqwest::RequestBuilder,
    ) -> Result<reqwest::Response> {
        let mut attempt = 0;
        loop {
            attempt += 1;
            match build().send().await {
                Ok(resp) => {
                    let status = resp.status();
                    if (status.as_u16() == 429 || status.is_server_error()) && attempt < 3 {
                        let wait = resp
                            .headers()
                            .get("retry-after")
                            .and_then(|v| v.to_str().ok())
                            .and_then(|v| v.parse::<u64>().ok())
                            .unwrap_or(1)
                            .min(5);
                        tokio::time::sleep(std::time::Duration::from_secs(wait)).await;
                        continue;
                    }
                    return Ok(resp);
                }
                Err(e) if attempt < 3 => {
                    tokio::time::sleep(std::time::Duration::from_millis(300 * attempt as u64))
                        .await;
                    let _ = e;
                    continue;
                }
                Err(e) => return Err(e).context("xAI request failed after retries"),
            }
        }
    }

    async fn ok_json(&self, resp: reqwest::Response, what: &str) -> Result<serde_json::Value> {
        let status = resp.status();
        let text = resp.text().await.unwrap_or_default();
        if !status.is_success() {
            if status.as_u16() == 401 {
                bail!(
                    "xAI returned 401 for {what}. Your {} credential was rejected. \
                     If you logged in with OAuth, note api.x.ai officially expects an API key — \
                     set XAI_API_KEY, or re-run `grokbot-control login`. Detail: {}",
                    self.credential.kind(),
                    text.chars().take(200).collect::<String>()
                );
            }
            bail!(
                "xAI API returned {status} for {what}: {}",
                text.chars().take(300).collect::<String>()
            );
        }
        serde_json::from_str(&text).with_context(|| format!("parse xAI {what} response"))
    }
}

fn chat_body(req: &ChatRequest, stream: bool) -> serde_json::Value {
    let mut body = serde_json::json!({
        "model": req.model,
        "temperature": req.temperature,
        "max_tokens": req.max_tokens,
        "messages": req.messages.iter().map(|m| serde_json::json!({
            "role": m.role, "content": m.content
        })).collect::<Vec<_>>(),
    });
    if let Some(tp) = req.top_p {
        body["top_p"] = serde_json::json!(tp);
    }
    if let Some(rf) = &req.response_format {
        body["response_format"] = rf.clone();
    }
    if let Some(re) = &req.reasoning_effort {
        body["reasoning_effort"] = serde_json::json!(re);
    }
    if let Some(prev) = &req.previous_response_id {
        body["previous_response_id"] = serde_json::json!(prev);
    }
    match req.search {
        SearchMode::Web => {
            body["tools"] = serde_json::json!([{"type": "web_search"}]);
        }
        SearchMode::X => {
            body["tools"] = serde_json::json!([{"type": "x_search"}]);
        }
        SearchMode::Off => {}
    }
    if !req.bot_tools.is_empty() {
        let mut tools: Vec<serde_json::Value> = req
            .bot_tools
            .iter()
            .filter(|t| t.enabled)
            .map(|t| {
                serde_json::json!({
                    "type": "function",
                    "function": {"name": t.name, "description": t.description, "parameters": t.parameters}
                })
            })
            .collect();
        if let Some(existing) = body["tools"].as_array() {
            tools.extend(existing.iter().cloned());
        }
        if !tools.is_empty() {
            body["tools"] = serde_json::Value::Array(tools);
        }
    }
    if stream {
        body["stream"] = serde_json::json!(true);
        body["stream_options"] = serde_json::json!({"include_usage": true});
    }
    body
}

fn parse_chat_value(v: &serde_json::Value, fallback_model: &str) -> Result<ChatReply> {
    let choices = v["choices"].as_array();
    let Some(first) = choices.and_then(|c| c.first()) else {
        bail!(
            "xAI chat response had no choices (id={:?}). Full shape keys: {:?}",
            v["id"],
            v.as_object().map(|o| o.keys().collect::<Vec<_>>())
        );
    };
    let content = first["message"]["content"]
        .as_str()
        .unwrap_or_default()
        .to_string();
    let tool_calls = if first["message"]["tool_calls"].is_null() {
        None
    } else {
        Some(first["message"]["tool_calls"].clone())
    };
    if content.is_empty() && tool_calls.is_none() {
        bail!(
            "xAI chat returned empty content (finish_reason={:?}, refusal={:?}) — not treating this as a successful reply",
            first["finish_reason"],
            first["message"]["refusal"]
        );
    }
    let mut citations = vec![];
    if let Some(arr) = v["citations"].as_array() {
        citations = arr
            .iter()
            .filter_map(|c| c.as_str().map(|s| s.to_string()))
            .collect();
    }
    Ok(ChatReply {
        content,
        model: v["model"].as_str().unwrap_or(fallback_model).to_string(),
        prompt_tokens: v["usage"]["prompt_tokens"].as_u64().unwrap_or(0),
        completion_tokens: v["usage"]["completion_tokens"].as_u64().unwrap_or(0),
        backend: "xai-live".into(),
        citations,
        tool_calls,
        response_id: v["id"].as_str().map(|s| s.to_string()),
        finish_reason: first["finish_reason"].as_str().map(|s| s.to_string()),
    })
}

#[async_trait]
impl GrokBackend for XaiBackend {
    fn name(&self) -> &str {
        "xai-live"
    }
    fn is_live(&self) -> bool {
        true
    }
    fn credential_info(&self) -> (String, String) {
        match &self.credential {
            Credential::ApiKey(_) => ("api_key".into(), "XAI_API_KEY".into()),
            Credential::OAuth(cell) => {
                let issuer = cell
                    .try_lock()
                    .ok()
                    .and_then(|t| t.issuer.clone())
                    .unwrap_or_default();
                ("oauth".into(), format!("login token from {issuer}"))
            }
        }
    }

    async fn chat(&self, req: ChatRequest) -> Result<ChatReply> {
        let body = chat_body(&req, false);
        let token = self.credential.bearer().await?;
        let url = format!("{}/chat/completions", self.base_url);
        let resp = self
            .send_with_retry(|| self.client.post(&url).bearer_auth(&token).json(&body))
            .await?;
        let v = self.ok_json(resp, "chat/completions").await?;
        parse_chat_value(&v, &req.model)
    }

    async fn chat_stream(
        &self,
        req: ChatRequest,
        sink: &mut (dyn FnMut(String) + Send),
    ) -> Result<ChatReply> {
        use tokio_stream::StreamExt;
        let body = chat_body(&req, true);
        let token = self.credential.bearer().await?;
        let resp = self
            .client
            .post(format!("{}/chat/completions", self.base_url))
            .bearer_auth(&token)
            .json(&body)
            .send()
            .await
            .context("xAI streaming chat request failed")?;
        let status = resp.status();
        if !status.is_success() {
            let text = resp.text().await.unwrap_or_default();
            bail!(
                "xAI API returned {status} for streaming chat: {}",
                text.chars().take(300).collect::<String>()
            );
        }
        let mut stream = resp.bytes_stream();
        let mut buffer = String::new();
        let mut content = String::new();
        let mut model = req.model.clone();
        let (mut pt, mut ct) = (0u64, 0u64);
        let mut response_id = None;
        while let Some(chunk) = stream.next().await {
            let bytes = chunk.context("xAI stream chunk failed")?;
            buffer.push_str(&String::from_utf8_lossy(&bytes));
            while let Some(nl) = buffer.find('\n') {
                let line = buffer[..nl].trim().to_string();
                buffer.drain(..=nl);
                let Some(data) = line.strip_prefix("data:") else {
                    continue;
                };
                let data = data.trim();
                if data == "[DONE]" || data.is_empty() {
                    continue;
                }
                if let Ok(v) = serde_json::from_str::<serde_json::Value>(data) {
                    if let Some(m) = v["model"].as_str() {
                        model = m.to_string();
                    }
                    response_id = v["id"]
                        .as_str()
                        .map(|s| s.to_string())
                        .or(response_id.take());
                    if let Some(delta) = v["choices"][0]["delta"]["content"].as_str() {
                        if !delta.is_empty() {
                            content.push_str(delta);
                            sink(delta.to_string());
                        }
                    }
                    if let Some(u) = v.get("usage").filter(|u| !u.is_null()) {
                        pt = u["prompt_tokens"].as_u64().unwrap_or(pt);
                        ct = u["completion_tokens"].as_u64().unwrap_or(ct);
                    }
                }
            }
        }
        if content.is_empty() {
            bail!("xAI streaming chat produced no content");
        }
        if pt == 0 {
            pt = req.messages.iter().map(|m| est_tokens(&m.content)).sum();
        }
        if ct == 0 {
            ct = est_tokens(&content);
        }
        Ok(ChatReply {
            content,
            model,
            prompt_tokens: pt,
            completion_tokens: ct,
            backend: "xai-live".into(),
            citations: vec![],
            tool_calls: None,
            response_id,
            finish_reason: Some("stop".into()),
        })
    }

    async fn list_models(&self) -> Result<Vec<ModelInfo>> {
        let token = self.credential.bearer().await?;
        let url = format!("{}/models", self.base_url);
        let resp = self
            .send_with_retry(|| self.client.get(&url).bearer_auth(&token))
            .await?;
        let v = self.ok_json(resp, "models").await?;
        let mut out = vec![];
        if let Some(arr) = v["data"].as_array() {
            for m in arr {
                if let Some(id) = m["id"].as_str() {
                    out.push(ModelInfo {
                        id: id.to_string(),
                        description: format!("xAI model {id}"),
                        context_window: None,
                        live: true,
                    });
                }
            }
        }
        if out.is_empty() {
            bail!("xAI /models returned no models — the credential may lack model access. Not substituting the mock catalogue.");
        }
        Ok(out)
    }

    async fn image_generate(&self, prompt: &str, model: &str, n: u32) -> Result<Vec<ImageResult>> {
        let body = serde_json::json!({
            "model": model, "prompt": prompt, "n": n.max(1), "response_format": "url",
        });
        let token = self.credential.bearer().await?;
        let url = format!("{}/images/generations", self.base_url);
        let resp = self
            .send_with_retry(|| self.client.post(&url).bearer_auth(&token).json(&body))
            .await?;
        let v = self.ok_json(resp, "images/generations").await?;
        let mut out = vec![];
        if let Some(arr) = v["data"].as_array() {
            for item in arr {
                out.push(ImageResult {
                    prompt: prompt.to_string(),
                    model: model.to_string(),
                    url: item["url"].as_str().map(|s| s.to_string()),
                    b64_json: item["b64_json"].as_str().map(|s| s.to_string()),
                    revised_prompt: item["revised_prompt"].as_str().map(|s| s.to_string()),
                    backend: "xai-live".into(),
                });
            }
        }
        if out.is_empty() {
            bail!("xAI image response contained no images");
        }
        Ok(out)
    }

    async fn vision_chat(
        &self,
        prompt: &str,
        image_url: &str,
        model: &str,
        max_tokens: u32,
    ) -> Result<ChatReply> {
        let body = serde_json::json!({
            "model": model, "max_tokens": max_tokens,
            "messages": [{
                "role": "user",
                "content": [
                    {"type": "text", "text": prompt},
                    {"type": "image_url", "image_url": {"url": image_url, "detail": "high"}}
                ]
            }],
        });
        let token = self.credential.bearer().await?;
        let url = format!("{}/chat/completions", self.base_url);
        let resp = self
            .send_with_retry(|| self.client.post(&url).bearer_auth(&token).json(&body))
            .await?;
        let v = self.ok_json(resp, "vision chat").await?;
        parse_chat_value(&v, model)
    }

    async fn video_generate(&self, prompt: &str, model: &str) -> Result<VideoResult> {
        let body = serde_json::json!({"model": model, "prompt": prompt});
        let token = self.credential.bearer().await?;
        let url = format!("{}/videos/generations", self.base_url);
        let resp = self
            .send_with_retry(|| self.client.post(&url).bearer_auth(&token).json(&body))
            .await?;
        let v = self.ok_json(resp, "videos/generations").await?;
        let url_out = v["data"][0]["url"].as_str().or_else(|| v["url"].as_str());
        Ok(VideoResult {
            prompt: prompt.to_string(),
            model: model.to_string(),
            url: url_out.map(|s| s.to_string()),
            backend: "xai-live".into(),
        })
    }

    async fn file_upload(&self, filename: &str, bytes: Vec<u8>) -> Result<FileInfo> {
        let token = self.credential.bearer().await?;
        let part = reqwest::multipart::Part::bytes(bytes.clone()).file_name(filename.to_string());
        let form = reqwest::multipart::Form::new()
            .text("purpose", "assistants")
            .part("file", part);
        let resp = self
            .client
            .post(format!("{}/files", self.base_url))
            .bearer_auth(&token)
            .multipart(form)
            .send()
            .await
            .context("xAI file upload failed")?;
        let v = self.ok_json(resp, "files upload").await?;
        Ok(FileInfo {
            id: v["id"].as_str().unwrap_or_default().to_string(),
            filename: v["filename"].as_str().unwrap_or(filename).to_string(),
            bytes: v["bytes"].as_u64().or(Some(bytes.len() as u64)),
            backend: "xai-live".into(),
        })
    }

    async fn file_list(&self) -> Result<Vec<FileInfo>> {
        let token = self.credential.bearer().await?;
        let url = format!("{}/files", self.base_url);
        let resp = self
            .send_with_retry(|| self.client.get(&url).bearer_auth(&token))
            .await?;
        let v = self.ok_json(resp, "files list").await?;
        let mut out = vec![];
        if let Some(arr) = v["data"].as_array() {
            for f in arr {
                out.push(FileInfo {
                    id: f["id"].as_str().unwrap_or_default().to_string(),
                    filename: f["filename"].as_str().unwrap_or_default().to_string(),
                    bytes: f["bytes"].as_u64(),
                    backend: "xai-live".into(),
                });
            }
        }
        Ok(out)
    }

    async fn file_delete(&self, file_id: &str) -> Result<()> {
        let token = self.credential.bearer().await?;
        let resp = self
            .client
            .delete(format!("{}/files/{}", self.base_url, file_id))
            .bearer_auth(&token)
            .send()
            .await
            .context("xAI file delete failed")?;
        let _ = self
            .ok_json(resp, "files delete")
            .await
            .or_else(|_| -> Result<serde_json::Value> { Ok(serde_json::json!({})) })?;
        Ok(())
    }

    async fn embed(&self, texts: &[String], model: &str) -> Result<Vec<Vec<f32>>> {
        let body = serde_json::json!({"model": model, "input": texts});
        let token = self.credential.bearer().await?;
        let url = format!("{}/embeddings", self.base_url);
        let resp = self
            .send_with_retry(|| self.client.post(&url).bearer_auth(&token).json(&body))
            .await?;
        let v = self.ok_json(resp, "embeddings").await?;
        let mut items: Vec<(usize, Vec<f32>)> = vec![];
        if let Some(arr) = v["data"].as_array() {
            for item in arr {
                let idx = item["index"].as_u64().unwrap_or(0) as usize;
                let emb: Vec<f32> = item["embedding"]
                    .as_array()
                    .map(|a| {
                        a.iter()
                            .filter_map(|x| x.as_f64().map(|f| f as f32))
                            .collect()
                    })
                    .unwrap_or_default();
                items.push((idx, emb));
            }
        }
        items.sort_by_key(|(i, _)| *i);
        if items.len() != texts.len() {
            bail!(
                "xAI embeddings returned {} vectors for {} inputs",
                items.len(),
                texts.len()
            );
        }
        Ok(items.into_iter().map(|(_, e)| e).collect())
    }

    async fn audio_transcribe(&self, audio_b64: &str, format: &str, model: &str) -> Result<String> {
        use base64::Engine;
        let bytes = base64::engine::general_purpose::STANDARD
            .decode(audio_b64)
            .context("audio data is not valid base64")?;
        let token = self.credential.bearer().await?;
        let part = reqwest::multipart::Part::bytes(bytes).file_name(format!("audio.{format}"));
        let form = reqwest::multipart::Form::new()
            .text("model", model.to_string())
            .part("file", part);
        let resp = self
            .client
            .post(format!("{}/audio/transcriptions", self.base_url))
            .bearer_auth(&token)
            .multipart(form)
            .send()
            .await
            .context("xAI audio transcription failed")?;
        let v = self.ok_json(resp, "audio/transcriptions").await?;
        Ok(v["text"].as_str().unwrap_or_default().to_string())
    }

    async fn audio_speak(&self, text: &str, voice: &str, model: &str) -> Result<String> {
        let body = serde_json::json!({
            "model": model, "input": text, "voice": voice, "response_format": "mp3",
        });
        let token = self.credential.bearer().await?;
        let resp = self
            .send_with_retry(|| {
                self.client
                    .post(format!("{}/audio/speech", self.base_url))
                    .bearer_auth(&token)
                    .json(&body)
            })
            .await?;
        let status = resp.status();
        if !status.is_success() {
            let t = resp.text().await.unwrap_or_default();
            bail!(
                "xAI audio/speech returned {status}: {}",
                t.chars().take(200).collect::<String>()
            );
        }
        let bytes = resp.bytes().await.context("read speech audio")?;
        use base64::Engine;
        Ok(base64::engine::general_purpose::STANDARD.encode(bytes))
    }
}

/// Pick a backend: `mock` | `live` | `auto`.
/// Credential precedence (auto): `--backend-auth` / GROKBOT_CREDENTIAL
/// (`oauth` | `api_key`) wins; otherwise OAuth login token, then
/// XAI_API_KEY, then mock.
pub fn select_backend(mode: &str) -> Result<Arc<dyn GrokBackend>> {
    select_backend_with(mode, None)
}

pub fn select_backend_with(
    mode: &str,
    credential_pref: Option<&str>,
) -> Result<Arc<dyn GrokBackend>> {
    let pref = credential_pref
        .map(|s| s.to_string())
        .or_else(|| std::env::var("GROKBOT_CREDENTIAL").ok())
        .unwrap_or_else(|| "auto".to_string());
    let make_credential = |allow_mock_fallback: bool| -> Result<Option<Credential>> {
        let key = std::env::var("XAI_API_KEY").unwrap_or_default();
        let oauth = auth::load_auth().unwrap_or(None);
        match pref.as_str() {
            "oauth" => match oauth {
                Some(t) => Ok(Some(Credential::OAuth(Arc::new(Mutex::new(t))))),
                None => bail!(
                    "credential=oauth but no login is stored — run `grokbot-control login` first"
                ),
            },
            "api_key" | "api-key" => {
                if key.trim().is_empty() {
                    bail!("credential=api_key but XAI_API_KEY is not set");
                }
                Ok(Some(Credential::ApiKey(key)))
            }
            _ => {
                if let Some(t) = oauth {
                    return Ok(Some(Credential::OAuth(Arc::new(Mutex::new(t)))));
                }
                if !key.trim().is_empty() {
                    return Ok(Some(Credential::ApiKey(key)));
                }
                if allow_mock_fallback {
                    return Ok(None);
                }
                bail!("no credential: run `grokbot-control login` or set XAI_API_KEY")
            }
        }
    };
    match mode {
        "mock" => Ok(Arc::new(MockBackend)),
        "live" => {
            let cred = make_credential(false)?.expect("credential checked");
            Ok(Arc::new(XaiBackend::new(cred, None)?))
        }
        "auto" => match make_credential(true)? {
            Some(cred) => Ok(Arc::new(XaiBackend::new(cred, None)?)),
            None => Ok(Arc::new(MockBackend)),
        },
        other => bail!("unknown backend mode '{other}' (expected mock|live|auto)"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn mock_chat_is_deterministic_shape() {
        let b = MockBackend;
        let r = b
            .chat(ChatRequest::new(
                "grok-4".into(),
                vec![Message::new("user", "hello grok")],
                0.7,
                100,
            ))
            .await
            .unwrap();
        assert!(r.content.contains("hello grok"));
        assert_eq!(r.backend, "mock");
        assert!(r.prompt_tokens >= 1 && r.completion_tokens >= 1);
        let models = b.list_models().await.unwrap();
        assert!(models.iter().any(|m| m.id == "grok-4"));
    }

    #[test]
    fn pricing_longest_prefix_and_unknown() {
        assert_eq!(pricing_for("grok-code-fast-1"), Pricing::Token(0.20, 1.50));
        assert_eq!(
            pricing_for("grok-4-1-fast-reasoning"),
            Pricing::Token(0.20, 0.50)
        );
        assert_eq!(pricing_for("grok-4"), Pricing::Token(3.00, 15.00));
        assert_eq!(pricing_for("grok-2-image-1212"), Pricing::Image(0.07));
        assert_eq!(pricing_for("mystery-model"), Pricing::Unknown);
        assert!(validate_base_url("http://attacker.example/v1").is_err());
        assert!(validate_base_url("https://api.x.ai/v1").is_ok());
        assert!(validate_base_url("http://127.0.0.1:9999/v1").is_ok());
        assert!(validate_base_url("").is_err());
    }
}
