#!/usr/bin/env bash
# D-031: inspect tracked worktree content without executing any of it.
set -euo pipefail
cd "$(dirname "${BASH_SOURCE[0]}")/../.."
exec python3 -B - "$@" <<'PY'
import argparse
import json
import os
from pathlib import Path
import re
import stat
import subprocess
import sys
import tempfile

ALLOWLIST = {
    "docs/design/decisions.md",
    "docs/design/coordinator-guide.md",
    "scripts/ci/no-tunnel-scan.sh",
}
# Case-sensitive bytes preserve the requested tokens and scan binary files too.
FORBIDDEN = re.compile(rb"cloudflared|ngrok|frpc|frps|bore\b|tailscale funnel|ssh -R|ssh -D")


def scan(root):
    result = subprocess.run(
        ["git", "ls-files", "--cached", "-z"], cwd=root,
        stdout=subprocess.PIPE, stderr=subprocess.PIPE,
    )
    if result.returncode:
        raise RuntimeError("cannot list tracked files (git exit %d)" % result.returncode)
    if result.stdout and not result.stdout.endswith(b"\0"):
        raise RuntimeError("git returned an incomplete tracked-file list")
    findings = []
    for raw_path in sorted(set(result.stdout.split(b"\0")) - {b""}):
        relative = os.fsdecode(raw_path)
        if relative in ALLOWLIST:
            continue
        path = root / relative
        try:
            mode = path.lstat().st_mode
        except FileNotFoundError:
            # Unstaged deletions remain in the index, but have no current content.
            continue
        except OSError as exc:
            raise RuntimeError("cannot inspect %s: %s" % (json.dumps(relative), exc.strerror)) from exc
        try:
            if stat.S_ISLNK(mode):
                # A tracked symlink's content is its target, not an external file.
                content = os.fsencode(os.readlink(path))
            elif stat.S_ISREG(mode):
                content = path.read_bytes()
            else:
                raise RuntimeError("unsupported tracked file: %s" % json.dumps(relative))
        except OSError as exc:
            raise RuntimeError("cannot read %s: %s" % (json.dumps(relative), exc.strerror)) from exc
        for match in FORBIDDEN.finditer(content):
            line = content.count(b"\n", 0, match.start()) + 1
            findings.append((relative, line, match.group().decode("ascii")))
    return findings


def self_test():
    # These are inert file contents. Only Git is invoked in this temporary repo.
    needles = [b"cloudflared", b"ngrok", b"frpc", b"frps", b"bore",
               b"tailscale funnel", b"ssh -R", b"ssh -D"]
    with tempfile.TemporaryDirectory(prefix="no-tunnel-scan-") as directory:
        root = Path(directory)

        def git(*args):
            subprocess.run(["git", *args], cwd=root, check=True,
                           stdout=subprocess.PIPE, stderr=subprocess.PIPE)

        def write(relative, content):
            path = root / relative
            path.parent.mkdir(parents=True, exist_ok=True)
            path.write_bytes(content)

        git("init", "-q")
        write("tracked.txt", b"Caddy\nfRps bored borehole\n")
        for relative in ALLOWLIST:
            write(relative, b"\n".join(needles))
        write("deleted.txt", needles[0])
        git("add", "--", "tracked.txt", "deleted.txt", *sorted(ALLOWLIST))
        (root / "deleted.txt").unlink()
        write("untracked.txt", b"\n".join(needles))
        assert scan(root) == [], "allowlist, deletion, case, boundary or untracked handling"

        # Read modified worktree content, even when the index still has clean text.
        write("tracked.txt", b"safe\n" + b"\n".join(needles))
        expected = [("tracked.txt", number + 2, needle.decode("ascii"))
                    for number, needle in enumerate(needles)]
        assert scan(root) == expected, "all requested tokens and line numbers"
        write("tracked.txt", b"safe\n")

        unusual = "directory with spaces/file\nname.bin"
        write(unusual, b"\xff\0ngrok\0")
        git("add", "--", unusual)
        assert scan(root) == [(unusual, 1, "ngrok")], "NUL-delimited paths and binary content"
        (root / unusual).unlink()

        (root / "link").symlink_to("cloudflared")
        git("add", "--", "link")
        assert scan(root) == [("link", 1, "cloudflared")], "symlink content"
        (root / "link").unlink()

        # Unsupported/unreadable tracked entries must fail closed.
        (root / "tracked.txt").unlink()
        (root / "tracked.txt").mkdir()
        try:
            scan(root)
        except RuntimeError:
            pass
        else:
            raise AssertionError("unsupported tracked entry did not fail")
    with tempfile.TemporaryDirectory(prefix="no-tunnel-scan-") as directory:
        try:
            scan(Path(directory))
        except RuntimeError:
            pass
        else:
            raise AssertionError("Git error did not fail")
    print("no-tunnel-scan: self-test passed")


parser = argparse.ArgumentParser(description="Reject prohibited tooling in tracked worktree files (D-031)")
parser.add_argument("--self-test", action="store_true", help="test with inert temporary Git fixtures")
args = parser.parse_args()
try:
    if args.self_test:
        self_test()
        sys.exit(0)
    findings = scan(Path.cwd())
except (OSError, RuntimeError, subprocess.CalledProcessError, AssertionError) as exc:
    print("no-tunnel-scan: %s" % exc, file=sys.stderr)
    sys.exit(1)
for relative, line, token in findings:
    print("%s:%d: prohibited token %s" % (json.dumps(relative), line, json.dumps(token)), file=sys.stderr)
if findings:
    print("no-tunnel-scan: failed (%d matches); see D-031" % len(findings), file=sys.stderr)
    sys.exit(1)
print("no-tunnel-scan: passed")
PY
