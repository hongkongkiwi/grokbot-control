# Security

## Reporting

Open a private security advisory on GitHub, or contact the maintainer
via the repository. Please do not file public issues for credential or
auth-bypass problems.

## Security model

- **Credentials stay out of state.** xAI API keys come from the
  environment only. OAuth tokens live in
  `~/.config/grokbot-control/auth.json` with `0600` permissions (dir
  `0700`), never in bot state, never printed, and are excluded from
  `state_export`/`state_backup` by construction (separate file).
- **Bot state** is written `0600` via fsynced atomic rename; backups
  under `backups/` inherit the same handling.
- **Network exposure:** the server refuses non-loopback binds without
  an auth token or OAuth enabled. Static tokens are compared in
  constant time. OAuth access tokens are stored server-side only as
  SHA-256 hashes, expire in 1 hour, and authorization codes are
  single-use with PKCE S256 enforced.
- **Scopes:** `read` < `chat` < `admin`; destructive tools
  (`state_import`, `state_reset`, `state_restore`, `usage_reset`,
  `memory_clear`, …) require `admin`.
- **Outbound safety:** `XAI_BASE_URL` must be https (http only for
  localhost) with no embedded credentials, so keys/tokens cannot be
  redirected to a plaintext or lookalike endpoint. Image URLs are
  validated the same way, and link-local metadata addresses are refused.
- **Spend caps:** image/batch/compare fan-outs are capped, and an
  optional daily USD budget gate (`daily_budget_usd`) stops backend
  calls when the estimated spend is reached.

## Known limitations

- The single approver-token consent flow for the built-in OAuth server
  is designed for a single operator, not multi-tenant hosting.
- OAuth tokens issued by consumer Grok login may be rejected by
  api.x.ai (which officially documents API keys); the server surfaces
  this as an explicit 401 diagnosis rather than failing silently.
