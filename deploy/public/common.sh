#!/usr/bin/env bash
# Shared implementation; invoked by the public VPS operator scripts.
set -euo pipefail
umask 077
PUBLIC_DIR="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)"
ENV_FILE="${REMUDA_ENV_FILE:-$PUBLIC_DIR/.env}"

die() { printf 'remuda-public: %s\n' "$*" >&2; exit 1; }
need() { command -v "$1" >/dev/null || die "Required command missing: $1"; }
require_root() { [[ $EUID == 0 ]] || die 'Run this script with sudo on the VPS.'; }

load_env() {
  [[ -f $ENV_FILE && ! -L $ENV_FILE ]] || die 'Copy .env.example to .env and configure it first.'
  local line key value seen='|'
  # Never source .env: shell expansion and Compose interpolation are forbidden.
  while IFS= read -r line || [[ -n $line ]]; do
    [[ -z $line || $line == \#* ]] && continue
    [[ $line == *=* ]] || die 'Expected literal KEY=VALUE in .env.'
    key=${line%%=*}; value=${line#*=}
    case "$key" in
      HUB_DOMAIN|ACME_EMAIL|HUB_IMAGE|CADDY_IMAGE|COMPOSE_PROJECT_NAME|DATA_DIR|BACKUP_DIR|BACKUP_RECIPIENT|VPS_PUBLIC_IP|PROXY_SUBNET|PROXY_IP) ;;
      *) die "Unsupported .env key: $key" ;;
    esac
    [[ $seen != *"|$key|"* ]] || die "Duplicate .env key: $key"
    seen+="$key|"
    [[ ! $value =~ [^a-zA-Z0-9_@%+.,:/=-] ]] || die "Use literal unquoted values and replace placeholders in $key."
    export "$key=$value"
  done < "$ENV_FILE"
  : "${HUB_DOMAIN:?Set HUB_DOMAIN}" "${ACME_EMAIL:?Set ACME_EMAIL}" "${HUB_IMAGE:?Set HUB_IMAGE}"
  export DATA_DIR="${DATA_DIR:-/var/lib/remuda-public}" BACKUP_DIR="${BACKUP_DIR:-/var/backups/remuda-public}"
  export COMPOSE_PROJECT_NAME="${COMPOSE_PROJECT_NAME:-remuda-public}"
  export PROXY_SUBNET="${PROXY_SUBNET:-172.30.19.0/24}" PROXY_IP="${PROXY_IP:-172.30.19.2}"
  export CADDY_IMAGE="${CADDY_IMAGE:-caddy:2-alpine}"
  [[ $COMPOSE_PROJECT_NAME =~ ^[a-z0-9][a-z0-9_-]*$ ]] || die 'Invalid COMPOSE_PROJECT_NAME.'
  [[ $HUB_DOMAIN =~ ^[a-zA-Z0-9]([a-zA-Z0-9.-]*[a-zA-Z0-9])?$ && $HUB_DOMAIN == *.* ]] || die 'HUB_DOMAIN must be a DNS hostname without scheme or port.'
  [[ $ACME_EMAIL == *@*.* ]] || die 'Set a valid ACME_EMAIL.'
  validate_image "$HUB_IMAGE"
  # Refuse dangerous/shared roots, path traversal and symlinked data locations.
  need python3
  python3 - "$DATA_DIR" "$BACKUP_DIR" "$PROXY_SUBNET" "$PROXY_IP" <<'PY'
import ipaddress, pathlib, sys
data, backup = map(pathlib.Path, sys.argv[1:3])
for path in (data, backup):
    if not path.is_absolute() or len(path.parts) < 4 or path.resolve() != path:
        sys.exit('Use canonical absolute DATA_DIR/BACKUP_DIR paths with a dedicated leaf directory.')
if data == backup or data in backup.parents or backup in data.parents:
    sys.exit('DATA_DIR and BACKUP_DIR must be separate directory trees.')
network = ipaddress.IPv4Network(sys.argv[3])
proxy = ipaddress.IPv4Address(sys.argv[4])
if not network.is_private or proxy not in network or proxy in (network.network_address, network.broadcast_address):
    sys.exit('PROXY_IP must be a usable address in a private PROXY_SUBNET.')
PY
}

validate_image() {
  [[ $1 =~ ^[a-zA-Z0-9][a-zA-Z0-9._:/-]*:[a-zA-Z0-9_][a-zA-Z0-9_.-]*$ && ${1##*:} != latest ]] || die 'Use an explicit image release tag, never latest.'
}

compose() {
  docker compose --project-name "$COMPOSE_PROJECT_NAME" --env-file "$ENV_FILE" \
    -f "$PUBLIC_DIR/docker-compose.yml" "$@"
}

lock_operation() {
  need flock
  exec 9>"/run/lock/$COMPOSE_PROJECT_NAME.lock"
  flock -n 9 || die 'Another install, backup or upgrade is running.'
}

check_docker() {
  need docker
  docker info >/dev/null
  docker compose version >/dev/null
  compose config --quiet
}

check_volume() {
  local device
  device=$(docker volume inspect "${COMPOSE_PROJECT_NAME}_hub_data" --format '{{index .Options "device"}}')
  [[ $device == "$DATA_DIR" ]] || die 'Existing hub_data volume uses a different DATA_DIR; restore/migrate explicitly.'
}

create_backup() (
  # Subshell owns cleanup. Success with "stopped" leaves Hub down for migration.
  local mode=${1:-resume} stage='' partial='' was_running=0 complete=0
  need age
  [[ ${BACKUP_RECIPIENT:-} =~ ^age1[0-9a-z]+$ ]] || die 'Set BACKUP_RECIPIENT to an age public recipient; keep its identity off the VPS.'
  [[ -f $DATA_DIR/hub.sqlite && -f $DATA_DIR/bootstrap-token && -f $DATA_DIR/secrets/master.key && -f $DATA_DIR/secrets/secrets.json ]] || die 'Hub data is incomplete; refusing backup.'
  check_volume
  install -d -m 0700 "$BACKUP_DIR"
  stage=$(mktemp -d "$BACKUP_DIR/.work.XXXXXXXX")
  partial="$stage/archive.tar.gz.age"
  # shellcheck disable=SC2329 # Invoked by the EXIT trap in this subshell.
  cleanup_backup() {
    local status=$?
    trap - EXIT
    rm -rf -- "$stage"
    if (( was_running )) && { (( ! complete )) || [[ $mode == resume ]]; }; then
      compose start --wait --wait-timeout 120 remuda-hub || status=1
    fi
    exit "$status"
  }
  trap cleanup_backup EXIT
  trap 'exit 130' INT
  trap 'exit 143' TERM
  if [[ -n $(compose ps --status running -q remuda-hub) ]]; then was_running=1; fi
  compose stop --timeout 30 remuda-hub
  mkdir -m 0700 "$stage/data"
  # Snapshot DB through SQLite's backup API (also incorporates recovered WAL).
  python3 - "$DATA_DIR/hub.sqlite" "$stage/data/hub.sqlite" <<'PY'
import pathlib, sqlite3, sys
with sqlite3.connect(pathlib.Path(sys.argv[1]).as_uri() + '?mode=ro', uri=True) as source:
    with sqlite3.connect(sys.argv[2]) as target:
        source.backup(target)
        if target.execute('PRAGMA quick_check').fetchall() != [('ok',)]:
            sys.exit('SQLite backup integrity check failed.')
PY
  # Preserve secrets/master.key AND secrets/secrets.json, bootstrap and push state.
  tar -C "$DATA_DIR" --exclude='./hub.sqlite' --exclude='./hub.sqlite-wal' --exclude='./hub.sqlite-shm' -cf - . |
    tar -C "$stage/data" -xf -
  cp -- "$ENV_FILE" "$stage/deployment.env"
  tar -C "$stage" -czf - data deployment.env |
    age --recipient "$BACKUP_RECIPIENT" --output "$partial"
  local destination
  destination="$BACKUP_DIR/hub-$(date -u +%Y%m%dT%H%M%SZ)-${stage##*.}.tar.gz.age"
  mv -- "$partial" "$destination"
  complete=1
  printf 'Encrypted backup: %s\n' "$destination"
)
