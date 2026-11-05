//! OAuth login for GrokBot Control.
//!
//! Two distinct things live here:
//! 1. **User login** (`login` CLI): OAuth 2.0 authorization-code + PKCE
//!    (browser, localhost callback) or device flow against a configurable
//!    issuer (xAI/Grok accounts by default via discovery). Tokens are
//!    stored at `~/.config/grokbot-control/auth.json` with 0600 perms —
//!    never in bot state (state_export must never leak them).
//! 2. **Built-in MCP OAuth server** (server.rs uses `OAuthAs` below):
//!    discovery + authorize/token endpoints so MCP clients can log in
//!    to *this* server with scopes (read/chat/admin).

use anyhow::{anyhow, bail, Context, Result};
use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::path::PathBuf;

pub fn b64url(data: &[u8]) -> String {
    URL_SAFE_NO_PAD.encode(data)
}

pub fn sha256_hex(data: &[u8]) -> String {
    Sha256::digest(data)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

pub fn pkce_pair() -> (String, String) {
    let mut bytes = [0u8; 32];
    rand::RngCore::fill_bytes(&mut rand::thread_rng(), &mut bytes);
    let verifier = b64url(&bytes);
    let challenge = b64url(&Sha256::digest(verifier.as_bytes()));
    (verifier, challenge)
}

pub fn random_token(bytes: usize) -> String {
    let mut buf = vec![0u8; bytes];
    rand::RngCore::fill_bytes(&mut rand::thread_rng(), &mut buf);
    b64url(&buf)
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TokenSet {
    pub access_token: String,
    #[serde(default)]
    pub refresh_token: Option<String>,
    /// Unix seconds when the access token expires (0 = unknown).
    #[serde(default)]
    pub expires_at: i64,
    #[serde(default)]
    pub token_endpoint: Option<String>,
    #[serde(default)]
    pub client_id: Option<String>,
    #[serde(default)]
    pub issuer: Option<String>,
    #[serde(default)]
    pub scope: Option<String>,
}

impl TokenSet {
    pub fn expired(&self) -> bool {
        self.expires_at > 0 && chrono::Utc::now().timestamp() >= self.expires_at - 30
    }
}

// TokenSet must never be Debug-printed with secrets.
pub struct Redacted<'a>(pub &'a TokenSet);
impl std::fmt::Debug for Redacted<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "TokenSet {{ issuer: {:?}, expires_at: {}, has_refresh: {} }}",
            self.0.issuer,
            self.0.expires_at,
            self.0.refresh_token.is_some()
        )
    }
}

pub fn config_dir() -> PathBuf {
    if let Ok(d) = std::env::var("GROKBOT_CONFIG_DIR") {
        if !d.is_empty() {
            return PathBuf::from(d);
        }
    }
    if let Some(x) = std::env::var_os("XDG_CONFIG_HOME") {
        return PathBuf::from(x).join("grokbot-control");
    }
    if let Some(h) = std::env::var_os("HOME") {
        return PathBuf::from(h).join(".config/grokbot-control");
    }
    PathBuf::from(".grokbot-control")
}

pub fn auth_path() -> PathBuf {
    if let Ok(p) = std::env::var("GROKBOT_AUTH_FILE") {
        if !p.is_empty() {
            return PathBuf::from(p);
        }
    }
    config_dir().join("auth.json")
}

pub fn load_auth() -> Result<Option<TokenSet>> {
    let path = auth_path();
    if !path.exists() {
        return Ok(None);
    }
    let raw = std::fs::read_to_string(&path)
        .with_context(|| format!("read auth file {}", path.display()))?;
    let tokens: TokenSet = serde_json::from_str(&raw).with_context(|| {
        format!(
            "parse auth file {} (try `grokbot-control logout` then `login`)",
            path.display()
        )
    })?;
    Ok(Some(tokens))
}

pub fn save_auth(tokens: &TokenSet) -> Result<PathBuf> {
    let path = auth_path();
    let dir = path
        .parent()
        .ok_or_else(|| anyhow!("auth path has no parent"))?;
    std::fs::create_dir_all(dir)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700));
    }
    let tmp = dir.join(format!(".auth.{}.tmp", std::process::id()));
    std::fs::write(&tmp, serde_json::to_string_pretty(tokens)?)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o600))?;
    }
    std::fs::rename(&tmp, &path)?;
    Ok(path)
}

pub fn clear_auth() -> Result<bool> {
    let path = auth_path();
    if path.exists() {
        std::fs::remove_file(&path)?;
        Ok(true)
    } else {
        Ok(false)
    }
}

pub fn default_issuer() -> String {
    std::env::var("GROK_OAUTH_ISSUER")
        .ok()
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "https://accounts.x.ai".to_string())
}
pub fn default_client_id() -> String {
    std::env::var("GROK_OAUTH_CLIENT_ID")
        .ok()
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "grokbot-control".to_string())
}

#[derive(Debug, Clone, Default)]
pub struct OAuthEndpoints {
    pub authorization_endpoint: Option<String>,
    pub token_endpoint: Option<String>,
    pub device_authorization_endpoint: Option<String>,
    pub userinfo_endpoint: Option<String>,
}

pub async fn discover(issuer: &str) -> Result<OAuthEndpoints> {
    let issuer = issuer.trim_end_matches('/');
    // Explicit endpoint overrides win (useful for stub issuers in tests).
    let mut ep = OAuthEndpoints {
        authorization_endpoint: std::env::var("GROK_OAUTH_AUTHORIZE_URL")
            .ok()
            .filter(|s| !s.is_empty()),
        token_endpoint: std::env::var("GROK_OAUTH_TOKEN_URL")
            .ok()
            .filter(|s| !s.is_empty()),
        device_authorization_endpoint: std::env::var("GROK_OAUTH_DEVICE_URL")
            .ok()
            .filter(|s| !s.is_empty()),
        userinfo_endpoint: None,
    };
    if ep.authorization_endpoint.is_some() && ep.token_endpoint.is_some() {
        return Ok(ep);
    }
    let url = format!("{issuer}/.well-known/openid-configuration");
    let v: serde_json::Value = reqwest::Client::new()
        .get(&url)
        .send()
        .await
        .with_context(|| format!("OAuth discovery failed at {url} — is {issuer} an OAuth issuer? Set GROK_OAUTH_AUTHORIZE_URL/GROK_OAUTH_TOKEN_URL to override endpoints."))?
        .error_for_status()
        .with_context(|| format!("OAuth discovery at {url} returned an error"))?
        .json()
        .await
        .context("parse OAuth discovery document")?;
    let get = |k: &str| v.get(k).and_then(|x| x.as_str()).map(|s| s.to_string());
    ep.authorization_endpoint = ep
        .authorization_endpoint
        .or_else(|| get("authorization_endpoint"));
    ep.token_endpoint = ep.token_endpoint.or_else(|| get("token_endpoint"));
    ep.device_authorization_endpoint = ep
        .device_authorization_endpoint
        .or_else(|| get("device_authorization_endpoint"));
    ep.userinfo_endpoint = get("userinfo_endpoint");
    Ok(ep)
}

fn token_from_response(
    v: &serde_json::Value,
    ep: &OAuthEndpoints,
    issuer: &str,
    client_id: &str,
) -> Result<TokenSet> {
    let access = v["access_token"]
        .as_str()
        .ok_or_else(|| {
            anyhow!(
                "token response had no access_token: {}",
                v.to_string().chars().take(200).collect::<String>()
            )
        })?
        .to_string();
    let expires_in = v["expires_in"].as_i64().unwrap_or(3600);
    Ok(TokenSet {
        access_token: access,
        refresh_token: v["refresh_token"].as_str().map(|s| s.to_string()),
        expires_at: chrono::Utc::now().timestamp() + expires_in,
        token_endpoint: ep.token_endpoint.clone(),
        client_id: Some(client_id.to_string()),
        issuer: Some(issuer.to_string()),
        scope: v["scope"].as_str().map(|s| s.to_string()),
    })
}

async fn post_token(url: &str, form: &[(&str, &str)]) -> Result<serde_json::Value> {
    let resp = reqwest::Client::new()
        .post(url)
        .form(form)
        .send()
        .await
        .with_context(|| format!("token request to {url} failed"))?;
    let status = resp.status();
    let text = resp.text().await.unwrap_or_default();
    if !status.is_success() {
        bail!(
            "token endpoint returned {status}: {}",
            text.chars().take(300).collect::<String>()
        );
    }
    serde_json::from_str(&text).context("parse token response")
}

pub async fn refresh(tokens: &TokenSet) -> Result<TokenSet> {
    let refresh = tokens.refresh_token.clone().ok_or_else(|| {
        anyhow!("access token expired and no refresh token is stored — run `grokbot-control login` again")
    })?;
    let url = tokens
        .token_endpoint
        .clone()
        .ok_or_else(|| anyhow!("no token endpoint stored — run `grokbot-control login` again"))?;
    let client_id = tokens.client_id.clone().unwrap_or_else(default_client_id);
    let v = post_token(
        &url,
        &[
            ("grant_type", "refresh_token"),
            ("refresh_token", &refresh),
            ("client_id", &client_id),
        ],
    )
    .await?;
    let ep = OAuthEndpoints {
        token_endpoint: Some(url),
        ..Default::default()
    };
    let mut next = token_from_response(
        &v,
        &ep,
        tokens.issuer.as_deref().unwrap_or_default(),
        &client_id,
    )?;
    if next.refresh_token.is_none() {
        next.refresh_token = Some(refresh);
    }
    Ok(next)
}

/// Browser authorization-code + PKCE login. Opens the browser when
/// `open_browser`, always prints the URL. Waits on a localhost callback.
pub async fn login_browser(issuer: &str, client_id: &str, open_browser: bool) -> Result<TokenSet> {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let ep = discover(issuer).await?;
    let auth_url_base = ep.authorization_endpoint.clone().ok_or_else(|| {
        anyhow!("issuer {issuer} published no authorization_endpoint — cannot do browser login; try `login --device`")
    })?;
    let token_url = ep
        .token_endpoint
        .clone()
        .ok_or_else(|| anyhow!("issuer published no token_endpoint"))?;

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
    let port = listener.local_addr()?.port();
    let redirect_uri = format!("http://127.0.0.1:{port}/callback");
    let (verifier, challenge) = pkce_pair();
    let state = random_token(16);
    let url = format!(
        "{auth_url_base}?response_type=code&client_id={}&redirect_uri={}&scope={}&state={}&code_challenge={}&code_challenge_method=S256",
        urlencoding(client_id),
        urlencoding(&redirect_uri),
        urlencoding("openid profile offline_access"),
        state,
        challenge
    );
    eprintln!("Open this URL to log in:\n\n  {url}\n");
    if open_browser {
        open_url(&url);
    }
    eprintln!("Waiting for the browser callback on {redirect_uri} (5 min)…");
    let (mut socket, _) =
        tokio::time::timeout(std::time::Duration::from_secs(300), listener.accept())
            .await
            .map_err(|_| anyhow!("login timed out after 5 minutes"))??;
    let mut buf = [0u8; 4096];
    let n = socket.read(&mut buf).await?;
    let request = String::from_utf8_lossy(&buf[..n]).to_string();
    let _ = socket
        .write_all(b"HTTP/1.1 200 OK\r\ncontent-type: text/plain\r\n\r\nLogged in. You can close this tab and return to grokbot-control.\r\n")
        .await;
    let query = request.split_whitespace().nth(1).unwrap_or("");
    let params = parse_query(query);
    if params.get("state").map(|s| s.as_str()) != Some(state.as_str()) {
        bail!("OAuth state mismatch — aborting (possible mix-up between login attempts)");
    }
    if let Some(err) = params.get("error") {
        bail!("login was refused by the issuer: {err}");
    }
    let code = params
        .get("code")
        .cloned()
        .ok_or_else(|| anyhow!("callback had no code"))?;
    let v = post_token(
        &token_url,
        &[
            ("grant_type", "authorization_code"),
            ("code", &code),
            ("redirect_uri", &redirect_uri),
            ("client_id", client_id),
            ("code_verifier", &verifier),
        ],
    )
    .await?;
    token_from_response(&v, &ep, issuer, client_id)
}

/// Device-code login for headless machines (no browser callback needed).
pub async fn login_device(issuer: &str, client_id: &str) -> Result<TokenSet> {
    let ep = discover(issuer).await?;
    let device_url = ep.device_authorization_endpoint.clone().ok_or_else(|| {
        anyhow!("issuer {issuer} published no device_authorization_endpoint — use browser `login` instead")
    })?;
    let token_url = ep
        .token_endpoint
        .clone()
        .ok_or_else(|| anyhow!("issuer published no token_endpoint"))?;
    let v = post_token(
        &device_url,
        &[
            ("client_id", client_id),
            ("scope", "openid profile offline_access"),
        ],
    )
    .await?;
    let device_code = v["device_code"]
        .as_str()
        .ok_or_else(|| anyhow!("device response had no device_code"))?
        .to_string();
    let user_code = v["user_code"].as_str().unwrap_or_default();
    let verify_url = v["verification_uri"]
        .as_str()
        .or_else(|| v["verification_url"].as_str())
        .unwrap_or(issuer);
    let interval = v["interval"].as_u64().unwrap_or(5).max(1);
    eprintln!("Go to {verify_url} and enter code: {user_code}");
    eprintln!("Waiting for approval (5 min)…");
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(300);
    loop {
        if std::time::Instant::now() > deadline {
            bail!("device login timed out after 5 minutes");
        }
        tokio::time::sleep(std::time::Duration::from_secs(interval)).await;
        let resp = reqwest::Client::new()
            .post(&token_url)
            .form(&[
                ("grant_type", "urn:ietf:params:oauth:grant-type:device_code"),
                ("device_code", device_code.as_str()),
                ("client_id", client_id),
            ])
            .send()
            .await?;
        let status = resp.status();
        let text = resp.text().await.unwrap_or_default();
        let body: serde_json::Value = serde_json::from_str(&text).unwrap_or_default();
        if status.is_success() {
            return token_from_response(&body, &ep, issuer, client_id);
        }
        match body["error"].as_str() {
            Some("authorization_pending") | Some("slow_down") => continue,
            Some(other) => bail!("device login failed: {other}"),
            None => bail!(
                "device login failed ({status}): {}",
                text.chars().take(200).collect::<String>()
            ),
        }
    }
}

fn parse_query(url: &str) -> std::collections::HashMap<String, String> {
    let q = url.split('?').nth(1).unwrap_or("");
    q.split('&')
        .filter_map(|pair| pair.split_once('='))
        .map(|(k, v)| (k.to_string(), percent_decode(v)))
        .collect()
}

fn percent_decode(s: &str) -> String {
    let bytes = s.replace('+', " ").into_bytes();
    let mut out = Vec::new();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            if let Ok(n) =
                u8::from_str_radix(std::str::from_utf8(&bytes[i + 1..i + 3]).unwrap_or(""), 16)
            {
                out.push(n);
                i += 3;
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).to_string()
}

pub fn urlencoding(s: &str) -> String {
    let mut out = String::new();
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(b as char)
            }
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

fn open_url(url: &str) {
    #[cfg(target_os = "macos")]
    let _ = std::process::Command::new("open").arg(url).spawn();
    #[cfg(target_os = "linux")]
    let _ = std::process::Command::new("xdg-open").arg(url).spawn();
    #[cfg(target_os = "windows")]
    let _ = std::process::Command::new("cmd")
        .args(["/C", "start", url])
        .spawn();
}

// ---------- scopes (shared by server OAuth + static tokens) ----------

pub const SCOPE_READ: &str = "read";
pub const SCOPE_CHAT: &str = "chat";
pub const SCOPE_ADMIN: &str = "admin";
pub const ALL_SCOPES: &[&str] = &[SCOPE_READ, SCOPE_CHAT, SCOPE_ADMIN];

pub fn parse_scopes(s: &str) -> Vec<String> {
    s.split(|c: char| c.is_whitespace() || c == ',')
        .map(|x| x.trim().to_string())
        .filter(|x| !x.is_empty())
        .collect()
}

pub fn scope_allows(granted: &[String], required: &str) -> bool {
    granted
        .iter()
        .any(|g| g == required || g == "*" || g == SCOPE_ADMIN)
}

/// Tools that mutate bot state or spend money need `chat`;
/// destructive/admin tools need `admin`; everything else is `read`.
pub fn required_scope(tool: &str) -> &'static str {
    match tool {
        "state_import"
        | "state_reset"
        | "state_restore"
        | "usage_reset"
        | "request_log_clear"
        | "memory_clear"
        | "bot_update_config"
        | "bot_safety_update"
        | "bot_set_default_model" => SCOPE_ADMIN,
        "chat"
        | "chat_batch"
        | "chat_stream"
        | "model_compare"
        | "conversation_send"
        | "conversation_create"
        | "conversation_add_message"
        | "image_generate"
        | "image_understand"
        | "audio_transcribe"
        | "audio_speak"
        | "video_generate"
        | "web_search"
        | "x_search"
        | "task_run"
        | "routines"
        | "text_summarize"
        | "text_translate"
        | "template_render" => SCOPE_CHAT,
        _ if tool.ends_with("_create")
            || tool.ends_with("_update")
            || tool.ends_with("_delete")
            || tool.ends_with("_store")
            || tool.ends_with("_add")
            || tool.contains("_set_")
            || tool.ends_with("_activate")
            || tool.ends_with("_duplicate")
            || tool.ends_with("_import")
            || tool.ends_with("_fork")
            || tool.ends_with("_clear")
            || tool.ends_with("_rename") =>
        {
            SCOPE_CHAT
        }
        _ => SCOPE_READ,
    }
}
