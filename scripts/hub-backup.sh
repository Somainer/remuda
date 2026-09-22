#!/usr/bin/env bash
# Encrypted local backup of a Hub data_dir (c-hubstate).
#
# Design contract (authoritative prose: docs/design/hub-topology.md §4–§5):
#   * age encryption is mandatory: without --recipient or AGE_RECIPIENT the
#     script refuses in EVERY mode, including dry-run.
#   * dry-run is the default; writing a bundle additionally requires
#     --execute AND REMUDA_BACKUP_YES_I_KNOW=1.
#   * the bundle covers SQLite with WAL/SHM, secrets/, the push VAPID private
#     key + push.sqlite, the Hub identity key (docs/design/protocol.md §7.7)
#     and bootstrap-token. See docs/design/hub-topology.md §4 for why each
#     member matters.
#   * local files only: no network, no remote transfer tool, never writes
#     under a deploy/ directory or into the data_dir being backed up.
#
# This script does not mutate the data_dir. It is POSIX-local bash plus
# GNU tar, sha256sum, and (execute only) age.
set -euo pipefail

PROG="hub-backup"
MODE="dry-run"
DATA_DIR=""
OUTPUT_DIR=""
RECIPIENT=""

usage() {
    cat <<'USAGE'
Usage: hub-backup.sh --data-dir DIR [--output-dir DIR]
                    [--recipient AGE_RECIPIENT] [--execute]

  --data-dir DIR     Hub data_dir to back up (or REMUDA_HUB_DATA_DIR).
  --output-dir DIR   Local directory for the bundle (default: current dir).
  --recipient R      age recipient (or AGE_RECIPIENT). Required in every mode.
  --execute          Actually write the encrypted bundle. Without it this is
                     a dry-run that writes nothing. With it the environment
                     must also set REMUDA_BACKUP_YES_I_KNOW=1.

The script never opens a network connection and never writes under deploy/.
USAGE
}

fail() {
    # $1 = exit code, remainder = message
    local code="$1"; shift
    echo "$PROG: ERROR: $*" >&2
    exit "$code"
}

info() {
    echo "$PROG: $*"
}

while [[ $# -gt 0 ]]; do
    case "$1" in
        --data-dir)
            [[ $# -ge 2 ]] || fail 2 "--data-dir requires a value"
            DATA_DIR="$2"; shift 2 ;;
        --output-dir)
            [[ $# -ge 2 ]] || fail 2 "--output-dir requires a value"
            OUTPUT_DIR="$2"; shift 2 ;;
        --recipient)
            [[ $# -ge 2 ]] || fail 2 "--recipient requires a value"
            RECIPIENT="$2"; shift 2 ;;
        --execute)
            MODE="execute"; shift ;;
        -h|--help)
            usage; exit 0 ;;
        *)
            usage >&2; fail 2 "unknown argument: $1" ;;
    esac
done

[[ -n "$DATA_DIR" ]] || DATA_DIR="${REMUDA_HUB_DATA_DIR:-}"
[[ -n "$DATA_DIR" ]] || fail 2 "no data_dir: pass --data-dir or set REMUDA_HUB_DATA_DIR"
[[ -d "$DATA_DIR" ]] || fail 2 "data_dir is not a directory: $DATA_DIR"
# Resolve without printing the absolute path into the bundle: labels below use
# the basename only.
DATA_DIR="$(cd "$DATA_DIR" && pwd)"
DATA_LABEL="$(basename "$DATA_DIR")"

[[ -n "$OUTPUT_DIR" ]] || OUTPUT_DIR="${REMUDA_BACKUP_DIR:-$PWD}"
mkdir -p "$OUTPUT_DIR"
OUTPUT_DIR="$(cd "$OUTPUT_DIR" && pwd)"

# ── Gate 1: age recipient is mandatory, even for a dry-run ──────────────────
[[ -n "$RECIPIENT" ]] || RECIPIENT="${AGE_RECIPIENT:-}"
if [[ -z "$RECIPIENT" ]]; then
    fail 2 "age recipient required: pass --recipient or set AGE_RECIPIENT; plaintext bundles are not supported (hub-topology.md §5.3)"
fi
RECIPIENT_FP="$(printf '%s' "$RECIPIENT" | sha256sum | cut -c1-12)"

# ── Gate 2: destination must not be deploy/ or inside the data_dir ──────────
case "/$OUTPUT_DIR/" in
    */deploy/*)
        fail 5 "refusing to write under a deploy/ directory: $OUTPUT_DIR" ;;
esac
case "$OUTPUT_DIR" in
    "$DATA_DIR")
        fail 5 "refusing to write the bundle inside the data_dir being backed up" ;;
esac

# ── Gate 3: execute needs the explicit environment acknowledgement ─────────
if [[ "$MODE" == "execute" ]]; then
    [[ "${REMUDA_BACKUP_YES_I_KNOW:-}" == "1" ]] \
        || fail 2 "--execute additionally requires REMUDA_BACKUP_YES_I_KNOW=1"
    command -v age >/dev/null 2>&1 \
        || fail 4 "age not found in PATH; install age or run without --execute for a dry-run"
    tar --version 2>/dev/null | grep -q GNU \
        || fail 4 "GNU tar is required (multi-stage archive assembly)"
fi

STAMP="$(date -u +%Y%m%dT%H%M%SZ)"
BUNDLE_NAME="hub-backup-${STAMP}.tar.age"
BUNDLE_PATH="$OUTPUT_DIR/$BUNDLE_NAME"

# Member specification: top-level name, required? (core members fail preflight
# when absent; optional ones are recorded absent). Directory members are
# recursively expanded to file entries for hashing/manifest purposes.
CORE_MEMBERS=(hub.sqlite secrets bootstrap-token)
OPTIONAL_MEMBERS=(
    hub.sqlite-wal
    hub.sqlite-shm
    vapid.json
    push.sqlite
    push.sqlite-wal
    push.sqlite-shm
    hub-identity
    bootstrap-issued-at
)

declare -a PRESENT_FILES=()
declare -a MEMBER_LINES=()
declare -a MISSING_CORE=()
HAVE_IDENTITY=0

add_member_line() {
    # $1 = top-level member, $2 = present|absent, $3 = detail
    MEMBER_LINES+=("  [$2] $1 ($3)")
}

for member in "${CORE_MEMBERS[@]}" "${OPTIONAL_MEMBERS[@]}"; do
    path="$DATA_DIR/$member"
    if [[ ! -e "$path" ]]; then
        if printf '%s\0' "${CORE_MEMBERS[@]}" | grep -F -x -z -q "$member"; then
            MISSING_CORE+=("$member")
            add_member_line "$member" "ABSENT-CORE" "required member missing"
        else
            add_member_line "$member" "absent" "not present; bundle will not contain it"
        fi
        continue
    fi
    if [[ -d "$path" ]]; then
        dir_files=()
        while IFS= read -r -d '' f; do
            dir_files+=("$f")
        done < <(cd "$DATA_DIR" && find "$member" -type f -print0 | LC_ALL=C sort -z)
        add_member_line "$member" "present" "directory, ${#dir_files[@]} file(s)"
        for f in "${dir_files[@]}"; do
            size="$(wc -c < "$DATA_DIR/$f" | tr -d ' ')"
            short="$(sha256sum "$DATA_DIR/$f" | cut -c1-12)"
            MEMBER_LINES+=("    - $f (${size} bytes, sha256:${short})")
            PRESENT_FILES+=("$f")
        done
        [[ -e "$DATA_DIR/hub-identity/identity.ed25519" ]] && HAVE_IDENTITY=1
    else
        size="$(wc -c < "$path" | tr -d ' ')"
        short="$(sha256sum "$path" | cut -c1-12)"
        add_member_line "$member" "present" "${size} bytes, sha256:${short}"
        PRESENT_FILES+=("$member")
        [[ "$member" == "hub-identity/identity.ed25519" ]] && HAVE_IDENTITY=1
    fi
done

# The Hub identity private key (protocol.md §7.7) is the member whose loss
# invalidates every Node pin on restore. §7.7 is unimplemented, so its absence
# is a warning today; once it ships it must be treated as core.
if [[ "$HAVE_IDENTITY" -eq 1 ]]; then
    IDENTITY_STATUS="present: hub-identity/identity.ed25519 included (Node pins survive restore)"
else
    IDENTITY_STATUS="absent: hub-identity/identity.ed25519 not found (protocol.md §7.7 unimplemented); after that spec ships its absence must block restore"
fi

info "mode       = $MODE$([[ "$MODE" == "dry-run" ]] && echo ' (nothing is written; pass --execute with REMUDA_BACKUP_YES_I_KNOW=1)')"
info "data_dir   = $DATA_LABEL"
info "output     = $BUNDLE_PATH"
info "recipient  = age recipient sha256:$RECIPIENT_FP (encryption mandatory)"
info "members:"
printf '%s\n' "${MEMBER_LINES[@]}"
info "identity   = $IDENTITY_STATUS"
info "manifest   = format=remuda-hub-backup, files=${#PRESENT_FILES[@]}, signature=null (pending protocol.md §7.7)"

if [[ ${#MISSING_CORE[@]} -gt 0 ]]; then
    fail 3 "required members missing from data_dir: ${MISSING_CORE[*]}"
fi

if [[ "$MODE" == "dry-run" ]]; then
    info "DRY-RUN ok"
    exit 0
fi

# ── Execute: best-effort WAL checkpoint, then tar | age ────────────────────
[[ ! -e "$BUNDLE_PATH" && ! -e "$BUNDLE_PATH.sha256" ]] \
    || fail 5 "output already exists for this second; retry for a new timestamp"
if command -v sqlite3 >/dev/null 2>&1; then
    sqlite3 "$DATA_DIR/hub.sqlite" 'PRAGMA wal_checkpoint(TRUNCATE);' >/dev/null 2>&1 \
        && info "wal checkpoint attempted (sqlite3)" \
        || info "wal checkpoint skipped (busy); crash-consistent copy semantics apply"
else
    info "sqlite3 unavailable; copying live WAL (crash-consistent on restore)"
fi

STAGE="$(mktemp -d)"
trap 'rm -rf "$STAGE"' EXIT
umask 077

# Inner manifest: relative names, per-file hashes, no absolute paths or tokens.
{
    echo '{'
    echo '  "format": "remuda-hub-backup",'
    echo '  "version": 1,'
    echo "  \"created_at\": \"$STAMP\","
    echo "  \"data_dir_label\": \"$DATA_LABEL\","
    echo '  "signature": null,'
    echo '  "signatureStatus": "pending: hub identity signature spec protocol.md §7.7 not implemented",'
    echo '  "members": ['
    first=1
    while IFS= read -r -d '' f; do
        hash_line="$(cd "$DATA_DIR" && sha256sum -- "$f")"
        hash="${hash_line%% *}"
        size="$(wc -c < "$DATA_DIR/$f" | tr -d ' ')"
        mode="$(stat -c '%a' "$DATA_DIR/$f")"
        [[ $first -eq 0 ]] && echo ','
        first=0
        printf '    {"path": "%s", "size": %s, "mode": "%s", "sha256": "%s"}' \
            "$f" "$size" "$mode" "$hash"
    done < <(printf '%s\0' "${PRESENT_FILES[@]}" | LC_ALL=C sort -z)
    echo
    echo '  ]'
    echo '}'
} > "$STAGE/manifest.json"

# Assemble an uncompressed tar (append is only valid pre-compression), then
# encrypt the whole archive to the single age recipient.
tar --acls --xattrs -cf "$STAGE/bundle.tar" -C "$STAGE" manifest.json
tar --acls --xattrs -rf "$STAGE/bundle.tar" -C "$DATA_DIR" -- "${PRESENT_FILES[@]}"
age -e -r "$RECIPIENT" -o "$BUNDLE_PATH" "$STAGE/bundle.tar"
chmod 600 "$BUNDLE_PATH"
(cd "$OUTPUT_DIR" && sha256sum "$BUNDLE_NAME") > "$BUNDLE_PATH.sha256"
chmod 600 "$BUNDLE_PATH.sha256"

info "wrote $BUNDLE_PATH ($(wc -c < "$BUNDLE_PATH" | tr -d ' ') bytes)"
info "wrote $BUNDLE_PATH.sha256"
info "execute ok"
