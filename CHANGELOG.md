# Changelog

## Unreleased — grokbot-control rename + OAuth + audit fixes

- Login retargeted to xAI's native OAuth (`auth.x.ai`, scope
  `grok-cli:access`); OAuth credentials route to the Grok CLI proxy by
  default. "Sign in with X" developer OAuth intentionally not used —
  it authorizes api.x.com (X API), not Grok.

- **Renamed** `grokbot-control-mcp` → `grokbot-control` (crate, binary,
  MCP server name). Legacy state at
  `~/.local/share/grokbot-control-mcp/state.json` is copied to the new
  path on first run; `GROKBOT_*` env vars unchanged.
- **OAuth login**: `login` (browser PKCE + device flow), `logout`,
  `whoami`; tokens at `~/.config/grokbot-control/auth.json` (0600),
  lazy refresh, usable as the xAI credential with API-key fallback.
- **Built-in MCP OAuth server** (`--oauth`): discovery, dynamic
  registration, consent, token (PKCE S256, refresh rotation), scopes
  `read`/`chat`/`admin` enforced per tool.
- **All 20 audit findings fixed** (bind guard, credential-safe base URL,
  single-writer lock, corrupt-state recovery, fsync atomic saves,
  schema v2 with defaults, central safety enforcement, atomic
  conversation turns, config validation, cron that fires, bounded SSE
  sessions, input/spend caps, protocol envelope validation, per-model
  cost accounting, loud backend failures, duplicate-name rules, backup
  rotation/restore, and more).
- **Features M1–M10**: streaming chat + async jobs with stop, xAI Files
  + embeddings + semantic search, structured output + operator bot
  tools, quota/budget gates, message edit/delete + history windowing,
  audio in/out, persona versioning + A/B, backup restore, notifications
  groundwork (logging/completion), multi-bot registry + profiles.
- **Inspired by the ecosystem** (patterns, no code): routines/bots/jobs
  from TheSethRose/grokbot-bridge, PKCE login from Kenzim/grokbot-mcp,
  video/search/files from merterbak/Grok-MCP.
- Tools: 78 → 112. Tests: 23 passing.

## 0.1.0

- Initial release as grokbot-control-mcp: Rust MCP server (stdio, HTTP,
  SSE) + CLI + skill for managing a GrokBot over the xAI API.
  78 tools, 6 resources, 3 prompts; mock + live backends.
