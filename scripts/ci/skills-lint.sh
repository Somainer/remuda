#!/usr/bin/env bash
# Lint every committed skill under skills/: is it executable as written, and
# does it stay inside what a launched agent may do?
#
# Four checks, all of them things a reviewer had to check by eye before:
#   1. shellcheck every skills/**/*.sh
#   2. each skills/<name>/SKILL.md declares frontmatter `name: <name>`
#   3. mode discipline inside a skill: *.sh executable, everything else not.
#      Git records only the executable bit, so that is what is compared; a
#      checkout's umask cannot make this fail.
#   4. no host-mutating install instructions (a user-scoped MCP registration,
#      or an install that copies into a user-level agent config directory)
#
# Check 4 is a reject list, not a style rule: a committed skill is delivered
# by Remuda into a managed home, never by editing the operator's own config.
set -euo pipefail
cd "$(dirname "${BASH_SOURCE[0]}")/../.."
exec python3 -B - "$@" <<'PY'
import argparse
from contextlib import contextmanager
import os
from pathlib import Path
import re
import shutil
import stat
import subprocess
import sys
import tempfile

SKILLS = Path("skills")
FORBIDDEN = [
    (re.compile(r"claude mcp add[^\n]*--scope user"),
     "user-scoped MCP registration"),
    (re.compile(r"\$HOME/\.claude|~/\.claude|\$\{HOME\}/\.claude"),
     "user-level agent config directory"),
]
EXECUTABLE_SUFFIXES = {".sh"}


def files_under(root):
    return sorted(path for path in root.rglob("*") if path.is_file())


def check_shellcheck(root, findings):
    if shutil.which("shellcheck") is None:
        print("skills-lint: warning: shellcheck not found; skipping the shell check",
              file=sys.stderr)
        return
    for path in sorted(path for path in root.rglob("*.sh") if path.is_file()):
        result = subprocess.run(
            ["shellcheck", "-x", "--", str(path)],
            stdout=subprocess.PIPE, stderr=subprocess.STDOUT, text=True,
        )
        if result.returncode:
            findings.append("%s: shellcheck failed\n%s" % (path, result.stdout.rstrip()))


def check_frontmatter(root, findings):
    for skill in sorted(path for path in root.iterdir() if path.is_dir()):
        target = skill / "SKILL.md"
        if not target.is_file():
            findings.append("%s: no SKILL.md" % skill)
            continue
        lines = target.read_text(encoding="utf-8").splitlines()
        if not lines or lines[0].strip() != "---":
            findings.append("%s: SKILL.md does not start with YAML frontmatter" % target)
            continue
        declared = None
        for line in lines[1:]:
            if line.strip() == "---":
                break
            match = re.match(r"name:\s*(\S+)\s*$", line)
            if match:
                declared = match.group(1)
                break
        if declared is None:
            findings.append("%s: frontmatter has no unscoped `name:`" % target)
        elif declared != skill.name:
            findings.append("%s: frontmatter name %r != directory %r"
                            % (target, declared, skill.name))


def check_modes(root, findings):
    for path in files_under(root):
        mode = stat.S_IMODE(path.lstat().st_mode)
        want_executable = path.suffix in EXECUTABLE_SUFFIXES
        if want_executable and not mode & 0o111:
            findings.append("%s: mode %04o, want an executable mode" % (path, mode))
        elif not want_executable and mode & 0o111:
            findings.append("%s: mode %04o, want a non-executable mode" % (path, mode))


def check_forbidden(root, findings):
    for path in files_under(root):
        try:
            text = path.read_text(encoding="utf-8")
        except (OSError, UnicodeDecodeError):
            continue
        for number, line in enumerate(text.splitlines(), start=1):
            for pattern, why in FORBIDDEN:
                if pattern.search(line):
                    findings.append("%s:%d: %s: %s" % (path, number, why, line.strip()))


def lint(root):
    if not root.is_dir():
        raise RuntimeError("%s is not a directory" % root)
    findings = []
    check_shellcheck(root, findings)
    check_frontmatter(root, findings)
    check_modes(root, findings)
    check_forbidden(root, findings)
    return findings


GOOD = {
    "SKILL.md": "---\nname: sample\ndescription: inert fixture\n---\n\n# Sample\n",
    "references/notes.md": "Reads stay inside the instance directory.\n",
    "scripts/run.sh": "#!/bin/sh\nset -eu\necho ok\n",
}


@contextmanager
def fixture(files, modes=None):
    with tempfile.TemporaryDirectory(prefix="skills-lint-") as directory:
        root = Path(directory)
        for relative, content in files.items():
            path = root / "sample" / relative
            path.parent.mkdir(parents=True, exist_ok=True)
            path.write_text(content, encoding="utf-8")
        for relative, mode in (modes or {}).items():
            os.chmod(root / "sample" / relative, mode)
        yield root


def self_test():
    with fixture(GOOD, {"scripts/run.sh": 0o644}) as root:
        assert any("want an executable mode" in finding for finding in lint(root)), lint(root)
    with fixture(GOOD, {"scripts/run.sh": 0o755}) as root:
        assert not any("mode" in finding for finding in lint(root)), lint(root)
    with fixture(GOOD, {"references/notes.md": 0o755}) as root:
        assert any("want a non-executable mode" in finding for finding in lint(root)), lint(root)
    with fixture({**GOOD, "SKILL.md": "---\nname: renamed\n---\n\n# Sample\n"}) as root:
        assert any("!= directory" in finding for finding in lint(root)), lint(root)
    with fixture({**GOOD, "scripts/install.sh":
                  "#!/bin/sh\nclaude mcp add --transport stdio --scope user x -- /bin/sh y.sh\n"},
                 {"scripts/run.sh": 0o755, "scripts/install.sh": 0o755}) as root:
        assert any("user-scoped MCP registration" in finding for finding in lint(root)), lint(root)
    with fixture({**GOOD, "references/notes.md":
                  'cp -R -n skills/sample "$HOME/.claude/skills/"\n'}) as root:
        assert any("user-level agent config directory" in finding for finding in lint(root)), lint(root)
    with fixture({**GOOD, "references/notes.md":
                  "- `${CODEX_HOME:-$HOME/.codex}/computer-use/Thing.app`\n"},
                 {"scripts/run.sh": 0o755}) as root:
        assert lint(root) == [], lint(root)
    with fixture({"references/notes.md": "no frontmatter here\n"}) as root:
        assert any("no SKILL.md" in finding for finding in lint(root)), lint(root)
    print("skills-lint: self-test passed")


parser = argparse.ArgumentParser(
    description="Lint committed skills (shellcheck, frontmatter, modes, host mutation)")
parser.add_argument("--self-test", action="store_true", help="test with inert temporary fixtures")
parser.add_argument("--root", type=Path, default=SKILLS, help="skills directory to lint")
args = parser.parse_args()
try:
    if args.self_test:
        self_test()
        sys.exit(0)
    findings = lint(args.root)
except (OSError, RuntimeError) as exc:
    print("skills-lint: %s" % exc, file=sys.stderr)
    sys.exit(1)
for finding in findings:
    print("skills-lint: %s" % finding, file=sys.stderr)
if findings:
    print("skills-lint: failed (%d findings)" % len(findings), file=sys.stderr)
    sys.exit(1)
print("skills-lint: passed")
PY
