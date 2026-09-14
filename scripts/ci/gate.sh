#!/usr/bin/env bash
# Shared coordinator / CI gate. Command output goes to stderr; --list emits JSON.
set -euo pipefail
cd "$(dirname "${BASH_SOURCE[0]}")/../.."
exec python3 -B - "$@" <<'PY'
import argparse
import json
import os
from pathlib import Path
import shutil
import subprocess
import sys
import time

parser = argparse.ArgumentParser(description="Run the ordered Remuda merge gate")
selection = parser.add_mutually_exclusive_group()
selection.add_argument("--web", action="store_true", help="include web checks")
selection.add_argument("--web-only", action="store_true", help="CI web job only")
parser.add_argument("--web-e2e", action="store_true", help="include live Hub browser authentication checks")
parser.add_argument("--list", action="store_true", help="print the plan without running it")
parser.add_argument("--report", type=Path, help="write one JSON step result per line")
tests = parser.add_mutually_exclusive_group()
tests.add_argument("--affected", action="store_true", help="test changed crates and reverse dependencies")
tests.add_argument("--full", action="store_true", help="test the full workspace (CI default)")
parser.add_argument("--base", help="expected main commit for affected test selection")
parser.add_argument("--head", default="HEAD", help="candidate revision (dry-run planning only)")
args = parser.parse_args()
if args.affected and not args.base:
    parser.error("--affected requires --base <expected-main>")
root = Path.cwd()
os.environ.setdefault("CARGO_TARGET_DIR", str(root / "target-gate"))
os.environ["CARGO_INCREMENTAL"] = "0"
sys.path.insert(0, str(root / "scripts/ci"))
from affected import test_selection

crates, reason = [], "web only"
if not args.web_only:
    try:
        crates, reason = test_selection(root, args.base if args.affected else None, args.head)
    except (OSError, ValueError, KeyError, subprocess.CalledProcessError) as exc:
        parser.exit(1, f"gate: cannot select Rust test crates: {exc}\n")
test_command = ["cargo", "test", "--locked"]
test_command += [arg for name in crates for arg in ["-p", name]] if args.affected else ["--workspace"]

# This is the only definition of gate commands, order, and retry policy.
definitions = [
    ("secret-scan", ["./scripts/ci/secret-scan.sh"], ".", 1),
    ("no-tunnel-scan", ["./scripts/ci/no-tunnel-scan.sh"], ".", 1),
    ("cargo-fmt", ["cargo", "fmt", "--all", "--check"], ".", 1),
    ("cargo-check", ["cargo", "check", "--workspace", "--all-targets", "--locked"], ".", 1),
    ("cargo-clippy", ["cargo", "clippy", "--workspace", "--all-targets", "--locked", "--", "-D", "warnings"], ".", 1),
    ("cargo-test", test_command, ".", 2),
    ("web-install", ["pnpm", "install", "--frozen-lockfile"], "web", 1),
    ("web-build", ["pnpm", "build"], "web", 1),
    ("web-test", ["pnpm", "test"], "web", 1),
    ("web-hub-e2e", ["pnpm", "run", "test:e2e:hub"], "web", 1),
]
# Test seam: an executable path, not a shell expression. It receives the step
# name and still exercises ordering, retries, reports, cwd, and environment.
override = os.environ.get("REMUDA_MERGE_GATE_COMMAND")
plan = []
for name, command, cwd, attempts in definitions:
    selected = (args.web or args.web_only or args.web_e2e) if cwd == "web" else not args.web_only
    if name == "cargo-test" and args.affected and not crates:
        selected = False
    if name == "web-hub-e2e":
        selected = args.web_e2e
    plan.append(dict(name=name, command=[override, name] if override else command,
                     cwd=cwd, maxAttempts=attempts,
                     status="planned" if selected else "skipped",
                     durationMs=0, attempts=0, retried=False))
    if name == "cargo-test":
        plan[-1].update(crates=crates, selection=reason)
if args.list:
    print(json.dumps(plan))
    sys.exit(0)

failed = False
if not args.web_only:
    print(f"gate: Rust tests: {', '.join(crates) or '(none)'} ({reason})", file=sys.stderr)
report = args.report.open("w", encoding="utf-8") if args.report else None
try:
    for step in plan:
        if failed or step["status"] == "skipped":
            step["status"] = "skipped"
        else:
            started = time.monotonic()
            for attempt in range(1, step["maxAttempts"] + 1):
                step["attempts"] = attempt
                step["retried"] = attempt > 1
                print(f"gate: {step['name']}" + (" (retried)" if attempt > 1 else ""),
                      file=sys.stderr, flush=True)
                command = step["command"]
                # The Hub e2e step shares one browser server across merge
                # lanes; serialise it on an advisory lock (flock(1)).
                if (step["name"] == "web-hub-e2e"
                        and os.environ.get("REMUDA_E2E_LOCK")
                        and shutil.which("flock")):
                    command = ["flock", os.environ["REMUDA_E2E_LOCK"], *command]
                try:
                    result = subprocess.run(command, cwd=root / step["cwd"],
                                            stdin=subprocess.DEVNULL,
                                            stdout=sys.stderr, stderr=sys.stderr)
                    code = result.returncode
                    error = f"exit status {code}"
                except OSError as exc:
                    code, error = 1, str(exc)
                if code == 0:
                    step["status"] = "ok"
                    break
            else:
                step["status"] = "failed"
                step["error"] = error
                failed = True
            step["durationMs"] = int((time.monotonic() - started) * 1000)
        if report:
            report.write(json.dumps(step) + "\n")
            report.flush()
finally:
    if report:
        report.close()
sys.exit(1 if failed else 0)
PY
