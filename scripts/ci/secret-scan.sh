#!/usr/bin/env bash
# Scan git staged + worktree (or given paths) for undeclared secrets
# and hashed private-token denylist (scripts/ci/private-tokens.sha256).
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/../.." && pwd)"
SCANNER="$ROOT/scripts/ci/secret-scan.py"

if ! command -v python3 >/dev/null 2>&1; then
  echo "secret-scan: python3 is required" >&2
  exit 1
fi

cd "$ROOT"
exec python3 "$SCANNER" "$@"
