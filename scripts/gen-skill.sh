#!/usr/bin/env bash
# Section files are authoritative; SKILL.md is the deterministic assembled copy.
set -euo pipefail
skill_repo="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
exec python3 - "$skill_repo" "$@" <<'PY'
import argparse
from pathlib import Path
import sys

root = Path(sys.argv.pop(1))
parser = argparse.ArgumentParser(description="Assemble skills/remuda/sections/*.md")
parser.add_argument("--check", action="store_true", help="fail if the committed skill is stale")
args = parser.parse_args()
folder = root / "skills/remuda"
sections = sorted((folder / "sections").glob("*.md"))
if not sections or sections[0].name != "00-intro.md":
    parser.error("sections/00-intro.md must provide the skill frontmatter")
parts = [path.read_text(encoding="utf-8").strip("\n") for path in sections]
if not parts[0].startswith("---\n"):
    parser.error("00-intro.md must start with YAML frontmatter")
assembled = ("\n\n".join(parts) + "\n").encode("utf-8")
target = folder / "SKILL.md"
if args.check:
    if not target.is_file() or target.read_bytes() != assembled:
        print("skill is stale: edit sections/*.md, then run ./scripts/gen-skill.sh", file=sys.stderr)
        sys.exit(1)
    print("generated skill: current")
else:
    target.write_bytes(assembled)
    print("generated skills/remuda/SKILL.md")
PY
