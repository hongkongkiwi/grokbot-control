---
name: grokbot-control
description: Manage GrokBot (xAI Grok bot) — chat, conversations, personas, templates, memory, documents, tasks, safety, usage — via the grokbot-control CLI or MCP tools.
---

# GrokBot Control

Use this skill whenever you need to operate GrokBot: talk to it, retune it,
manage its conversations/personas/memory/knowledge/tasks, check its health,
or back it up. There are two equivalent ways in. Pick the first one that is
available; they share one engine and one state file, so they never disagree.

## Way 1 — MCP tools (preferred when connected)

If tools named `chat`, `bot_get_profile`, `conversation_*`, `persona_*`,
`memory_*`, `diagnostics`, etc. are in your tool list, call them directly.
Start with `diagnostics` (health) and `bot_get_profile` (current config).

## Way 2 — CLI (always works from a shell)

Binary: `grokbot-control` (build with `cargo build --release` in the
project; the binary lands in `target/release/`). Every MCP tool is a CLI
command — arguments are validated and coerced from the tool's own schema:

```bash
grokbot-control tools --plain                 # list all 78 tools
grokbot-control run chat --prompt "hello" --temperature 0.2
grokbot-control run conversation-create --title "Release help"
grokbot-control run conversation_send --id <id> --content "hi"
grokbot-control run persona_list
grokbot-control run memory_store --key owner --value Andy --tags person,admin
grokbot-control run memory_recall --args '{"key":"owner"}'
grokbot-control chat tell me a joke           # shortcut: one-shot chat
grokbot-control status                        # shortcut: diagnostics
grokbot-control repl                          # interactive shell
```

Rules for the CLI:

- Tool names accept hyphens or underscores (`conversation-send` = `conversation_send`).
- Types come from the schema: `--temperature 0.2` is a number,
  `--tags a,b` is a list, `--enabled` / `--enabled=false` is a boolean,
  `--variables '{"name":"Ada"}'`-style JSON works for objects/arrays,
  and `--args '{...}'` passes a whole JSON argument object.
- Output is JSON on stdout. A failed call prints `{"ok": false, "error": ...}`
  plus a `usage:` line and exits 1 — read the error, fix the argument, retry.
- State flags come before the subcommand:
  `grokbot-control --state-file /path/state.json --backend mock run chat --prompt hi`

## Choosing a backend

- Default `auto`: live xAI when `XAI_API_KEY` is set, otherwise the offline
  mock (deterministic, free, replies are labelled `[mock grok · …]`).
- Never claim a reply is real Grok output when the backend is `mock`.
  Check with `run bot_provider_status` or `status` if unsure.
- `--backend live` fails fast without a key; `--backend mock` never networks.

## Operating playbook

1. **Orient:** `diagnostics` → `bot_get_profile` → `persona_list`.
2. **Talk:** `chat` for one-shot; `conversation_create` + `conversation_send`
   for multi-turn (history is kept server-side; use the returned `id`).
3. **Retune:** `persona_activate`, `bot_update_config` (model, temperature,
   system prompt), `bot_tool_set_enabled`, `bot_safety_update`.
4. **Teach:** `memory_store`, `document_add` (+ `document_search` to verify).
5. **Schedule:** `task_create` with a cron string, `task_run` to execute now.
6. **Observe:** `usage_stats`, `usage_cost`, `request_log`, `conversation_stats`.
7. **Protect:** `state_export` before destructive work; `state_backup`
   writes a timestamped file; `state_reset`/`state_import`/`memory_clear`
   need `--confirm true` semantics (`confirm: true`) — never run them unasked.

## Guardrails

- Do not invent conversation/persona IDs — list first, then act on real IDs.
- Personas and templates also accept names (`--id Coder`), conversations do not.
- Do not expose or store API keys: the key comes from the environment only.
- If a tool errors with a blocked-topic message, safety settings are doing
  their job — report it instead of routing around it.
