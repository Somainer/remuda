#!/usr/bin/env bash
set -euo pipefail
# shellcheck source=deploy/public/common.sh
source "$(dirname -- "${BASH_SOURCE[0]}")/common.sh"
[[ $# == 1 ]] || die 'Usage: upgrade.sh REGISTRY/IMAGE:RELEASE_TAG'
require_root
load_env
lock_operation
check_docker
new_image=$1
validate_image "$new_image"
# Pull before downtime; a failed pull leaves the running Hub unchanged.
docker pull "$new_image"
create_backup stopped
migration_started=1
cleanup_upgrade() {
  local status=$?
  if (( status != 0 && migration_started )); then
    compose stop --timeout 30 remuda-hub || true
    printf 'Upgrade failed. Hub remains stopped. Restore the encrypted pre-upgrade backup before reverting an image.\n' >&2
  fi
}
trap cleanup_upgrade EXIT
trap 'exit 130' INT
trap 'exit 143' TERM
HUB_IMAGE="$new_image" compose run --rm --no-deps remuda-hub hub --migrate
HUB_IMAGE="$new_image" compose up -d --no-deps --wait --wait-timeout 180 remuda-hub
# Persist the selected tag atomically only after its healthcheck passes.
python3 - "$ENV_FILE" "$new_image" <<'PY'
import os, pathlib, sys, tempfile
path = pathlib.Path(sys.argv[1])
lines = [line for line in path.read_text().splitlines() if not line.startswith('HUB_IMAGE=')]
lines.append('HUB_IMAGE=' + sys.argv[2])
fd, temporary = tempfile.mkstemp(prefix='.env-upgrade-', dir=path.parent)
try:
    with os.fdopen(fd, 'w') as stream:
        stream.write('\n'.join(lines) + '\n')
        stream.flush()
        os.fsync(stream.fileno())
    os.replace(temporary, path)
finally:
    if os.path.exists(temporary): os.unlink(temporary)
PY
migration_started=0
printf 'Upgrade healthy: %s\n' "$new_image"
