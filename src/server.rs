//! Transports: stdio (NDJSON), Streamable HTTP (`POST/GET/DELETE /mcp`,
//! JSON or SSE responses), legacy SSE (`GET /sse` + `POST /messages`),
//! plus a built-in OAuth 2.1 authorization server for MCP clients
//! (discovery, authorize with approver consent, token with PKCE).

use crate::auth;
use crate::mcp::{self, Conn};
use crate::state::new_id;
use crate::tools::Ctx;
use anyhow::Result;
use axum::{
    body::Body,
    extract::{Query, State},
    http::{HeaderMap, StatusCode},
    response::{Html, IntoResponse, Redirect, Response},
    routing::{get, post},
    Form, Json, Router,
};
use serde::Deserialize;
use serde_json::{json, Value};
use std::collections::HashMap;
use std::sync::Arc;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::sync::{mpsc, Mutex};

/// Which web endpoints a web server exposes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WebMode {
    Http,
    Sse,
    All,
}

impl WebMode {
    pub fn parse(s: &str) -> Result<Self> {
        match s {
            "http" => Ok(Self::Http),
            "sse" => Ok(Self::Sse),
            "all" => Ok(Self::All),
            other => anyhow::bail!("unknown web mode '{other}' (expected http|sse|all)"),
        }
    }
    fn has_http(&self) -> bool {
        matches!(self, Self::Http | Self::All)
    }
    fn has_sse(&self) -> bool {
        matches!(self, Self::Sse | Self::All)
    }
    pub fn label(&self) -> &'static str {
        match self {
            Self::Http => "http",
            Self::Sse => "sse",
            Self::All => "all (http+sse)",
        }
    }
}

/// stdio transport: one JSON-RPC message per line. Per-connection
/// protocol state lives here (Conn), shared bot state in Ctx.
pub async fn serve_stdio(ctx: Ctx) -> Result<()> {
    let stdin = tokio::io::stdin();
    let mut stdout = tokio::io::stdout();
    let mut lines = BufReader::new(stdin).lines();
    let mut conn = Conn::default();
    while let Some(line) = lines.next_line().await? {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let req: Value = match serde_json::from_str(line) {
            Ok(v) => v,
            Err(e) => {
                let resp = json!({"jsonrpc": "2.0", "id": null,
                                  "error": {"code": -32700, "message": format!("parse error: {e}")}});
                stdout.write_all(format!("{resp}\n").as_bytes()).await?;
                stdout.flush().await?;
                continue;
            }
        };
        if let Some(resp) = mcp::handle_any_conn(&ctx, &mut conn, &req).await {
            stdout.write_all(format!("{resp}\n").as_bytes()).await?;
            stdout.flush().await?;
        }
    }
    Ok(())
}

// ---------- OAuth authorization server state ----------

#[allow(dead_code)]
#[derive(Clone, Debug)]
struct OAuthCode {
    challenge: String,
    redirect_uri: String,
    client_id: String,
    scopes: Vec<String>,
    expires: std::time::Instant,
}

#[derive(Clone, Debug)]
struct OAuthGrant {
    scopes: Vec<String>,
    expires: std::time::Instant,
    refresh_hash: Option<String>,
}

#[derive(Default)]
#[allow(dead_code)]
pub struct OAuthAs {
    codes: Mutex<HashMap<String, OAuthCode>>,
    grants: Mutex<HashMap<String, OAuthGrant>>, // keyed by sha256(access token)
    client_id: String,
}

type Sessions = Arc<Mutex<HashMap<String, (mpsc::Sender<String>, std::time::Instant)>>>;

#[derive(Clone)]
struct HttpState {
    ctx: Ctx,
    auth_token: Option<String>,
    sessions: Sessions,
    mode: WebMode,
    oauth: Arc<OAuthAs>,
    oauth_enabled: bool,
}

fn bearer(headers: &HeaderMap) -> Option<String> {
    headers
        .get("authorization")
        .and_then(|v| v.to_str().ok())
        .map(|v| {
            v.strip_prefix("Bearer ")
                .or_else(|| v.strip_prefix("bearer "))
                .unwrap_or(v)
                .to_string()
        })
}

fn ct_eq(a: &str, b: &str) -> bool {
    use subtle::ConstantTimeEq;
    a.as_bytes().ct_eq(b.as_bytes()).into()
}

async fn authorized_scopes(state: &HttpState, headers: &HeaderMap) -> Option<Vec<String>> {
    let token = bearer(headers)?;
    // OAuth access token?
    let hash = auth::sha256_hex(token.as_bytes());
    {
        let grants = state.oauth.grants.lock().await;
        if let Some(g) = grants.get(&hash) {
            if g.expires > std::time::Instant::now() {
                return Some(g.scopes.clone());
            }
        }
    }
    // Static approver token grants everything.
    if let Some(expected) = &state.auth_token {
        if !expected.is_empty() && ct_eq(&token, expected) {
            return Some(vec!["*".to_string()]);
        }
    }
    // No static token configured: local/dev mode is open (loopback-only
    // enforced at bind time unless a token is set).
    if state
        .auth_token
        .as_ref()
        .map(|t| t.is_empty())
        .unwrap_or(true)
        && !state.oauth_enabled
    {
        return Some(vec!["*".to_string()]);
    }
    None
}

fn unauthorized() -> Response {
    (
        StatusCode::UNAUTHORIZED,
        Json(json!({"jsonrpc": "2.0", "id": null,
                    "error": {"code": -32001, "message": "unauthorized: missing/invalid bearer token"}})),
    )
        .into_response()
}

fn sse_response(rx: mpsc::Receiver<String>, session_id: Option<String>) -> Response {
    use tokio_stream::StreamExt as _;
    let stream =
        tokio_stream::wrappers::ReceiverStream::new(rx).map(Ok::<String, std::convert::Infallible>);
    let mut resp = Response::new(Body::from_stream(stream));
    resp.headers_mut()
        .insert("content-type", "text/event-stream".parse().unwrap());
    resp.headers_mut()
        .insert("cache-control", "no-cache".parse().unwrap());
    if let Some(id) = session_id {
        resp.headers_mut()
            .insert("mcp-session-id", id.parse().unwrap());
    }
    resp
}

fn spawn_keepalive(tx: mpsc::Sender<String>) {
    tokio::spawn(async move {
        let mut tick = tokio::time::interval(std::time::Duration::from_secs(15));
        tick.tick().await;
        loop {
            tick.tick().await;
            if tx.try_send(": ping\n\n".to_string()).is_err() {
                // Full or closed: closed ends the loop; full skips a beat.
                if tx.is_closed() {
                    break;
                }
            }
        }
    });
}

fn wants_sse(headers: &HeaderMap) -> bool {
    headers
        .get("accept")
        .and_then(|v| v.to_str().ok())
        .map(|v| v.contains("text/event-stream"))
        .unwrap_or(false)
}

fn is_loopback_bind(addr: &str) -> bool {
    let host = addr
        .rsplit(':')
        .nth(1)
        .unwrap_or(addr)
        .trim_matches(['[', ']']);
    matches!(host, "127.0.0.1" | "localhost" | "::1") || host.is_empty()
}

/// Serve a web transport on `addr`. Refuses non-loopback binds without
/// an auth mechanism (token or OAuth) — finding 1 in the audit.
pub async fn serve_web(
    ctx: Ctx,
    addr: &str,
    auth_token: Option<String>,
    mode: WebMode,
) -> Result<()> {
    serve_web_oauth(ctx, addr, auth_token, mode, false).await
}

pub async fn serve_web_oauth(
    ctx: Ctx,
    addr: &str,
    auth_token: Option<String>,
    mode: WebMode,
    oauth_enabled: bool,
) -> Result<()> {
    let has_token = auth_token.as_ref().map(|t| !t.is_empty()).unwrap_or(false);
    if !is_loopback_bind(addr) && !has_token && !oauth_enabled {
        anyhow::bail!(
            "refusing to bind {addr} without authentication: set --auth-token / MCP_AUTH_TOKEN or enable --oauth, or bind 127.0.0.1"
        );
    }
    if auth_token.as_ref().map(|t| t.is_empty()).unwrap_or(false) && !oauth_enabled {
        eprintln!("grokbot-control: WARNING no auth token set (loopback only is fine; never expose this port)");
    }
    let state = Arc::new(HttpState {
        ctx,
        auth_token,
        sessions: Arc::new(Mutex::new(HashMap::new())),
        mode,
        oauth: Arc::new(OAuthAs {
            codes: Mutex::new(HashMap::new()),
            grants: Mutex::new(HashMap::new()),
            client_id: std::env::var("GROKBOT_OAUTH_CLIENT_ID")
                .unwrap_or_else(|_| "grokbot-mcp-client".into()),
        }),
        oauth_enabled,
    });
    {
        let sessions = state.sessions.clone();
        tokio::spawn(async move {
            let mut tick = tokio::time::interval(std::time::Duration::from_secs(30));
            loop {
                tick.tick().await;
                let mut map = sessions.lock().await;
                map.retain(|_, (tx, created)| {
                    !tx.is_closed() && created.elapsed() < std::time::Duration::from_secs(1800)
                });
            }
        });
    }
    let mut app = Router::new()
        .route("/", get(root_info))
        .route("/health", get(health));
    if mode.has_http() {
        app = app
            .route("/mcp", post(mcp_post))
            .route("/mcp", get(mcp_get_stream))
            .route("/mcp", axum::routing::delete(mcp_delete_session));
    }
    if mode.has_sse() {
        app = app
            .route("/sse", get(sse_get))
            .route("/messages", post(messages_post));
    }
    if oauth_enabled {
        app = app
            .route(
                "/.well-known/oauth-protected-resource",
                get(protected_resource_meta),
            )
            .route("/.well-known/oauth-authorization-server", get(as_meta))
            .route("/oauth/authorize", get(oauth_authorize))
            .route("/oauth/approve", post(oauth_approve))
            .route("/oauth/token", post(oauth_token))
            .route("/oauth/register", post(oauth_register));
    }
    let app = app.with_state(state);
    let listener = tokio::net::TcpListener::bind(addr).await?;
    eprintln!(
        "grokbot-control {} listening on http://{addr} (health: /health{}, oauth: {})",
        mode.label(),
        match mode {
            WebMode::Http => ", endpoint: /mcp",
            WebMode::Sse => ", endpoint: /sse + /messages",
            WebMode::All => ", endpoints: /mcp + /sse + /messages",
        },
        if oauth_enabled { "on" } else { "off" }
    );
    axum::serve(listener, app)
        .with_graceful_shutdown(async {
            let _ = tokio::signal::ctrl_c().await;
            eprintln!("grokbot-control: shutting down");
        })
        .await?;
    Ok(())
}

pub async fn serve_http(ctx: Ctx, addr: &str, auth_token: Option<String>) -> Result<()> {
    serve_web(ctx, addr, auth_token, WebMode::Http).await
}

async fn root_info(State(st): State<Arc<HttpState>>, headers: HeaderMap) -> Response {
    if authorized_scopes(&st, &headers).await.is_none() {
        return unauthorized();
    }
    Json(json!({
        "name": crate::SERVER_NAME, "version": crate::VERSION,
        "backend": st.ctx.backend.name(), "mode": st.mode.label(),
        "transports": {
            "stdio": "newline-delimited JSON-RPC on stdin/stdout",
            "http": st.mode.has_http().then_some("POST/GET/DELETE /mcp (JSON or SSE responses)"),
            "sse": st.mode.has_sse().then_some("GET /sse + POST /messages?sessionId=<id>"),
        },
        "oauth_enabled": st.oauth_enabled,
        "health": "/health",
        "tools": crate::tools::tool_definitions().len(),
        "resources": crate::tools::resource_definitions().len(),
        "prompts": crate::tools::prompt_definitions().len(),
    }))
    .into_response()
}

async fn health(State(st): State<Arc<HttpState>>, headers: HeaderMap) -> Response {
    // Liveness is public and minimal; detail needs auth.
    if authorized_scopes(&st, &headers).await.is_none() {
        return Json(json!({"status": "ok"})).into_response();
    }
    let sessions = st.sessions.lock().await.len();
    Json(json!({"status": "ok", "server": crate::SERVER_NAME,
                "version": crate::VERSION, "backend": st.ctx.backend.name(),
                "live": st.ctx.backend.is_live(), "mode": st.mode.label(),
                "sse_sessions": sessions, "uptime_seconds": st.ctx.uptime_seconds()}))
    .into_response()
}

async fn request_ctx(st: &HttpState, headers: &HeaderMap) -> Option<Ctx> {
    let scopes = authorized_scopes(st, headers).await?;
    Some(st.ctx.clone().with_scopes(scopes))
}

async fn mcp_post(
    State(st): State<Arc<HttpState>>,
    headers: HeaderMap,
    body: axum::body::Bytes,
) -> Response {
    let Some(ctx) = request_ctx(&st, &headers).await else {
        return unauthorized();
    };
    let req: Value = match serde_json::from_slice(&body) {
        Ok(v) => v,
        Err(e) => {
            return (
                StatusCode::BAD_REQUEST,
                Json(json!({"jsonrpc": "2.0", "id": null,
                            "error": {"code": -32700, "message": format!("parse error: {e}")}})),
            )
                .into_response();
        }
    };
    match mcp::handle_any(&ctx, &req).await {
        Some(resp) => {
            if wants_sse(&headers) {
                let (tx, rx) = mpsc::channel::<String>(64);
                let _ = tx.send(format!("event: message\ndata: {resp}\n\n")).await;
                drop(tx);
                sse_response(rx, None)
            } else {
                Json(resp).into_response()
            }
        }
        None => StatusCode::ACCEPTED.into_response(),
    }
}

async fn mcp_get_stream(State(st): State<Arc<HttpState>>, headers: HeaderMap) -> Response {
    if authorized_scopes(&st, &headers).await.is_none() {
        return unauthorized();
    }
    if !wants_sse(&headers) {
        return (
            StatusCode::NOT_ACCEPTABLE,
            Json(json!({"error": "GET /mcp requires Accept: text/event-stream"})),
        )
            .into_response();
    }
    let session_id = new_id();
    let (tx, rx) = mpsc::channel::<String>(64);
    let _ = tx
        .send(": grokbot-control stream opened\n\n".to_string())
        .await;
    st.sessions
        .lock()
        .await
        .insert(session_id.clone(), (tx.clone(), std::time::Instant::now()));
    spawn_keepalive(tx);
    sse_response(rx, Some(session_id))
}

async fn mcp_delete_session(State(st): State<Arc<HttpState>>, headers: HeaderMap) -> Response {
    if authorized_scopes(&st, &headers).await.is_none() {
        return unauthorized();
    }
    if let Some(id) = headers.get("mcp-session-id").and_then(|v| v.to_str().ok()) {
        if st.sessions.lock().await.remove(id).is_none() {
            return (
                StatusCode::NOT_FOUND,
                Json(json!({"error": "unknown session"})),
            )
                .into_response();
        }
    }
    StatusCode::OK.into_response()
}

async fn sse_get(State(st): State<Arc<HttpState>>, headers: HeaderMap) -> Response {
    if authorized_scopes(&st, &headers).await.is_none() {
        return unauthorized();
    }
    let session_id = new_id();
    let (tx, rx) = mpsc::channel::<String>(64);
    let _ = tx
        .send(format!(
            "event: endpoint\ndata: /messages?sessionId={session_id}\n\n"
        ))
        .await;
    st.sessions
        .lock()
        .await
        .insert(session_id.clone(), (tx.clone(), std::time::Instant::now()));
    spawn_keepalive(tx);
    sse_response(rx, Some(session_id))
}

async fn messages_post(
    State(st): State<Arc<HttpState>>,
    headers: HeaderMap,
    Query(q): Query<HashMap<String, String>>,
    body: axum::body::Bytes,
) -> Response {
    let Some(ctx) = request_ctx(&st, &headers).await else {
        return unauthorized();
    };
    let Some(session_id) = q.get("sessionId").or_else(|| q.get("session_id")) else {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({"error": "missing sessionId query parameter"})),
        )
            .into_response();
    };
    let tx = st
        .sessions
        .lock()
        .await
        .get(session_id)
        .map(|(tx, _)| tx.clone());
    let Some(tx) = tx else {
        return (
            StatusCode::NOT_FOUND,
            Json(json!({"error": format!("unknown or closed SSE session: {session_id}")})),
        )
            .into_response();
    };
    let req: Value = match serde_json::from_slice(&body) {
        Ok(v) => v,
        Err(e) => {
            return (
                StatusCode::BAD_REQUEST,
                Json(json!({"jsonrpc": "2.0", "id": null,
                            "error": {"code": -32700, "message": format!("parse error: {e}")}})),
            )
                .into_response();
        }
    };
    if let Some(resp) = mcp::handle_any(&ctx, &req).await {
        if tx
            .send(format!("event: message\ndata: {resp}\n\n"))
            .await
            .is_err()
        {
            st.sessions.lock().await.remove(session_id);
            return (
                StatusCode::GONE,
                Json(json!({"error": "SSE session closed"})),
            )
                .into_response();
        }
    }
    StatusCode::ACCEPTED.into_response()
}

// ---------- Built-in OAuth 2.1 authorization server ----------

fn issuer_base(headers: &HeaderMap) -> String {
    let host = headers
        .get("host")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("127.0.0.1:8080");
    format!("http://{host}")
}

async fn protected_resource_meta(
    State(_st): State<Arc<HttpState>>,
    headers: HeaderMap,
) -> Response {
    let base = issuer_base(&headers);
    Json(json!({
        "resource": format!("{base}/mcp"),
        "authorization_servers": [base],
        "scopes_supported": auth::ALL_SCOPES,
        "bearer_methods_supported": ["header"],
    }))
    .into_response()
}

async fn as_meta(State(_st): State<Arc<HttpState>>, headers: HeaderMap) -> Response {
    let base = issuer_base(&headers);
    Json(json!({
        "issuer": base,
        "authorization_endpoint": format!("{base}/oauth/authorize"),
        "token_endpoint": format!("{base}/oauth/token"),
        "registration_endpoint": format!("{base}/oauth/register"),
        "scopes_supported": auth::ALL_SCOPES,
        "response_types_supported": ["code"],
        "grant_types_supported": ["authorization_code", "refresh_token"],
        "code_challenge_methods_supported": ["S256"],
        "token_endpoint_auth_methods_supported": ["none"],
    }))
    .into_response()
}

async fn oauth_register(State(st): State<Arc<HttpState>>, Json(body): Json<Value>) -> Response {
    let _ = st;
    let client_id = format!("client-{}", &new_id()[..12]);
    (
        StatusCode::CREATED,
        Json(json!({
            "client_id": client_id,
            "client_name": body["client_name"],
            "redirect_uris": body["redirect_uris"],
            "grant_types": ["authorization_code", "refresh_token"],
            "response_types": ["code"],
            "token_endpoint_auth_method": "none",
        })),
    )
        .into_response()
}

async fn oauth_authorize(Query(q): Query<HashMap<String, String>>) -> Response {
    let client_id = q.get("client_id").cloned().unwrap_or_default();
    let redirect_uri = q.get("redirect_uri").cloned().unwrap_or_default();
    let state = q.get("state").cloned().unwrap_or_default();
    let challenge = q.get("code_challenge").cloned().unwrap_or_default();
    let scope = q.get("scope").cloned().unwrap_or_else(|| "read".into());
    if redirect_uri.is_empty() || challenge.is_empty() {
        return (
            StatusCode::BAD_REQUEST,
            "redirect_uri and code_challenge (PKCE S256) are required",
        )
            .into_response();
    }
    let html = format!(
        "<!doctype html><title>GrokBot Control — authorize</title>\
         <h1>Authorize MCP client</h1><p>Client <b>{client_id}</b> requests scopes <b>{scope}</b>.</p>\
         <form method='post' action='/oauth/approve'>\
         <input type='hidden' name='client_id' value='{client_id}'>\
         <input type='hidden' name='redirect_uri' value='{redirect_uri}'>\
         <input type='hidden' name='state' value='{state}'>\
         <input type='hidden' name='code_challenge' value='{challenge}'>\
         <input type='hidden' name='scope' value='{scope}'>\
         <label>Approver token (your server --auth-token): <input type='password' name='approver_token' required></label>\
         <button type='submit'>Approve</button></form>"
    );
    Html(html).into_response()
}

#[derive(Deserialize)]
struct ApproveForm {
    client_id: String,
    redirect_uri: String,
    state: String,
    code_challenge: String,
    scope: String,
    approver_token: String,
}

async fn oauth_approve(
    State(st): State<Arc<HttpState>>,
    Form(form): Form<ApproveForm>,
) -> Response {
    let Some(expected) = &st.auth_token else {
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            "OAuth approve requires a server --auth-token as approver credential",
        )
            .into_response();
    };
    if expected.is_empty() || !ct_eq(&form.approver_token, expected) {
        return (StatusCode::UNAUTHORIZED, "bad approver token").into_response();
    }
    let parsed = reqwest::Url::parse(&form.redirect_uri);
    let local_ok = parsed
        .as_ref()
        .map(|u| {
            let host = u.host_str().unwrap_or_default();
            u.scheme() == "https" || matches!(host, "127.0.0.1" | "localhost" | "::1")
        })
        .unwrap_or(false);
    if !local_ok {
        return (
            StatusCode::BAD_REQUEST,
            "redirect_uri must be https or localhost",
        )
            .into_response();
    }
    let code = auth::random_token(24);
    let scopes = auth::parse_scopes(&form.scope);
    st.oauth.codes.lock().await.insert(
        auth::sha256_hex(code.as_bytes()),
        OAuthCode {
            challenge: form.code_challenge.clone(),
            redirect_uri: form.redirect_uri.clone(),
            client_id: form.client_id.clone(),
            scopes,
            expires: std::time::Instant::now() + std::time::Duration::from_secs(300),
        },
    );
    let sep = if form.redirect_uri.contains('?') {
        '&'
    } else {
        '?'
    };
    Redirect::temporary(&format!(
        "{}{sep}code={code}&state={}",
        form.redirect_uri,
        auth::urlencoding(&form.state)
    ))
    .into_response()
}

#[derive(Deserialize)]
struct TokenForm {
    grant_type: String,
    code: Option<String>,
    redirect_uri: Option<String>,
    client_id: Option<String>,
    code_verifier: Option<String>,
    refresh_token: Option<String>,
}

async fn oauth_token(State(st): State<Arc<HttpState>>, Form(form): Form<TokenForm>) -> Response {
    match form.grant_type.as_str() {
        "authorization_code" => {
            let (Some(code), Some(verifier)) = (&form.code, &form.code_verifier) else {
                return oauth_err("invalid_request", "code and code_verifier are required");
            };
            let entry = st
                .oauth
                .codes
                .lock()
                .await
                .remove(&auth::sha256_hex(code.as_bytes()));
            let Some(entry) = entry else {
                return oauth_err("invalid_grant", "unknown or already-used code");
            };
            if entry.expires < std::time::Instant::now() {
                return oauth_err("invalid_grant", "code expired");
            }
            if form.redirect_uri.as_deref() != Some(entry.redirect_uri.as_str()) {
                return oauth_err("invalid_grant", "redirect_uri mismatch");
            }
            let challenge = auth::b64url(&{
                use sha2::Digest;
                sha2::Sha256::digest(verifier.as_bytes())
            });
            if !ct_eq(&challenge, &entry.challenge) {
                return oauth_err("invalid_grant", "PKCE verification failed");
            }
            issue_tokens(&st, entry.scopes).await
        }
        "refresh_token" => {
            let Some(refresh) = &form.refresh_token else {
                return oauth_err("invalid_request", "refresh_token is required");
            };
            let hash = auth::sha256_hex(refresh.as_bytes());
            let scopes = {
                let grants = st.oauth.grants.lock().await;
                grants
                    .values()
                    .find(|g| g.refresh_hash.as_deref() == Some(hash.as_str()))
                    .map(|g| g.scopes.clone())
            };
            let Some(mut scopes) = scopes else {
                return oauth_err("invalid_grant", "unknown refresh token");
            };
            // Rotate: revoke grants tied to the old refresh token.
            st.oauth
                .grants
                .lock()
                .await
                .retain(|_, g| g.refresh_hash.as_deref() != Some(hash.as_str()));
            if form.client_id.is_some() {
                let _ = &mut scopes;
            }
            issue_tokens(&st, scopes).await
        }
        other => oauth_err(
            "unsupported_grant_type",
            &format!("unsupported grant_type {other}"),
        ),
    }
}

fn oauth_err(error: &str, description: &str) -> Response {
    (
        StatusCode::BAD_REQUEST,
        Json(json!({"error": error, "error_description": description})),
    )
        .into_response()
}

async fn issue_tokens(st: &HttpState, scopes: Vec<String>) -> Response {
    let access = auth::random_token(32);
    let refresh = auth::random_token(32);
    let refresh_hash = auth::sha256_hex(refresh.as_bytes());
    st.oauth.grants.lock().await.insert(
        auth::sha256_hex(access.as_bytes()),
        OAuthGrant {
            scopes: scopes.clone(),
            expires: std::time::Instant::now() + std::time::Duration::from_secs(3600),
            refresh_hash: Some(refresh_hash),
        },
    );
    Json(json!({
        "access_token": access,
        "token_type": "Bearer",
        "expires_in": 3600,
        "refresh_token": refresh,
        "scope": scopes.join(" "),
    }))
    .into_response()
}
