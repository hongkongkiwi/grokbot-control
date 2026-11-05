use anyhow::Result;
use clap::{Parser, Subcommand};
use grokbot_control::backend::select_backend_with;
use grokbot_control::server;
use grokbot_control::state::{
    default_state_path, legacy_default_state_path, migrate_legacy_state, Store,
};
use grokbot_control::tools::{tool_definitions, Ctx};
use std::path::PathBuf;

/// Grokbot Control — manage every aspect of a GrokBot (xAI Grok)
/// from any MCP-capable AI client, over stdio, HTTP, or SSE,
/// or directly from the CLI (`run`, `chat`, `repl`, `login`).
#[derive(Parser, Debug)]
#[command(name = "grokbot-control", version, about, long_about = None)]
struct Cli {
    #[command(subcommand)]
    command: Option<Command>,

    /// Transport: stdio | http (Streamable HTTP /mcp) | sse (legacy /sse + /messages) | all (http+sse on one port)
    #[arg(
        long,
        env = "GROKBOT_TRANSPORT",
        default_value = "stdio",
        global = true
    )]
    transport: String,

    /// Web listen address (http/sse/all transports)
    #[arg(
        long,
        env = "GROKBOT_HTTP_ADDR",
        default_value = "127.0.0.1:8080",
        global = true
    )]
    http_addr: String,

    /// State JSON file (default: GROKBOT_STATE_FILE or ~/.local/share/grokbot-control/state.json)
    #[arg(long, global = true)]
    state_file: Option<PathBuf>,

    /// Backend: auto (live iff XAI_API_KEY set) | mock | live
    #[arg(long, env = "GROKBOT_BACKEND", default_value = "auto", global = true)]
    backend: String,

    /// Credential for live backend: auto (OAuth login, then XAI_API_KEY) | oauth | api_key
    #[arg(long, env = "GROKBOT_CREDENTIAL", global = true)]
    credential: Option<String>,

    /// Named profile: isolates state under ~/.local/share/grokbot-control/profiles/<name>/
    #[arg(long, env = "GROKBOT_PROFILE", global = true)]
    profile: Option<String>,

    /// Enable the built-in OAuth 2.1 server for MCP clients on web transports
    #[arg(long, env = "MCP_OAUTH_ENABLED", global = true)]
    oauth: bool,

    /// Bearer token required on HTTP /mcp (empty = no auth; stdio ignores this)
    #[arg(long, env = "MCP_AUTH_TOKEN", global = true)]
    auth_token: Option<String>,
}

#[derive(Subcommand, Debug)]
enum Command {
    /// Serve the MCP server (default when no subcommand is given)
    Serve,
    /// Print all MCP tool definitions as JSON and exit
    Tools {
        /// Print just tool names, one per line
        #[arg(long)]
        plain: bool,
    },
    /// Print backend/state diagnostics as JSON and exit
    Doctor,
    /// CLI mode: run any single MCP tool from the shell, e.g.
    /// `run chat --prompt "hello" --temperature 0.2` (see `tools` for names;
    /// arguments are validated/coerced from each tool's own schema)
    Run {
        /// Tool name (hyphens or underscores), e.g. chat, conversation-send
        tool: String,
        /// Tool arguments as one JSON object (merged under --key value pairs)
        #[arg(long = "args")]
        args_json: Option<String>,
        /// Emit compact one-line JSON
        #[arg(long)]
        compact: bool,
        /// --key value / --key=value pairs for the tool
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        pairs: Vec<String>,
    },
    /// CLI mode: interactive shell over the same tools (`help` inside)
    Repl,
    /// CLI shortcut: one-shot chat, e.g. `chat tell me a joke`
    Chat {
        /// Prompt text
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        text: Vec<String>,
    },
    /// CLI shortcut: diagnostics (bot + server health as JSON)
    Status,
    /// Log in to Grok/xAI with OAuth (browser PKCE; --device for headless).
    /// Tokens are stored 0600 outside bot state and used as the live credential.
    Login {
        /// Use the device-code flow instead of a browser callback
        #[arg(long)]
        device: bool,
        /// Do not try to open a browser (print the URL instead)
        #[arg(long)]
        no_browser: bool,
        /// OAuth issuer (default: GROK_OAUTH_ISSUER or https://accounts.x.ai)
        #[arg(long)]
        issuer: Option<String>,
        /// OAuth client id (default: GROK_OAUTH_CLIENT_ID or grokbot-control)
        #[arg(long)]
        client_id: Option<String>,
    },
    /// Log out: delete the stored OAuth tokens (API key fallback still applies)
    Logout,
    /// Show login status (issuer, expiry, credential in effect) — never prints tokens
    Whoami,
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .with_writer(std::io::stderr)
        .init();

    let cli = Cli::parse();

    if let Some(Command::Tools { plain }) = &cli.command {
        if *plain {
            for name in grokbot_control::cli::tool_names() {
                println!("{name}");
            }
        } else {
            println!("{}", serde_json::to_string_pretty(&tool_definitions())?);
        }
        return Ok(());
    }

    match &cli.command {
        Some(Command::Login {
            device,
            no_browser,
            issuer,
            client_id,
        }) => {
            let issuer = issuer
                .clone()
                .unwrap_or_else(grokbot_control::auth::default_issuer);
            let client_id = client_id
                .clone()
                .unwrap_or_else(grokbot_control::auth::default_client_id);
            let tokens = if *device {
                grokbot_control::auth::login_device(&issuer, &client_id).await?
            } else {
                grokbot_control::auth::login_browser(&issuer, &client_id, !no_browser).await?
            };
            let path = grokbot_control::auth::save_auth(&tokens)?;
            println!(
                "{}",
                serde_json::to_string_pretty(&serde_json::json!({
                    "logged_in": true,
                    "issuer": tokens.issuer,
                    "expires_at": tokens.expires_at,
                    "auth_file": path.display().to_string(),
                    "note": "tokens are never printed and stay out of bot state; live calls use this login unless XAI_API_KEY/--credential says otherwise",
                }))?
            );
            return Ok(());
        }
        Some(Command::Logout) => {
            let removed = grokbot_control::auth::clear_auth()?;
            println!(
                "{}",
                serde_json::json!({"logged_in": false, "removed": removed,
                "note": "XAI_API_KEY fallback (if set) still applies"})
            );
            return Ok(());
        }
        Some(Command::Whoami) => {
            let tokens = grokbot_control::auth::load_auth()?;
            let key_set = !std::env::var("XAI_API_KEY")
                .unwrap_or_default()
                .trim()
                .is_empty();
            println!(
                "{}",
                serde_json::to_string_pretty(&serde_json::json!({
                    "logged_in": tokens.is_some(),
                    "issuer": tokens.as_ref().and_then(|t| t.issuer.clone()),
                    "expires_at": tokens.as_ref().map(|t| t.expires_at),
                    "expired": tokens.as_ref().map(|t| t.expired()),
                    "has_refresh_token": tokens.as_ref().map(|t| t.refresh_token.is_some()),
                    "api_key_set": key_set,
                    "credential_in_effect": if tokens.is_some() { "oauth" } else if key_set { "api_key" } else { "mock (none)" },
                    "auth_file": grokbot_control::auth::auth_path().display().to_string(),
                }))?
            );
            return Ok(());
        }
        _ => {}
    }

    let make_ctx = |cli: &Cli| -> Result<(Ctx, PathBuf)> {
        let backend = select_backend_with(&cli.backend, cli.credential.as_deref())?;
        let explicit = cli.state_file.is_some()
            || !std::env::var("GROKBOT_STATE_FILE")
                .unwrap_or_default()
                .is_empty();
        let path = match (&cli.state_file, &cli.profile) {
            (Some(p), _) => p.clone(),
            (None, Some(profile)) => {
                if profile.is_empty()
                    || !profile.chars().all(|c| {
                        c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-' || c == '_'
                    })
                {
                    anyhow::bail!("profile names are lowercase [a-z0-9-_]+");
                }
                let home = std::env::var_os("HOME")
                    .map(PathBuf::from)
                    .unwrap_or_default();
                home.join(".local/share/grokbot-control/profiles")
                    .join(profile)
                    .join("state.json")
            }
            (None, None) => default_state_path(),
        };
        if !explicit {
            if let Some(old) = legacy_default_state_path() {
                let _ = migrate_legacy_state(&path, &old);
            }
        }
        let store = Store::load(Some(path.clone()))?;
        Ok((Ctx::new(store, backend), path))
    };

    match &cli.command {
        Some(Command::Doctor) => {
            let (ctx, _) = make_ctx(&cli)?;
            let diag =
                grokbot_control::tools::call_tool(&ctx, "diagnostics", &serde_json::json!({}))
                    .await?;
            println!("{}", serde_json::to_string_pretty(&diag)?);
            return Ok(());
        }
        Some(Command::Run {
            tool,
            args_json,
            compact,
            pairs,
        }) => {
            let (ctx, _) = make_ctx(&cli)?;
            let (payload, ok) =
                grokbot_control::cli::run_tool(&ctx, tool, pairs, args_json.as_deref()).await;
            grokbot_control::cli::print_result(&payload, *compact);
            if !ok {
                std::process::exit(1);
            }
            return Ok(());
        }
        Some(Command::Repl) => {
            let (ctx, _) = make_ctx(&cli)?;
            return grokbot_control::cli::repl(&ctx).await;
        }
        Some(Command::Chat { text }) => {
            let prompt = text.join(" ");
            if prompt.trim().is_empty() {
                anyhow::bail!("usage: grokbot-control chat <prompt...>");
            }
            let (ctx, _) = make_ctx(&cli)?;
            let pairs = vec!["--prompt".to_string(), prompt];
            let (payload, ok) = grokbot_control::cli::run_tool(&ctx, "chat", &pairs, None).await;
            grokbot_control::cli::print_result(&payload, false);
            if !ok {
                std::process::exit(1);
            }
            return Ok(());
        }
        Some(Command::Status) => {
            let (ctx, _) = make_ctx(&cli)?;
            let (payload, ok) =
                grokbot_control::cli::run_tool(&ctx, "diagnostics", &[], None).await;
            grokbot_control::cli::print_result(&payload, false);
            if !ok {
                std::process::exit(1);
            }
            return Ok(());
        }
        _ => {}
    }

    let (ctx, path) = make_ctx(&cli)?;

    eprintln!(
        "grokbot-control v{} · backend={} · state={} · transport={}",
        grokbot_control::VERSION,
        ctx.backend.name(),
        path.display(),
        cli.transport
    );

    // Cron scheduler runs in serve modes (not in one-shot CLI calls).
    {
        let sched_ctx = ctx.clone();
        tokio::spawn(async move {
            grokbot_control::tools::scheduler_loop(sched_ctx).await;
        });
    }
    match cli.transport.as_str() {
        "stdio" => server::serve_stdio(ctx).await,
        "http" | "sse" | "all" => {
            let mode = server::WebMode::parse(&cli.transport)?;
            server::serve_web_oauth(ctx, &cli.http_addr, cli.auth_token, mode, cli.oauth).await
        }
        other => anyhow::bail!("unknown transport '{other}' (expected stdio|http|sse|all)"),
    }
}
