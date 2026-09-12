#!/usr/bin/env bash
# Read-only M1 preflight against Hub host and SG Node over SSH.
# Never starts, stops, or rewrites remote services.
set -euo pipefail

HUB_SSH="${REMUDA_M1_HUB_SSH:-devbox-sg-host}"
NODE_SSH="${REMUDA_M1_NODE_SSH:-devbox-sg}"
SSH_OPTS=(-o BatchMode=yes -o ConnectTimeout=8 -o ConnectionAttempts=1)

log() { printf '%s\n' "$*" >&2; }

ssh_ro() {
  local host="$1"
  shift
  ssh "${SSH_OPTS[@]}" "$host" "$@"
}

redact() {
  # Collapse RFC1918 last octets and keep SSH aliases, not FQDNs.
  sed -E \
    -e 's/\b(10\.[0-9]+)\.[0-9]+\.[0-9]+\b/\1.*.*/g' \
    -e 's/\b(192\.168)\.[0-9]+\.[0-9]+\b/\1.*.*/g' \
    -e 's/\b(172\.(1[6-9]|2[0-9]|3[0-1]))\.[0-9]+\.[0-9]+\b/\1.*.*/g'
}

check() {
  local name="$1"
  local status="$2"
  local detail="$3"
  printf '| %s | %s | %s |\n' "$name" "$status" "$detail"
  if [[ "$status" == "NO-GO" ]]; then
    FAIL=1
  fi
}

FAIL=0
DETAILS=()

probe_host() {
  local alias="$1"
  local out
  if out=$(ssh_ro "$alias" 'echo ok' 2>&1); then
    check "ssh $alias" GO "BatchMode ok"
  else
    check "ssh $alias" NO-GO "$(printf '%s' "$out" | redact | tr '\n' ' ' | cut -c1-80)"
    return 1
  fi
}

echo "| Check | Result | Detail |"
echo "| --- | --- | --- |"

if probe_host "$HUB_SSH"; then
  HUB_OS=$(ssh_ro "$HUB_SSH" 'uname -srm; . /etc/os-release 2>/dev/null; echo "$PRETTY_NAME"' 2>/dev/null | redact | tr '\n' '; ')
  check "hub os" GO "$HUB_OS"

  if ssh_ro "$HUB_SSH" 'command -v docker >/dev/null'; then
    DV=$(ssh_ro "$HUB_SSH" 'docker version --format "{{.Server.Version}}" 2>/dev/null || docker version | head -2')
    check "hub docker" GO "$(printf '%s' "$DV" | redact | tr '\n' ' ')"
  else
    check "hub docker" NO-GO "docker not on PATH"
  fi

  COMPOSE_OK=0
  if ssh_ro "$HUB_SSH" 'docker compose version >/dev/null 2>&1'; then
    CV=$(ssh_ro "$HUB_SSH" 'docker compose version')
    check "hub compose" GO "$(printf '%s' "$CV" | tr '\n' ' ')"
    COMPOSE_OK=1
  elif ssh_ro "$HUB_SSH" 'command -v docker-compose >/dev/null'; then
    CV=$(ssh_ro "$HUB_SSH" 'docker-compose version')
    check "hub compose" GO "$(printf '%s' "$CV" | tr '\n' ' ')"
    COMPOSE_OK=1
  else
    check "hub compose" NO-GO "docker compose not found"
  fi

  NET=$(ssh_ro "$HUB_SSH" 'docker network inspect deploy_default --format "{{.Name}} {{.Driver}} {{.Scope}}" 2>/dev/null || true')
  if [[ "$NET" == deploy_default* ]]; then
    check "network deploy_default" GO "$NET"
  else
    LS=$(ssh_ro "$HUB_SSH" 'docker network ls --format "{{.Name}}" 2>/dev/null | tr "\n" " "')
    check "network deploy_default" NO-GO "missing; networks: $LS"
  fi

  CADDY=$(ssh_ro "$HUB_SSH" 'docker ps --format "{{.Names}}" 2>/dev/null | grep -E "^deploy-caddy" || true')
  if [[ -n "$CADDY" ]]; then
    check "caddy container" GO "$CADDY"
  else
    check "caddy container" NO-GO "no deploy-caddy* running"
  fi

  CF=$(ssh_ro "$HUB_SSH" 'test -f ~/astergate/deploy/Caddyfile && echo yes || echo no')
  if [[ "$CF" == yes ]]; then
    IMP=$(ssh_ro "$HUB_SSH" 'grep -E "^[[:space:]]*import " ~/astergate/deploy/Caddyfile 2>/dev/null | head -3 || true' | redact)
    if printf '%s' "$IMP" | grep -q import; then
      check "caddy import" GO "$(printf '%s' "$IMP" | tr '\n' '; ')"
    else
      check "caddy import" GO "Caddyfile present; add import Caddyfile.d/*.caddy (one line)"
    fi
  else
    check "caddy import" NO-GO "~/astergate/deploy/Caddyfile missing"
  fi

  LISTEN=$(ssh_ro "$HUB_SSH" 'ss -lnt 2>/dev/null || netstat -lnt 2>/dev/null' | redact)
  if printf '%s' "$LISTEN" | grep -Eq ':443[[:space:]]'; then
    check "hub :443" GO "listener present (do not bind another)"
  else
    check "hub :443" NO-GO "no process listening on 443"
  fi
  if printf '%s' "$LISTEN" | grep -Eq ':8080[[:space:]]'; then
    check "hub :8080 host" GO "host already has :8080; Hub must stay unpublished (compose has no ports:)"
  else
    check "hub :8080 host" GO "host :8080 free; Hub still must not publish ports"
  fi

  DATA=$(ssh_ro "$HUB_SSH" 'df -h /data00 2>/dev/null | tail -1 || df -h / | tail -1' | redact)
  check "hub disk" GO "$DATA"

  if ssh_ro "$HUB_SSH" 'command -v cloudflared >/dev/null || command -v /usr/local/bin/cloudflared >/dev/null'; then
    check "hub cloudflared bin" GO "binary present"
  else
    check "hub cloudflared bin" GO "not installed yet (token-based install is in README)"
  fi

  EXISTING=$(ssh_ro "$HUB_SSH" 'docker ps -a --format "{{.Names}}" 2>/dev/null | grep -E "remuda" || true')
  if [[ -n "$EXISTING" ]]; then
    check "hub remuda containers" GO "already present (preflight will not start/stop): $EXISTING"
  else
    check "hub remuda containers" GO "none (compose up is operator-run, not this script)"
  fi
fi

if probe_host "$NODE_SSH"; then
  NODE_OS=$(ssh_ro "$NODE_SSH" 'uname -srm; . /etc/os-release 2>/dev/null; echo "$PRETTY_NAME"' 2>/dev/null | redact | tr '\n' '; ')
  check "node os" GO "$NODE_OS"
  SYS=$(ssh_ro "$NODE_SSH" 'command -v systemctl >/dev/null && echo systemd || echo no-systemd; ps -p 1 -o comm=' 2>/dev/null)
  check "node pid1" GO "$SYS"
  TMP=$(ssh_ro "$NODE_SSH" 'df -h /tmp | tail -1' | redact)
  check "node /tmp" GO "$TMP"
  if ssh_ro "$NODE_SSH" 'test -x /opt/remuda/remuda && echo yes || echo no' | grep -q yes; then
    check "node /opt/remuda" GO "binary already present (preflight will not overwrite)"
  else
    check "node /opt/remuda" GO "empty; scp step is operator-run"
  fi
fi

echo
if [[ "$FAIL" -eq 0 ]]; then
  echo "PREFLIGHT: GO"
  exit 0
fi
echo "PREFLIGHT: NO-GO"
exit 1
