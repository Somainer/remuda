#!/usr/bin/env bash
# M0 acceptance skeleton (plan M0-15 / A-017).
# stub: drive fake-claude through claude-print + remuda-journal for all four scripts.
# live: print the canary commands and prerequisites; never call a model.
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/../.." && pwd)"
MODE=""
HERDR_SESSION="remuda-test"
CONFIRM_EXTERNAL=0

usage() {
  cat <<'EOF' >&2
Usage: m0.sh --mode stub|live [--herdr-session remuda-test] [--confirm-external-calls]

  --mode stub   claude-print + journal for ok/approval/askuser/workflow; leak + secret gates
  --mode live   print the live canary plan; do not invoke Claude/Herdr/models
EOF
}

log() { printf '%s\n' "$*" >&2; }
die() { log "error: $*"; exit 1; }

iso_now() {
  python3 -c 'from datetime import datetime, timezone; print(datetime.now(timezone.utc).strftime("%Y-%m-%dT%H:%M:%SZ"))'
}

while [[ $# -gt 0 ]]; do
  case "$1" in
    --mode)
      [[ $# -ge 2 ]] || die "--mode needs stub|live"
      MODE="$2"
      shift 2
      ;;
    --mode=*)
      MODE="${1#--mode=}"
      shift
      ;;
    --herdr-session)
      [[ $# -ge 2 ]] || die "--herdr-session needs a value"
      HERDR_SESSION="$2"
      shift 2
      ;;
    --herdr-session=*)
      HERDR_SESSION="${1#--herdr-session=}"
      shift
      ;;
    --confirm-external-calls)
      CONFIRM_EXTERNAL=1
      shift
      ;;
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

[[ "$MODE" == "stub" || "$MODE" == "live" ]] || { usage; exit 2; }
[[ -n "$HERDR_SESSION" ]] || die "herdr session is empty"
if [[ "$HERDR_SESSION" == "default" ]]; then
  die "refusing herdr session default"
fi

STARTED="$(iso_now)"
CASE_ID="A-017-m0-${MODE}"
HERDR_SH="$ROOT/scripts/acceptance/herdr-isolated.sh"
SCAN="$ROOT/scripts/ci/secret-scan.sh"

target_dir() {
  local dir="${CARGO_TARGET_DIR:-${CARGO_BUILD_TARGET_DIR:-}}"
  if [[ -n "$dir" ]]; then
    if [[ "$dir" = /* ]]; then
      printf '%s\n' "$dir"
    else
      printf '%s\n' "$ROOT/$dir"
    fi
  else
    printf '%s\n' "$ROOT/target"
  fi
}

live_plan() {
  cat <<EOF >&2
M0 live canary is not executed by this script.
Prerequisites (all required before an operator re-runs with a future live runner):
  - --mode live --confirm-external-calls (this invocation confirm=$(
    [[ "$CONFIRM_EXTERNAL" -eq 1 ]] && echo yes || echo NO
  ))
  - LIVE_CANARY=1
  - claude on PATH; --model haiku --max-budget-usd 0.3
  - isolated cwd /tmp/remuda-m0/ (0700); do not use the user Claude home
  - herdr on PATH; dedicated session ${HERDR_SESSION} (never default)
  - no tools / no bot / TD-M0-PERM-01 visible
Commands that a live runner would execute (not run now):
  ${HERDR_SH} start --session ${HERDR_SESSION}
  cargo test --workspace --locked
  cargo test -p remuda-herdr --test herdr_isolated -- --ignored --nocapture
  remuda dev
  claude -p --model haiku --max-budget-usd 0.3 --permission-mode default \\
    --permission-prompts host --permission-prompt-tool stdio
  ${HERDR_SH} stop --session ${HERDR_SESSION}
  ${SCAN}
EOF
  local socket
  socket="$("$HERDR_SH" start --session "$HERDR_SESSION" --fixture-only)"
  REMUDA_M0_SOCKET="$socket" REMUDA_M0_CASE="$CASE_ID" REMUDA_M0_STARTED="$STARTED" \
    REMUDA_M0_CONFIRM="$CONFIRM_EXTERNAL" REMUDA_M0_SESSION="$HERDR_SESSION" \
    python3 - <<'PY'
import json, os
from datetime import datetime, timezone
print(json.dumps({
  "schemaVersion": 1,
  "caseId": os.environ["REMUDA_M0_CASE"],
  "startedAt": os.environ["REMUDA_M0_STARTED"],
  "finishedAt": datetime.now(timezone.utc).strftime("%Y-%m-%dT%H:%M:%SZ"),
  "result": "skipped",
  "observedVersions": {
    "mode": "live",
    "herdrSession": os.environ["REMUDA_M0_SESSION"],
    "herdrSocket": os.environ["REMUDA_M0_SOCKET"],
    "confirmExternalCalls": os.environ["REMUDA_M0_CONFIRM"] == "1",
  },
  "artifactRefs": [],
  "failure": None,
}, indent=2, sort_keys=True))
print()
PY
}

fake_claude_pids() {
  pgrep -x fake-claude 2>/dev/null || true
}

pid_set() {
  printf '%s\n' "$@" | awk 'NF && !seen[$0]++' | sort -n
}

stub_run() {
  command -v python3 >/dev/null 2>&1 || die "python3 is required"
  command -v cargo >/dev/null 2>&1 || die "cargo is required"
  [[ -f "$SCAN" ]] || die "missing $SCAN"

  local leftover bin stub status=0 before_s after_s target
  # Global: EXIT trap runs after `local` vars in this function are gone (`set -u`).
  M0_WORKDIR="$(mktemp -d "${TMPDIR:-/tmp}/remuda-m0.XXXXXX")"
  cleanup() {
    if [[ -n "${M0_WORKDIR:-}" && -d "$M0_WORKDIR" ]]; then
      rm -rf "$M0_WORKDIR"
    fi
  }
  trap cleanup EXIT

  target="$(target_dir)"
  log "building fake-claude and m0-print-stub into $target"
  if ! (cd "$ROOT" && cargo build -p remuda-testing --bin fake-claude --bin m0-print-stub --target-dir "$target" --locked) >&2; then
    log "Cargo.lock not in sync; rebuilding without --locked"
    (cd "$ROOT" && cargo build -p remuda-testing --bin fake-claude --bin m0-print-stub --target-dir "$target") >&2
  fi
  bin="$target/debug/fake-claude"
  stub="$target/debug/m0-print-stub"
  [[ -x "$bin" ]] || die "fake-claude binary missing at $bin"
  [[ -x "$stub" ]] || die "m0-print-stub binary missing at $stub"

  before_s="$(pid_set $(fake_claude_pids))"

  local script
  mkdir -p "$M0_WORKDIR/logs"
  for script in ok approval askuser workflow; do
    log "script $script (claude-print + journal)"
    mkdir -p "$M0_WORKDIR/$script"
    "$stub" \
      --script "$script" \
      --fake-claude "$bin" \
      --workdir "$M0_WORKDIR/$script" \
      >&2
  done

  sleep 0.2
  after_s="$(pid_set $(fake_claude_pids))"
  leftover="$(comm -13 <(printf '%s\n' "$before_s") <(printf '%s\n' "$after_s") | awk 'NF')"
  if [[ -n "$leftover" ]]; then
    log "fake-claude leak pids: $leftover"
    # shellcheck disable=SC2086
    kill $leftover 2>/dev/null || true
    status=1
  fi

  log "secret-scan artifacts"
  if ! "$SCAN" "$M0_WORKDIR"; then
    status=1
  fi

  local socket
  socket="$("$HERDR_SH" start --session "$HERDR_SESSION" --fixture-only)"

  rm -rf "$M0_WORKDIR"
  trap - EXIT
  if [[ -e "$M0_WORKDIR" ]]; then
    log "temp dir survived cleanup: $M0_WORKDIR"
    status=1
  fi

  local result="pass"
  local failure=""
  if [[ "$status" -ne 0 ]]; then
    result="fail"
    failure="stub gate failed (leak, secret-scan, or script)"
  fi
  local rustc
  rustc="$(rustc -V 2>/dev/null || echo unknown)"
  REMUDA_M0_BIN="$bin" REMUDA_M0_SOCKET="$socket" REMUDA_M0_RUSTC="$rustc" \
    REMUDA_M0_RESULT="$result" REMUDA_M0_FAILURE="$failure" \
    REMUDA_M0_CASE="$CASE_ID" REMUDA_M0_STARTED="$STARTED" \
    REMUDA_M0_SESSION="$HERDR_SESSION" \
    python3 - <<'PY'
import hashlib, json, os
from datetime import datetime, timezone
bin_path = os.environ["REMUDA_M0_BIN"]
digest = ""
try:
    h = hashlib.sha256()
    with open(bin_path, "rb") as fh:
        for chunk in iter(lambda: fh.read(1 << 16), b""):
            h.update(chunk)
    digest = h.hexdigest()
except OSError:
    pass
failure = os.environ.get("REMUDA_M0_FAILURE") or None
print(json.dumps({
  "schemaVersion": 1,
  "caseId": os.environ["REMUDA_M0_CASE"],
  "startedAt": os.environ["REMUDA_M0_STARTED"],
  "finishedAt": datetime.now(timezone.utc).strftime("%Y-%m-%dT%H:%M:%SZ"),
  "result": os.environ["REMUDA_M0_RESULT"],
  "observedVersions": {
    "mode": "stub",
    "herdrSession": os.environ["REMUDA_M0_SESSION"],
    "herdrSocket": os.environ["REMUDA_M0_SOCKET"],
    "rustc": os.environ["REMUDA_M0_RUSTC"],
    "fakeClaude": bin_path,
    "fakeClaudeSha256": digest,
    "scripts": ["ok", "approval", "askuser", "workflow"],
  },
  "artifactRefs": [],
  "failure": failure,
}, indent=2, sort_keys=True))
print()
PY
  return "$status"
}

cd "$ROOT"
if [[ "$MODE" == "live" ]]; then
  live_plan
  exit 0
fi
stub_run
