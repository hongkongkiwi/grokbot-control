#!/usr/bin/env bash
# Legacy SSE transport demo (2024-11-05 HTTP+SSE):
#   grokbot-control --transport sse --http-addr 127.0.0.1:8080 --auth-token dev-secret
# Flow: GET /sse -> "event: endpoint" tells you where to POST ->
# POST /messages?sessionId=<id> (202) -> reply streams back on the GET.
set -euo pipefail
BASE="${GROKBOT_HTTP:-http://127.0.0.1:8080}"
TOKEN="${MCP_AUTH_TOKEN:-dev-secret}"
AUTH="authorization: Bearer ${TOKEN}"

echo "== open SSE stream in background, capture first events =="
OUT="$(mktemp)"
curl -sN "$BASE/sse" -H "$AUTH" >"$OUT" &
CURL_PID=$!
trap 'kill $CURL_PID 2>/dev/null || true; rm -f "$OUT"' EXIT
sleep 1
cat "$OUT"
SESSION="$(grep -o 'sessionId=[^ ]*' "$OUT" | head -1 | cut -d= -f2)"
echo "session: $SESSION"

echo "== POST tools/list to /messages (expect 202) =="
curl -s -o /dev/null -w '%{http_code}\n' -X POST "$BASE/messages?sessionId=$SESSION" \
  -H 'content-type: application/json' -H "$AUTH" \
  -d '{"jsonrpc":"2.0","id":1,"method":"tools/list"}'

sleep 1
echo "== reply that streamed back on the SSE channel =="
grep -a 'event: message' -A1 "$OUT" | head -4 || cat "$OUT"
