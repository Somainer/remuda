#!/usr/bin/env python3
"""Prepare a reviewable Caddy change; apply/rollback restart only after approval.

Run on the Hub host in the existing deployment directory. The private settings
file contains URLs and the reviewed baseline hashes, never the DNS token.
"""
import argparse
import fcntl
import hashlib
import json
import os
from pathlib import Path
import re
import subprocess
import time
from urllib.parse import urlsplit

IMPORT = b"import /config/remuda/Caddyfile.d/*.caddy\n"
CONTAINER = "deploy-caddy-1"
MAIN = Path("Caddyfile")
BACKUP = Path("Caddyfile.pre-remuda-approved")
PREVIEW = Path("Caddyfile.remuda-prepared")
STATE = Path(".remuda-caddy-change-state.json")
INCLUDE = Path("Caddyfile.d/remuda.caddy")
REMOTE_INCLUDE = "/config/remuda/Caddyfile.d/remuda.caddy"


def digest(data):
    return hashlib.sha256(data).hexdigest()


def candidate(original):
    """Only change the global admin option and append the dedicated import."""
    if IMPORT.strip() in original:
        raise ValueError("Remuda import already exists; review current state")
    updated, count = re.subn(
        rb"(?m)^([ \t]*)admin[ \t]+off[ \t]*$",
        rb"\1admin localhost:2019", original,
    )
    if count != 1:
        raise ValueError("Expected exactly one reviewed admin off option")
    return updated + (b"" if updated.endswith(b"\n") else b"\n") + b"\n" + IMPORT


def run(command, timeout=60):
    result = subprocess.run(command, stdout=subprocess.PIPE, stderr=subprocess.PIPE,
                            timeout=timeout)
    if result.returncode:
        # Logs can contain private names; keep diagnostics in the private host file.
        with open(".remuda-caddy-change.log", "ab") as log:
            log.write(result.stdout + result.stderr)
        raise RuntimeError("Command failed; see private .remuda-caddy-change.log")
    return result.stdout


def container_state():
    return json.loads(run(["docker", "inspect", CONTAINER]))[0]["State"]


def health(url, expected_hash=None):
    parsed = urlsplit(url)
    if parsed.scheme != "https" or not parsed.hostname or parsed.username or parsed.password:
        raise ValueError("Health endpoints must be HTTPS URLs without credentials")
    body = run(["curl", "--noproxy", "*", "--silent", "--show-error", "--fail",
                "--max-time", "10", url], timeout=15)
    if expected_hash and digest(body) != expected_hash:
        raise RuntimeError("Gateway readiness body changed")
    return body


def gateway(settings):
    health(settings["gateway_health_url"], settings["gateway_health_sha256"])


def prepare(settings):
    original = MAIN.read_bytes()
    if digest(original) != settings["baseline_caddy_sha256"]:
        raise RuntimeError("Caddyfile differs from reviewed baseline; refusing change")
    status = container_state()
    if not status["Running"] or status["StartedAt"] != settings["baseline_started_at"]:
        raise RuntimeError("Caddy lifecycle changed since review; inspect before approval")
    if digest(INCLUDE.read_bytes()) != settings["include_sha256"]:
        raise RuntimeError("Staged include differs from reviewed content")
    gateway(settings)
    updated = candidate(original)
    PREVIEW.write_bytes(updated)
    # The existing container drops capabilities. These configs contain references
    # to CF_API_TOKEN, not its value; permit the container process to read them.
    PREVIEW.chmod(0o644)
    INCLUDE.chmod(0o644)
    run(["docker", "exec", CONTAINER, "mkdir", "-p", "/config/remuda/Caddyfile.d"])
    run(["docker", "cp", str(INCLUDE), CONTAINER + ":" + REMOTE_INCLUDE])
    run(["docker", "cp", str(PREVIEW), CONTAINER + ":/config/remuda/Caddyfile.prepared"])
    run(["docker", "exec", CONTAINER, "caddy", "validate", "--config",
         "/config/remuda/Caddyfile.prepared", "--adapter", "caddyfile"])
    gateway(settings)
    if MAIN.read_bytes() != original:
        raise RuntimeError("Active Caddyfile changed during preparation")
    return original, updated


def wait_healthy(settings, include_hub):
    deadline = time.monotonic() + 120
    while True:
        try:
            gateway(settings)
            if include_hub and json.loads(health(settings["hub_health_url"])) != {"ok": True}:
                raise RuntimeError("Hub health payload is not ok")
            return
        except (RuntimeError, subprocess.TimeoutExpired, ValueError):
            if time.monotonic() >= deadline:
                raise RuntimeError("Post-restart health verification timed out")
            time.sleep(3)


def rollback(settings):
    saved = json.loads(STATE.read_text())
    original = BACKUP.read_bytes()
    if digest(original) != saved["original_sha256"]:
        raise RuntimeError("Rollback backup changed; refusing overwrite")
    current = digest(MAIN.read_bytes())
    if current not in (saved["candidate_sha256"], saved["original_sha256"]):
        raise RuntimeError("Caddyfile contains subsequent edits; manual review required")
    try:
        gateway(settings)
    except Exception:
        # An unhealthy gateway must not prevent restoring the approved baseline.
        pass
    MAIN.write_bytes(original)  # Preserve the existing bind-mounted inode.
    # This is the hash-verified, previously running baseline. The failed change
    # may have left Caddy stopped, so rollback must not depend on docker exec.
    run(["docker", "restart", CONTAINER], timeout=90)
    wait_healthy(settings, include_hub=False)
    print("Rollback complete: original admin/import and gateway readiness restored")


def apply(settings):
    original, updated = prepare(settings)
    if BACKUP.exists():
        raise RuntimeError("Approval backup already exists; inspect prior execution first")
    BACKUP.write_bytes(original)
    STATE.write_text(json.dumps({"original_sha256": digest(original),
                                 "candidate_sha256": digest(updated)}))
    changed = False
    try:
        gateway(settings)
        if MAIN.read_bytes() != original:
            raise RuntimeError("Caddyfile changed since preparation")
        try:
            MAIN.write_bytes(updated)
        except BaseException:
            # A failed write can leave the bind-mounted inode truncated. Restore
            # our own incomplete write before the normal drift-guarded rollback.
            MAIN.write_bytes(original)
            rollback(settings)
            raise
        changed = True
        run(["docker", "restart", CONTAINER], timeout=90)
        wait_healthy(settings, include_hub=True)
    except BaseException:
        if changed:
            rollback(settings)
        raise
    print("Apply complete: gateway readiness unchanged and Hub HTTPS health verified")


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("action", choices=["prepare", "apply", "rollback"])
    parser.add_argument("--settings", default=".remuda-caddy-change.json")
    args = parser.parse_args()
    os.umask(0o077)
    settings = json.loads(Path(args.settings).read_text())
    with open(".remuda-caddy-change.lock", "w") as lock:
        fcntl.flock(lock, fcntl.LOCK_EX | fcntl.LOCK_NB)
        if args.action == "prepare":
            prepare(settings)
            print("Prepared and validated; active Caddyfile unchanged; no restart executed")
        elif args.action == "apply":
            apply(settings)
        else:
            rollback(settings)


if __name__ == "__main__":
    main()
