# GrokBot Control MCP — Implementation Plan

Date: 2026-11-05 · Status: approved to implement in-session
Settled decisions: GrokBot = xAI Grok API bot · Transport = stdio + HTTP · Backend = mock + optional live xAI

## Goal

A Rust MCP server (`grokbot-control`) that lets any MCP-capable AI client
(Claude, Cursor, custom agents) manage every aspect of a Grok-powered bot:
identity/config, chat, conversations, personas, prompt templates, memory,
documents, scheduled tasks, tools toggles, safety, usage/analytics, backup.

## Non-goals

- No Discord/Telegram/Slack connection (settled: xAI API bot).
- No streaming-token MCP transport in v1 (chat returns complete replies;
  HTTP uses plain JSON responses, stdio uses NDJSON JSON-RPC).
- No database server; state is one JSON file (atomic replace).

## Architecture

```
MCP client ──stdio (NDJSON JSON-RPC 2.0)──┐
                                          ├─► dispatch (src/mcp.rs) ─► tools (src/tools.rs)
MCP client ──HTTP POST /mcp (bearer)──────┘            │
                                                       ├─► State store (src/state.rs) ─► state JSON file
                                                       └─► GrokBackend (src/backend.rs)
                                                             ├─ MockBackend (default, offline, deterministic)
                                                             └─ XaiBackend (reqwest → api.x.ai/v1, needs XAI_API_KEY)
```

Hand-rolled MCP dispatch (no `rmcp` dependency): protocol versions
`2024-11-05` / `2025-03-26`, methods `initialize`, `ping`, `tools/list`,
`tools/call`, `resources/list`, `resources/read`, `prompts/list`,
`prompts/get`, plus `notifications/*` (accepted, no reply).

## Tasks (each maps to a requirement)

| # | Task | Files | Acceptance |
|---|------|-------|------------|
| 1 | Scaffold: deps, CLI (`serve`, `--transport`, `--http-addr`, `--state-file`, `--backend`, `--auth-token`), tracing to stderr | `Cargo.toml`, `src/main.rs`, `src/lib.rs` | `cargo build` clean; `--help` works |
| 2 | State store: `BotConfig`, `Persona`, `Conversation`/`Message`, `PromptTemplate`, `MemoryEntry`, `Document`, `ScheduledTask`, `UsageStats`, request log; load/save JSON, atomic write | `src/state.rs` | Round-trip unit test; missing file → seeded defaults (3 personas, 2 templates) |
| 3 | Backend trait `chat()` + `list_models()`; mock (deterministic, echoes context, fake usage) + live xAI (`/chat/completions`, `/models`, Bearer `XAI_API_KEY`) | `src/backend.rs` | Mock test asserts reply + usage; live path compiles, errors clearly without key |
| 4 | MCP types + dispatch: JSON-RPC envelope, error codes, `initialize` negotiation | `src/mcp.rs` | Unit tests: initialize, ping, unknown method → `-32601` |
| 5 | Tools (28): bot profile/config get+update, models list, provider status/test, chat, chat-in-conversation; conversations CRUD + add/fork/summarize/export; personas CRUD + activate; templates CRUD + render; memory CRUD + search; documents CRUD + search; tasks CRUD + run-due; bot-tools list/toggle; safety get/update; usage, request log, diagnostics, backup export/import, reset | `src/tools.rs` | Every tool in `tools/list` has a handler; integration test calls each happy path |
| 6 | Resources (6: config, personas, conversations, models, usage, log) + Prompts (3: chat-with-persona, summarize, code-review) | `src/tools.rs`, `src/mcp.rs` | `resources/read` + `prompts/get` integration tests |
| 7 | Transports: stdio loop (stdout = protocol only, logs → stderr); axum HTTP `POST /mcp`, `GET /health`, `GET /`; bearer auth when token set | `src/server.rs`, `src/main.rs` | Live smoke: stdio handshake + HTTP curl, 401 without token when set |
| 8 | Tests: unit (state, backend, mcp) + integration (in-process dispatch) | `tests/integration.rs`, inline `#[cfg(test)]` | `cargo test` green; `cargo clippy` clean |
| 9 | README (badges, mermaid, quickstart, client configs, full tool/resource/prompt tables, env table, architecture, roadmap) + `examples/` client configs | `README.md`, `examples/*.json` | README tool table generated from actual `tools/list` output |

## Risks

- xAI live API untestable without a key → mock is the tested path; live code
  kept thin over reqwest and surfaces upstream errors verbatim.
- MCP spec drift → negotiate client version, default `2024-11-05`.
- Stdio stdout pollution → all logging via `tracing` to stderr only.

## Addendum (same day): three transports + expanded management

- Transports are now `stdio` | `http` (Streamable HTTP: `POST /mcp` JSON or
  SSE replies via `Accept: text/event-stream`, `GET /mcp` SSE stream with
  `Mcp-Session-Id`, `DELETE /mcp`) | `sse` (legacy 2024-11-05: `GET /sse`
  endpoint event + `POST /messages?sessionId=`, 15s keepalives) | `all`.
- Tools grew 48 → 78: model get/set + pricing, token_count, usage_cost/reset,
  image_generate (xAI images API), image_understand (vision), chat_batch,
  model_compare, text_summarize/translate, conversation rename/flags/search/
  clear/stats, persona duplicate/export/import, template_duplicate,
  memory export/clear/stats, document_update, task update/enable,
  server_info, state_backup (timestamped file), state_validate,
  request_log_clear. Conversations gained `pinned`/`archived` (serde-default,
  old state files still load).
- Backend trait gained `image_generate` + `vision_chat` (mock + live xAI).
- Tests: 9 (added expanded-tools suite + a real-socket HTTP/SSE suite).
