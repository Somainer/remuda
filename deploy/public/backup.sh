#!/usr/bin/env bash
set -euo pipefail
# shellcheck source=deploy/public/common.sh
source "$(dirname -- "${BASH_SOURCE[0]}")/common.sh"
[[ $# == 0 ]] || die 'Usage: backup.sh (configure BACKUP_RECIPIENT in .env)'
require_root
load_env
lock_operation
check_docker
create_backup resume
