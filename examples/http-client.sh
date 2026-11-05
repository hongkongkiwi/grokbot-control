#!/usr/bin/env bash
# Talk to a running HTTP instance:
#   grokbot-control --transport http --http-addr 127.0.0.1:8080 --auth-token dev-secret
set -euo pipefail
BASE="${GROKBOT_HTTP:-http://127.0.0.1:8080}"
TOKEN="${MCP_AUTH_TOKEN:-dev-secret}"

echo "== health =="
curl -s "$BASE/health"; echo

echo "== initialize =="
curl -s -X POST "$BASE/mcp" -H 'content-type: application/json' \
  -H "authorization: Bearer ${TOKEN}" \
  -d '{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2024-11-05"}}'; echo

echo "== chat =="
curl -s -X POST "$BASE/mcp" -H 'content-type: application/json' \
  -H "authorization: Bearer ${TOKEN}" \
  -d '{"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"chat","arguments":{"prompt":"Hello GrokBot, introduce yourself"}}}'; echo

echo "== diagnostics =="
curl -s -X POST "$BASE/mcp" -H 'content-type: application/json' \
  -H "authorization: Bearer ${TOKEN}" \
  -d '{"jsonrpc":"2.0","id":3,"method":"tools/call","params":{"name":"diagnostics","arguments":{}}}'; echo
