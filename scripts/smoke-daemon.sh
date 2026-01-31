#!/usr/bin/env bash
#
# End-to-end smoke test for zenui-server.
# Exits non-zero on any failure.
#
# Usage:
#   scripts/smoke-daemon.sh                  # uses target/debug/zenui-server
#   ZENUI_SERVER_BIN=/path/to/zenui-server scripts/smoke-daemon.sh

set -euo pipefail

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$REPO_ROOT"

SERVER_BIN="${ZENUI_SERVER_BIN:-$REPO_ROOT/target/debug/zenui-server}"
if [[ ! -x "$SERVER_BIN" ]]; then
    echo "error: zenui-server binary not found at $SERVER_BIN"
    echo "       build it with: cargo build -p zenui-daemon-bin"
    exit 1
fi

log() { printf '[smoke] %s\n' "$*"; }
fail() { printf '[smoke] FAIL: %s\n' "$*" >&2; exit 1; }

# Fresh state: delete any existing ready file for this project.
log "clearing any stale ready file"
"$SERVER_BIN" stop --project-root "$REPO_ROOT" 2>/dev/null || true
RUNTIME_DIR="${TMPDIR:-/tmp}"
rm -f "$RUNTIME_DIR"/zenui/daemon-*.json 2>/dev/null || true

# Start the daemon in the background (detached mode).
log "starting zenui-server"
"$SERVER_BIN" start --project-root "$REPO_ROOT" --idle-timeout-secs 300 || fail "start command returned non-zero"

# Give the ready file a moment to settle.
for _ in $(seq 1 40); do
    if "$SERVER_BIN" status --project-root "$REPO_ROOT" 2>/dev/null | grep -q http_base; then
        break
    fi
    sleep 0.05
done

# Read back the status.
log "checking status"
STATUS_OUTPUT="$("$SERVER_BIN" status --project-root "$REPO_ROOT")"
echo "$STATUS_OUTPUT" | sed 's/^/    /'
if ! echo "$STATUS_OUTPUT" | grep -q "zenui-server running"; then
    fail "status did not report running daemon"
fi

# Ready file v2: the HTTP transport line looks like
#   [0] kind=http  http_base=http://127.0.0.1:51916  ws_url=ws://.../ws
# Pull out the http_base= field.
HTTP_BASE="$(
    echo "$STATUS_OUTPUT" \
        | grep -E 'kind=http[[:space:]]' \
        | head -n1 \
        | sed -E 's/.*http_base=([^[:space:]]+).*/\1/'
)"
if [[ -z "$HTTP_BASE" ]]; then
    fail "could not parse http_base from status output"
fi
log "daemon listening at $HTTP_BASE"

# Hit the health endpoint.
log "GET $HTTP_BASE/api/health"
HEALTH_BODY="$(curl -fsS --max-time 2 "$HTTP_BASE/api/health")" || fail "health check failed"
echo "$HEALTH_BODY" | grep -q '"status":"ok"' || fail "health check body not ok: $HEALTH_BODY"

# Hit the status endpoint.
log "GET $HTTP_BASE/api/status"
STATUS_BODY="$(curl -fsS --max-time 2 "$HTTP_BASE/api/status")" || fail "status endpoint failed"
echo "$STATUS_BODY" | grep -q '"connected_clients"' || fail "status body missing connected_clients: $STATUS_BODY"

# Non-loopback rejection test is hard to run from loopback-only bound server;
# skip until Phase 4 Windows/cross-binding support lands.

# Ask for graceful shutdown.
log "POST $HTTP_BASE/api/shutdown"
curl -fsS --max-time 5 -X POST "$HTTP_BASE/api/shutdown" >/dev/null || fail "shutdown request rejected"

# Wait for the ready file to disappear (up to 10s).
log "waiting for ready file to be deleted"
for _ in $(seq 1 200); do
    if ! "$SERVER_BIN" status --project-root "$REPO_ROOT" 2>/dev/null | grep -q "running"; then
        log "daemon exited cleanly"
        exit 0
    fi
    sleep 0.05
done
fail "daemon did not delete its ready file within 10s"
