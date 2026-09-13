#!/usr/bin/env python3
"""Extract the embedded agent-detection rule manifests from a `herdr` binary.

herdr ships its agent state classifier as a set of TOML documents compiled into
`.rodata` (see D-028 §10: "herdr 的「状态分类器」不是模型，是一张可提取的 TOML
规则表"). The documents sit back to back with no separator, so this script finds
each `id = "<kind>"` / `version = "..."` header and then walks forward with a
line grammar until the document stops being TOML.

Usage:
    python3 extract-herdr-rules.py <herdr-binary> \
        [--out-dir crates/remuda-rules/rules] \
        [--herdr-version 0.9.0] \
        [--only claude,codex,grok,agy] \
        [--list]

The binary is read as data; it is never executed.
"""

from __future__ import annotations

import argparse
import re
import sys
from pathlib import Path

# A manifest always opens with these two keys, in this order.
DOC_START = re.compile(rb'id = "([a-z0-9_-]+)"\nversion = "')

# At bracket depth 0 a manifest line is blank, a comment, a `[[rules]]` table
# header, or a bare `key = value`. Anything else means we walked off the end of
# the document into neighbouring rodata.
TOP_LEVEL_LINE = re.compile(r"^(?:\s*|\s*#.*|\[\[rules\]\]|[a-z_][a-z0-9_]* = .*)$")

HEADER_KEYS = ("version", "min_engine_version", "updated_at", "aliases")


def scan_depth(line: str, depth: int) -> int:
    """Return the bracket depth after `line`, ignoring brackets inside strings.

    TOML literal strings (`'...'`) carry the regexes, and those are full of
    `[`/`]`, so a naive bracket count would never close.
    """
    i = 0
    n = len(line)
    while i < n:
        ch = line[i]
        if ch == "#" and depth == 0:
            break  # comment to end of line
        if ch == "'":
            end = line.find("'", i + 1)
            i = n if end < 0 else end + 1
            continue
        if ch == '"':
            i += 1
            while i < n:
                if line[i] == "\\":
                    i += 2
                    continue
                if line[i] == '"':
                    i += 1
                    break
                i += 1
            continue
        if ch == "[":
            depth += 1
        elif ch == "]":
            depth -= 1
        i += 1
    return depth


def document_at(text: str, start: int) -> str:
    """Take the maximal prefix from `start` that still parses as the grammar."""
    lines = text[start:].split("\n")
    depth = 0
    kept: list[str] = []
    for line in lines:
        if depth == 0 and not TOP_LEVEL_LINE.match(line):
            break
        kept.append(line)
        depth = scan_depth(line, depth)
        if depth < 0:
            raise ValueError(f"unbalanced brackets at offset {start}")
    while kept and not kept[-1].strip():
        kept.pop()
    if depth != 0:
        raise ValueError(f"unterminated array in document at offset {start}")
    return "\n".join(kept) + "\n"


def header_field(doc: str, key: str) -> str | None:
    m = re.search(rf"^{key} = (.+)$", doc, re.MULTILINE)
    return m.group(1).strip() if m else None


def extract(blob: bytes) -> list[tuple[str, int, str]]:
    """Return `(kind, offset, document)` for every manifest in the binary."""
    starts = [(m.start(), m.group(1).decode("ascii")) for m in DOC_START.finditer(blob)]
    out = []
    for i, (off, kind) in enumerate(starts):
        # Manifests sit back to back with no separator, so the next header is
        # the hard upper bound; the line grammar handles the final document.
        end = starts[i + 1][0] if i + 1 < len(starts) else min(off + 64 * 1024, len(blob))
        doc = document_at(blob[off:end].decode("utf-8", "replace"), 0)
        if header_field(doc, "min_engine_version") is None:
            continue  # not a manifest, just a coincidental `id`/`version` pair
        out.append((kind, off, doc))
    return out


def banner(kind: str, doc: str, herdr_version: str, binary: Path) -> str:
    fields = [f"{k} = {header_field(doc, k)}" for k in HEADER_KEYS if header_field(doc, k)]
    joined = "\n".join(f"#   {f}" for f in fields)
    return (
        f"# Agent-detection rule manifest for `{kind}`, extracted verbatim from the\n"
        f"# herdr {herdr_version} binary ({binary.name}) — see D-028 §10.\n"
        "#\n"
        "# Upstream: herdr, Copyright 2026 the herdr authors, Apache-2.0.\n"
        "# This file is a copied third-party asset; see the repo NOTICE. Do not hand\n"
        "# edit — regenerate with crates/remuda-rules/scripts/extract-herdr-rules.py\n"
        "# when the pinned herdr binary moves, then re-run the crate tests.\n"
        "#\n"
        "# Declared versions:\n"
        f"{joined}\n"
        "#\n"
        "# Everything below this banner is byte-for-byte upstream text.\n"
    )


def main() -> int:
    ap = argparse.ArgumentParser(description=__doc__)
    ap.add_argument("binary", type=Path, help="path to the herdr binary (read as data)")
    ap.add_argument("--out-dir", type=Path, default=Path("crates/remuda-rules/rules"))
    ap.add_argument("--herdr-version", default="0.9.0")
    ap.add_argument("--only", default="", help="comma-separated kinds; default all")
    ap.add_argument("--list", action="store_true", help="list manifests, write nothing")
    args = ap.parse_args()

    blob = args.binary.read_bytes()
    docs = extract(blob)
    if not docs:
        print("no manifests found; is this a herdr binary?", file=sys.stderr)
        return 1

    wanted = {k for k in args.only.split(",") if k}
    if args.list:
        for kind, off, doc in docs:
            rules = doc.count("[[rules]]")
            print(f"{kind:10} @{off:<10} {header_field(doc, 'version')}  {rules} rules")
        return 0

    args.out_dir.mkdir(parents=True, exist_ok=True)
    written = 0
    for kind, _off, doc in docs:
        if wanted and kind not in wanted:
            continue
        path = args.out_dir / f"{kind}.toml"
        path.write_text(banner(kind, doc, args.herdr_version, args.binary) + doc, "utf-8")
        written += 1
        print(f"wrote {path} ({doc.count('[[rules]]')} rules)")
    print(f"{written} manifest(s) written to {args.out_dir}")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
