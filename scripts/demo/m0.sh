#!/usr/bin/env bash
# M0 demo: build, remuda dev (loopback + access code), fake-claude claude-print,
# journal follow, instance CLI + MCP, hub-embedded web.
#
# Owners of remuda dev / local routes: crates/remuda (codex-astra),
# crates/remuda-node (codex-sol). This script does not patch those crates.
# On a missing command or route it records the exact command + error under
# docs/design/impl-notes.md "## M0 demo gaps" and exits non-zero.
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/../.." && pwd)"
NOTES="$ROOT/docs/design/impl-notes.md"
DEMO_DIR="${REMUDA_M0_DEMO_DIR:-${TMPDIR:-/tmp}/remuda-m0-demo}"
HUB_LISTEN="${REMUDA_M0_HUB_LISTEN:-127.0.0.1:18080}"
NODE_LISTEN="${REMUDA_M0_NODE_LISTEN:-127.0.0.1:18787}"
ACCESS_CODE="${REMUDA_M0_ACCESS_CODE:-m0-demo-access}"
OK_SCRIPT="$ROOT/crates/remuda-testing/fixtures/scripts/ok.jsonl"

REMUDA_PID=""
HUB_PID=""
LOG_DIR=""
REMUDA_BIN=""
FAKE_CLAUDE=""

log() { printf '%s\n' "$*" >&2; }
die() { log "error: $*"; exit 1; }

usage() {
  cat <<'EOF' >&2
Usage: scripts/demo/m0.sh

Builds the workspace, starts remuda dev on loopback with a private access-code
file, drives a claude-print instance via fake-claude (ok.jsonl), follows the
journal over WebSocket, exercises remuda instance + remuda mcp, then serves
the built web UI from the Hub.
EOF
}

while [[ $# -gt 0 ]]; do
  case "$1" in
    -h|--help)
      usage
      exit 0
      ;;
    *)
      usage
      exit 2
      ;;
  esac
done

cleanup() {
  if [[ -n "$REMUDA_PID" ]] && kill -0 "$REMUDA_PID" 2>/dev/null; then
    kill "$REMUDA_PID" 2>/dev/null || true
    wait "$REMUDA_PID" 2>/dev/null || true
  fi
  if [[ -n "$HUB_PID" ]] && kill -0 "$HUB_PID" 2>/dev/null; then
    kill "$HUB_PID" 2>/dev/null || true
    wait "$HUB_PID" 2>/dev/null || true
  fi
}
trap cleanup EXIT

record_gap() {
  local command="$1"
  local error="$2"
  local owner="${3:-crates/remuda (codex-astra), crates/remuda-node (codex-sol)}"
  python3 - "$NOTES" "$command" "$error" "$owner" <<'PY'
import datetime, pathlib, sys
path = pathlib.Path(sys.argv[1])
command, error, owner = sys.argv[2], sys.argv[3], sys.argv[4]
now = datetime.datetime.now(datetime.timezone.utc).strftime("%Y-%m-%dT%H:%M:%SZ")
block = (
    "## M0 demo gaps\n\n"
    "<!-- m0-demo-gaps:start -->\n"
    f"Recorded by `scripts/demo/m0.sh` at {now}. The script stops at the first\n"
    "failing step and does not patch `crates/remuda` or `crates/remuda-node`.\n\n"
    f"**Owner:** {owner}\n\n"
    "**Command:**\n\n"
    "```sh\n"
    f"{command}\n"
    "```\n\n"
    "**Error:**\n\n"
    "```\n"
    f"{error.rstrip()}\n"
    "```\n"
    "<!-- m0-demo-gaps:end -->\n"
)
text = path.read_text() if path.exists() else "# Implementation notes\n"
start = "<!-- m0-demo-gaps:start -->"
end = "<!-- m0-demo-gaps:end -->"
heading = "## M0 demo gaps"
if start in text and end in text:
    before, rest = text.split(start, 1)
    _, after = rest.split(end, 1)
    # keep heading already present before the start marker
    if heading in before:
        before = before[: before.rfind(heading)]
    text = before.rstrip() + "\n\n" + block + after.lstrip("\n")
elif heading in text:
    before, after = text.split(heading, 1)
    # drop the old heading body until the next ## or EOF
    rest = after.split("\n## ", 1)
    tail = ("\n## " + rest[1]) if len(rest) == 2 else ""
    text = before.rstrip() + "\n\n" + block + tail
else:
    text = text.rstrip() + "\n\n" + block
path.write_text(text if text.endswith("\n") else text + "\n")
print(f"wrote {path} ## M0 demo gaps", file=sys.stderr)
PY
}

fail_gap() {
  local command="$1"
  local error="$2"
  local owner="${3:-crates/remuda (codex-astra), crates/remuda-node (codex-sol)}"
  record_gap "$command" "$error" "$owner"
  log "error: M0 demo gap"
  log "command: $command"
  log "owner: $owner"
  log "$error"
  exit 1
}

tail_err() {
  local logf="$1"
  if [[ -f "$logf" ]]; then
    tail -n 40 "$logf"
  else
    printf '(no log)\n'
  fi
}

http_json() {
  local method="$1" url="$2" body="${3:-}"
  python3 - "$method" "$url" "$body" "${TOKEN:-}" "${ACCESS_CODE}" <<'PY'
import json, os, sys, urllib.error, urllib.request
method, url, body, token, access = sys.argv[1:6]
headers = {"Accept": "application/json"}
if body:
    headers["Content-Type"] = "application/json"
if token:
    headers["Authorization"] = "Bearer " + token
if access:
    headers["X-Remuda-Access-Code"] = access
data = body.encode() if body else None
req = urllib.request.Request(url, data=data, method=method, headers=headers)
try:
    with urllib.request.urlopen(req, timeout=8) as resp:
        raw = resp.read().decode()
        print(json.dumps({"status": resp.status, "body": json.loads(raw) if raw.strip() else None}))
except urllib.error.HTTPError as err:
    raw = err.read().decode(errors="replace")
    try:
        parsed = json.loads(raw) if raw.strip() else raw
    except json.JSONDecodeError:
        parsed = raw
    print(json.dumps({"status": err.code, "body": parsed}))
except Exception as err:
    print(json.dumps({"status": 0, "body": str(err)}))
    sys.exit(2)
PY
}

follow_ws() {
  local url="$1" seconds="${2:-3}"
  python3 - "$url" "$seconds" "${TOKEN:-}" "${ACCESS_CODE}" <<'PY'
import base64, hashlib, json, os, select, socket, ssl, struct, sys, urllib.parse
url, seconds, token, access = sys.argv[1], float(sys.argv[2]), sys.argv[3], sys.argv[4]
parsed = urllib.parse.urlparse(url)
assert parsed.scheme in ("ws", "wss"), parsed.scheme
port = parsed.port or (443 if parsed.scheme == "wss" else 80)
path = parsed.path or "/"
if parsed.query:
    path += "?" + parsed.query
key = base64.b64encode(os.urandom(16)).decode()
req = (
    f"GET {path} HTTP/1.1\r\n"
    f"Host: {parsed.hostname}:{port}\r\n"
    "Upgrade: websocket\r\n"
    "Connection: Upgrade\r\n"
    f"Sec-WebSocket-Key: {key}\r\n"
    "Sec-WebSocket-Version: 13\r\n"
)
if token:
    req += f"Authorization: Bearer {token}\r\n"
if access:
    req += f"X-Remuda-Access-Code: {access}\r\n"
req += "\r\n"
sock = socket.create_connection((parsed.hostname, port), timeout=8)
if parsed.scheme == "wss":
    sock = ssl.create_default_context().wrap_socket(sock, server_hostname=parsed.hostname)
sock.sendall(req.encode())
buf = b""
while b"\r\n\r\n" not in buf:
    chunk = sock.recv(4096)
    if not chunk:
        raise SystemExit("websocket handshake closed")
    buf += chunk
head, rest = buf.split(b"\r\n\r\n", 1)
status = head.split(b"\r\n", 1)[0].decode(errors="replace")
if b" 101 " not in head.split(b"\r\n", 1)[0]:
    sys.stderr.write(status + "\n" + head.decode(errors="replace") + "\n")
    raise SystemExit("websocket upgrade failed: " + status)
expect = base64.b64encode(hashlib.sha1((key + "258EAFA5-E914-47DA-95CA-C5AB0DC85B11").encode()).digest()).decode()
if f"sec-websocket-accept: {expect}".encode().lower() not in head.lower():
    raise SystemExit("websocket accept mismatch")

def recv_frames(data):
    out = []
    while True:
        if len(data) < 2:
            return out, data
        b1, b2 = data[0], data[1]
        masked = b2 & 0x80
        ln = b2 & 0x7F
        idx = 2
        if ln == 126:
            if len(data) < 4:
                return out, data
            ln = struct.unpack("!H", data[2:4])[0]
            idx = 4
        elif ln == 127:
            if len(data) < 10:
                return out, data
            ln = struct.unpack("!Q", data[2:10])[0]
            idx = 10
        if masked:
            if len(data) < idx + 4 + ln:
                return out, data
            mask = data[idx:idx + 4]
            idx += 4
            payload = bytes(b ^ mask[i % 4] for i, b in enumerate(data[idx:idx + ln]))
        else:
            if len(data) < idx + ln:
                return out, data
            payload = data[idx:idx + ln]
        data = data[idx + ln:]
        opcode = b1 & 0x0F
        if opcode == 0x1:
            out.append(payload.decode("utf-8", "replace"))
        elif opcode == 0x8:
            return out, b""
    return out, data

import time
deadline = time.time() + seconds
sock.settimeout(0.4)
data = rest
frames = []
while time.time() < deadline:
    try:
        chunk = sock.recv(65536)
    except socket.timeout:
        continue
    except OSError:
        break
    if not chunk:
        break
    data += chunk
    got, data = recv_frames(data)
    frames.extend(got)
sock.close()
for raw in frames:
    try:
        msg = json.loads(raw)
    except json.JSONDecodeError:
        print(json.dumps({"type": "raw", "text": raw}))
        continue
    kind = msg.get("type") or msg.get("method") or "unknown"
    seq = msg.get("seq") or msg.get("asOfSeq") or (msg.get("result") or {}).get("durableSeq")
    event = msg.get("event") or msg.get("params") or msg
    etype = None
    if isinstance(event, dict):
        etype = event.get("type") or (event.get("event") or {}).get("type") if isinstance(event.get("event"), dict) else event.get("type")
    print(json.dumps({"type": kind, "seq": seq, "eventType": etype, "event": event}, sort_keys=True))
if not frames:
    raise SystemExit("no websocket frames received")
PY
}

require_cmd() {
  command -v "$1" >/dev/null 2>&1 || die "missing $1"
}

step_build() {
  log "==> (1) build workspace + fake-claude"
  mkdir -p "$LOG_DIR"
  local logf="$LOG_DIR/build.log"
  if ! (cd "$ROOT" && cargo build --workspace --locked) >"$logf" 2>&1; then
    log "cargo build --workspace --locked failed; retrying without --locked"
    if ! (cd "$ROOT" && cargo build --workspace) >"$logf" 2>&1; then
      fail_gap "cargo build --workspace" "$(tail_err "$logf")" "crates/remuda (codex-astra), crates/remuda-node (codex-sol)"
    fi
  fi
  if ! (cd "$ROOT" && cargo build -p remuda-testing --bin fake-claude) >>"$logf" 2>&1; then
    fail_gap "cargo build -p remuda-testing --bin fake-claude" "$(tail_err "$logf")" "crates/remuda-testing"
  fi
  REMUDA_BIN="$ROOT/target/debug/remuda"
  FAKE_CLAUDE="$ROOT/target/debug/fake-claude"
  [[ -x "$REMUDA_BIN" ]] || fail_gap \
    "cargo build --workspace" \
    "workspace build reported success but $REMUDA_BIN is missing" \
    "crates/remuda (codex-astra)"
  [[ -x "$FAKE_CLAUDE" ]] || fail_gap \
    "cargo build -p remuda-testing --bin fake-claude" \
    "fake-claude binary missing at $FAKE_CLAUDE" \
    "crates/remuda-testing"
}

step_dev() {
  log "==> (2) remuda dev on loopback with access code"
  mkdir -p "$DEMO_DIR/bin" "$DEMO_DIR/data"
  umask 077
  printf '%s\n' "$ACCESS_CODE" >"$DEMO_DIR/access-code"
  chmod 600 "$DEMO_DIR/access-code"
  ln -sfn "$FAKE_CLAUDE" "$DEMO_DIR/bin/claude"
  export PATH="$DEMO_DIR/bin:$PATH"
  export FAKE_CLAUDE_SCRIPT="$OK_SCRIPT"
  export REMUDA_DATA_DIR="$DEMO_DIR/data"
  export REMUDA_COOKIE_SECURE=0
  local logf="$LOG_DIR/dev.log"
  local cmd=(
    "$REMUDA_BIN" --data-dir "$DEMO_DIR/data"
    dev
    --listen "$NODE_LISTEN"
    --hub-listen "$HUB_LISTEN"
    --access-code-file "$DEMO_DIR/access-code"
    --workspace "$ROOT"
  )
  "${cmd[@]}" >"$logf" 2>&1 &
  REMUDA_PID=$!
  local i
  for i in $(seq 1 40); do
    if ! kill -0 "$REMUDA_PID" 2>/dev/null; then
      fail_gap "${cmd[*]}" "$(tail_err "$logf")" "crates/remuda (codex-astra), crates/remuda-node (codex-sol)"
    fi
    if python3 - "$HUB_LISTEN" <<'PY'
import socket, sys
host, port = sys.argv[1].rsplit(":", 1)
s = socket.socket()
s.settimeout(0.2)
try:
    s.connect((host, int(port)))
except OSError:
    sys.exit(1)
finally:
    s.close()
PY
    then
      log "remuda dev listening hub=$HUB_LISTEN node=$NODE_LISTEN pid=$REMUDA_PID"
      return 0
    fi
    sleep 0.15
  done
  fail_gap "${cmd[*]}" "timed out waiting for $HUB_LISTEN"$'\n'"$(tail_err "$logf")" \
    "crates/remuda (codex-astra), crates/remuda-node (codex-sol)"
}

step_login() {
  log "==> login Hub with bootstrap / access code"
  local resp
  resp="$(http_json POST "http://$HUB_LISTEN/v1/login" "{\"bootstrapToken\":\"$ACCESS_CODE\",\"deviceName\":\"m0-demo\"}")" \
    || fail_gap "POST http://$HUB_LISTEN/v1/login" "$resp" "crates/remuda-hub / crates/remuda"
  local status
  status="$(python3 -c 'import json,sys; print(json.load(sys.stdin)["status"])' <<<"$resp")"
  TOKEN="$(python3 -c 'import json,sys; b=json.load(sys.stdin)["body"] or {}; print(b.get("token") or "")' <<<"$resp")"
  if [[ "$status" != "200" || -z "$TOKEN" ]]; then
    fail_gap "POST http://$HUB_LISTEN/v1/login {bootstrapToken}" "HTTP $status $resp" \
      "crates/remuda (codex-astra), crates/remuda-hub"
  fi
  export TOKEN
  export REMUDA_HUB="http://$HUB_LISTEN"
  export REMUDA_TOKEN="$TOKEN"
  export REMUDA_BOOTSTRAP_TOKEN="$ACCESS_CODE"
}

step_create_print() {
  log "==> (3) create claude-print instance (fake-claude / ok.jsonl)"
  [[ -f "$OK_SCRIPT" ]] || die "missing $OK_SCRIPT"
  local logf="$LOG_DIR/instance-create.log"
  local cmd=(
    "$REMUDA_BIN" instance create
    --hub "http://$HUB_LISTEN"
    --token "$TOKEN"
    --kind claude
    --driver claude-print
    --title "m0-demo"
    --prompt "ok"
  )
  if ! "${cmd[@]}" >"$logf" 2>&1; then
    fail_gap "${cmd[*]}" "$(cat "$logf")"$'\n'"PATH=$PATH FAKE_CLAUDE_SCRIPT=$FAKE_CLAUDE_SCRIPT" \
      "crates/remuda (codex-astra), crates/remuda-node (codex-sol)"
  fi
  INSTANCE_ID="$(python3 -c 'import json,sys; d=json.load(open(sys.argv[1])); print((d.get("instance") or {}).get("instanceId") or (d.get("instance") or {}).get("id") or "")' "$logf")"
  if [[ -z "$INSTANCE_ID" ]]; then
    fail_gap "${cmd[*]}" "create succeeded but instanceId missing"$'\n'"$(cat "$logf")" \
      "crates/remuda (codex-astra)"
  fi
  log "instance $INSTANCE_ID"
}

step_follow() {
  log "==> (4) follow journal over WS"
  local url="ws://$HUB_LISTEN/v1/follow?instanceId=$INSTANCE_ID"
  local out="$LOG_DIR/follow.jsonl"
  if ! follow_ws "$url" 4 >"$out"; then
    fail_gap "GET WS $url (Authorization: Bearer <device-token>)" "$(cat "$out" 2>/dev/null || true)"$'\n'"follow produced no frames or handshake failed" \
      "crates/remuda (codex-astra), crates/remuda-hub"
  fi
  log "normalized journal frames:"
  cat "$out" >&2
}

step_cli_mcp() {
  log "==> (5) remuda instance send/wait/read + remuda mcp"
  local logf="$LOG_DIR/instance-cli.log"
  local send=(
    "$REMUDA_BIN" instance send "$INSTANCE_ID"
    --hub "http://$HUB_LISTEN" --token "$TOKEN"
    --text "second turn"
  )
  if ! "${send[@]}" >>"$logf" 2>&1; then
    fail_gap "${send[*]}" "$(tail_err "$logf")" "crates/remuda (codex-astra)"
  fi
  local waitc=(
    "$REMUDA_BIN" instance wait "$INSTANCE_ID"
    --hub "http://$HUB_LISTEN" --token "$TOKEN"
    --timeout-ms 5000
  )
  if ! "${waitc[@]}" >>"$logf" 2>&1; then
    fail_gap "${waitc[*]}" "$(tail_err "$logf")" "crates/remuda (codex-astra)"
  fi
  local readc=(
    "$REMUDA_BIN" instance read "$INSTANCE_ID"
    --hub "http://$HUB_LISTEN" --token "$TOKEN"
    --limit 50
  )
  if ! "${readc[@]}" >>"$logf" 2>&1; then
    fail_gap "${readc[*]}" "$(tail_err "$logf")" "crates/remuda (codex-astra)"
  fi
  local mcp_out="$LOG_DIR/mcp.jsonl"
  if ! {
    printf '%s\n' \
      '{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2024-11-05","capabilities":{},"clientInfo":{"name":"m0-demo","version":"0"}}}' \
      '{"jsonrpc":"2.0","id":2,"method":"tools/list","params":{}}' \
      '{"jsonrpc":"2.0","id":3,"method":"tools/call","params":{"name":"remuda_instance_read","arguments":{"instanceId":"'"$INSTANCE_ID"'","limit":20}}}'
  } | env REMUDA_HUB="http://$HUB_LISTEN" REMUDA_TOKEN="$TOKEN" "$REMUDA_BIN" mcp >"$mcp_out" 2>"$LOG_DIR/mcp.err"; then
    fail_gap "remuda mcp  (initialize, tools/list, tools/call remuda_instance_read)" \
      "$(cat "$LOG_DIR/mcp.err")"$'\n'"$(cat "$mcp_out")" \
      "crates/remuda (codex-astra)"
  fi
  python3 - "$mcp_out" <<'PY' || fail_gap "remuda mcp tools/list + tools/call" "MCP JSON-RPC did not list remuda_instance_read or tools/call failed" "crates/remuda (codex-astra)"
import json, sys
path = sys.argv[1]
lines = [json.loads(l) for l in open(path) if l.strip()]
assert len(lines) >= 3, lines
names = [t.get("name") for t in ((lines[1].get("result") or {}).get("tools") or [])]
assert "remuda_instance_read" in names, names
call = lines[2]
assert call.get("result", {}).get("isError") is not True, call
print("mcp tools:", ", ".join(names), file=sys.stderr)
PY
}

step_web() {
  log "==> (6) build web and serve via Hub embed"
  local logf="$LOG_DIR/web-build.log"
  if [[ ! -d "$ROOT/web/node_modules" ]]; then
    if ! (cd "$ROOT/web" && pnpm install) >"$logf" 2>&1; then
      fail_gap "pnpm --dir web install" "$(tail_err "$logf")" "web"
    fi
  fi
  if ! (cd "$ROOT/web" && pnpm build) >>"$logf" 2>&1; then
    fail_gap "pnpm --dir web build" "$(tail_err "$logf")" "web"
  fi
  [[ -f "$ROOT/web/dist/index.html" ]] || fail_gap "pnpm --dir web build" "web/dist/index.html missing" "web"
  # remuda dev already holds the Hub. Restarting it with --web-root is a remuda
  # flag; if the running Hub does not serve dist, try remuda hub --web-root.
  local page
  page="$(python3 - "$HUB_LISTEN" <<'PY'
import sys, urllib.request
url = "http://%s/" % sys.argv[1]
try:
    with urllib.request.urlopen(url, timeout=5) as resp:
        body = resp.read()[:200]
        print(resp.status, resp.headers.get("content-type", ""), body[:40].decode("utf-8", "replace"))
except Exception as err:
    print("0", str(err))
PY
)"
  if grep -q "text/html" <<<"$page"; then
    log "Hub web UI: http://$HUB_LISTEN/"
    return 0
  fi
  local hub_log="$LOG_DIR/hub-web.log"
  local hub_listen="127.0.0.1:18081"
  "$REMUDA_BIN" hub --listen "$hub_listen" --web-root "$ROOT/web/dist" --data-dir "$DEMO_DIR/hub-web" >"$hub_log" 2>&1 &
  HUB_PID=$!
  sleep 0.4
  if ! kill -0 "$HUB_PID" 2>/dev/null; then
    fail_gap "remuda hub --listen $hub_listen --web-root $ROOT/web/dist" "$(tail_err "$hub_log")" \
      "crates/remuda (codex-astra)"
  fi
  log "Hub web UI: http://$hub_listen/"
}

main() {
  require_cmd cargo
  require_cmd python3
  require_cmd pnpm
  mkdir -p "$DEMO_DIR"
  LOG_DIR="$DEMO_DIR/logs"
  mkdir -p "$LOG_DIR"
  step_build
  step_dev
  step_login
  step_create_print
  step_follow
  step_cli_mcp
  step_web
  log "M0 demo ok"
  log "hub: http://$HUB_LISTEN/"
  log "instance: $INSTANCE_ID"
}

main "$@"
