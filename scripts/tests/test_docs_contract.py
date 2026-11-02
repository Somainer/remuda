#!/usr/bin/env python3
"""Documentation contract checker for the repository's markdown.

The checker is stdlib-only and static: it never builds, starts a service, or
touches the network. It reads the markdown corpus from the git index plus
the three command sources that define what a command reference may say:

- the clap definitions under ``crates/remuda/src``;
- ``justfile`` recipes;
- ``web/package.json`` scripts.

Five check kinds:

- ``link``: relative links resolve to an existing file or directory;
- ``anchor``: ``#fragment`` links resolve to a heading or an explicit HTML
  anchor in the target document;
- ``file-anchor``: ``path/file.rs:NNN`` names a file that exists and has at
  least NNN lines. Pointing at the symbol the surrounding text names (within
  three lines) is checked too, but its result is advisory;
- ``command``: ``remuda``/``just``/``pnpm --dir web`` references name a real
  subcommand or flag, recipe, or package script. Advisory: these models are
  hand-parsed from Rust, the justfile and package.json, so a parser gap must
  never fail a branch;
- ``orphan``: a ``docs/design/*.md`` document nothing links to, excluding a
  short entry-point allowlist. Advisory: a branch often lands a design doc
  before the link that points at it.

The landing gate runs on every branch, including branches that move code
lines without touching docs. So the enforced mode fails only on the
well-defined checks: links, heading anchors, and file-anchor existence and
line count. Symbol-proximity mismatches, command references, orphan design
docs, and baselined violations that have vanished are advisory warnings
there. ``--strict`` (maintenance only, never the gate) also fails on those
advisory results.

The enforced violations are recorded in
``scripts/tests/docs_contract_baseline.json``. The baseline may only shrink:
``--update-baseline`` removes entries whose violation is gone and refuses to
add any.

Run ``python3 -B scripts/tests/test_docs_contract.py --check`` from the repo
root (the gate does this), or with ``--root`` against a fixture tree.
"""
from __future__ import annotations

import argparse
import io
import json
import os
import re
import shutil
import subprocess
import sys
import tempfile
import unicodedata
import unittest
import urllib.parse
from dataclasses import dataclass, field
from pathlib import Path

ROOT = Path(__file__).resolve().parents[2]
BASELINE = ROOT / "scripts" / "tests" / "docs_contract_baseline.json"

KIND_LINK = "link"
KIND_ANCHOR = "anchor"
KIND_FILE_ANCHOR = "file-anchor"
KIND_COMMAND = "command"
KIND_ORPHAN = "orphan"
KINDS = (KIND_LINK, KIND_ANCHOR, KIND_FILE_ANCHOR, KIND_COMMAND, KIND_ORPHAN)

SCAN_DIRS = ("docs", "skills")
# Directory names never considered, tracked or not. Test fixtures are
# excluded because they are deliberately broken corpora; build outputs and
# vendored trees must not change the result on one host.
SKIP_DIRS = {
    ".git", ".hg", ".svn", "node_modules", ".venv", "venv", "__pycache__",
    "scratch", "dist", "coverage", "test-results", "playwright-report",
    "target", "vendor",
}
SKIP_PATH_PREFIXES = (("scripts", "tests", "fixtures"),)

# Documents that are entry points rather than the target of a link.
ORPHAN_ENTRY_POINTS = {
    "README.md",
    "docs/architecture.md",
    "docs/design/decisions.md",
    "docs/design/home.md",
    "docs/design/impl-notes.md",
    "docs/design/main-agent.md",
    "docs/design/plan-phase0.md",
    "docs/design/proposal.md",
    "docs/design/protocol.md",
    "docs/design/roadmap-2026-09.md",
    "docs/design/ui-spec.md",
}

EXTERNAL_SCHEME = re.compile(r"^[a-zA-Z][a-zA-Z0-9+.-]*:")
FENCE_OPEN = re.compile(r"^\s{0,3}(`{3,}|~{3,})")
INLINE_CODE = re.compile(r"`+[^`\n]*?`+")
LINK_START = re.compile(r"!?\[[^\]\n]*\]\(")
# Footnote definitions (`[^1]: ...`) are not links.
REFERENCE_DEFINITION = re.compile(r"^\s{0,3}\[(?!\^)([^\]]+)\]:")
REFERENCE_USE = re.compile(r"\[([^\]\n]*)\]\[([^\]\n]*)\]")
BRACKET = re.compile(r"\[([^\]\n]+)\]")
HTML_ANCHOR = re.compile(
    r"<a\s[^>]*(?:id|name)\s*=\s*[\"']([^\"']+)[\"']", re.IGNORECASE
)
HTML_HEADING_ID = re.compile(
    r"<h[1-6]\s[^>]*id\s*=\s*[\"']([^\"']+)[\"']", re.IGNORECASE
)
HEADING = re.compile(r"^\s{0,3}(#{1,6})\s+(.*?)\s*$")
CODE_SPAN = re.compile(r"`([^`\n]+)`")
IDENTIFIER = re.compile(r"[A-Za-z_][A-Za-z0-9_]*(?:::[A-Za-z_][A-Za-z0-9_]*)*")
FILE_ANCHOR = re.compile(
    r"(?<![\w./-])"
    r"(?P<path>[A-Za-z0-9_.@+-]+(?:/[A-Za-z0-9_.@+-]+)*\.(?:rs|py|ts|tsx|js|jsx|svelte|toml|sh))"
    r":(?P<start>\d+)(?:[-–](?P<end>\d+))?"
)
SYMBOL_WINDOW = 3


# ── violation model and baseline ──────────────────────────────────────────

@dataclass(frozen=True)
class Violation:
    kind: str
    doc: str
    ref: str
    message: str
    # Advisory results never fail the gate; --strict fails on them.
    advisory: bool = False

    @property
    def key(self) -> str:
        return f"{self.kind}|{self.doc}|{self.ref}"


@dataclass
class Report:
    files: list[Path]
    violations: dict[str, Violation] = field(default_factory=dict)

    def add(self, kind: str, doc: Path | str, ref: str, message: str, root: Path,
            advisory: bool = False) -> None:
        doc_text = relative(doc, root)
        violation = Violation(kind, doc_text, ref, message, advisory)
        # A key is unique, but the same citation can be advisory once and
        # enforced later (or vice versa). Keep the stronger record so an
        # advisory can never mask an enforced violation.
        existing = self.violations.get(violation.key)
        if existing is None or (existing.advisory and not advisory):
            self.violations[violation.key] = violation

    def enforced(self) -> set[str]:
        """Keys that fail the gate when absent from the baseline."""
        return {v.key for v in self.violations.values() if not v.advisory}

    def advisory(self) -> set[str]:
        """Keys reported as warnings, failing only under --strict."""
        return {v.key for v in self.violations.values() if v.advisory}

    def count(self, kind: str, advisory: bool | None = None) -> int:
        return sum(1 for v in self.violations.values()
                   if v.kind == kind and (advisory is None or v.advisory == advisory))


def relative(path: Path | str, root: Path) -> str:
    candidate = Path(path)
    try:
        return candidate.resolve().relative_to(root.resolve()).as_posix()
    except ValueError:
        return candidate.as_posix()


def load_baseline(path: Path) -> set[str]:
    data = json.loads(path.read_text(encoding="utf-8"))
    if data.get("version") != 1:
        raise ValueError(f"{path}: unsupported baseline version {data.get('version')!r}")
    entries = data.get("violations")
    if not isinstance(entries, list) or not all(isinstance(e, str) for e in entries):
        raise ValueError(f"{path}: violations must be a list of strings")
    return set(entries)


def dump_baseline(path: Path, keys: set[str]) -> None:
    payload = {
        "version": 1,
        "note": (
            "Enforced docs-contract violations accepted at the time this "
            "baseline was written (advisory symbol-proximity results are not "
            "recorded). It may only shrink: fix a violation and run "
            "`python3 -B scripts/tests/test_docs_contract.py --update-baseline`."
        ),
        "violations": sorted(keys),
    }
    path.write_text(json.dumps(payload, ensure_ascii=False, indent=2) + "\n",
                    encoding="utf-8")


# ── corpus ────────────────────────────────────────────────────────────────

def _skipped(rel: Path) -> bool:
    parts = rel.parts
    for prefix in SKIP_PATH_PREFIXES:
        if parts[:len(prefix)] == prefix:
            return True
    return any(part in SKIP_DIRS or part.startswith("target-") for part in parts)


def _in_markdown_scope(rel: Path) -> bool:
    return rel.parts[:1] in (("docs",), ("skills",)) or rel.name == "README.md"


def tracked_files(root: Path) -> list[Path] | None:
    """Tracked files under ``root`` (git index), or None outside git.

    The corpus must not depend on one host's untracked or gitignored local
    files (private research notes, cargo package output under ``target/``).
    """
    if not (root / ".git").exists():
        return None
    try:
        result = subprocess.run(
            ["git", "-C", str(root), "ls-files", "-z"],
            capture_output=True, check=False,
        )
    except OSError:
        return None
    if result.returncode != 0:
        return None
    found: list[Path] = []
    for raw in result.stdout.split(b"\0"):
        if not raw:
            continue
        try:
            path = root / raw.decode("utf-8")
        except UnicodeDecodeError:
            continue
        if path.is_file():
            found.append(path)
    return found


def _walk_files(root: Path) -> list[Path]:
    found: list[Path] = []
    for dirpath, dirnames, filenames in os.walk(root):
        here = Path(dirpath)
        rel = here.relative_to(root)
        dirnames[:] = sorted(name for name in dirnames if not _skipped(rel / name))
        found.extend(here / name for name in filenames)
    return found


def markdown_files(root: Path, use_git: bool = True) -> list[Path]:
    """Every scanned markdown file, in a stable order.

    ``docs/`` and ``skills/`` are scanned whole; elsewhere only README files
    are part of the contract. With a git checkout the file list comes from
    the index, so local-only files never change the answer. Fixture roots
    pass ``use_git=False``.
    """
    tracked = tracked_files(root) if use_git else None
    candidates = tracked if tracked is not None else _walk_files(root)
    found = {
        path for path in candidates
        if path.name.endswith(".md")
        and _in_markdown_scope(path.relative_to(root))
        and not _skipped(path.relative_to(root))
    }
    return sorted(found)


@dataclass(frozen=True)
class Line:
    number: int
    text: str
    fenced: bool


def markdown_lines(text: str) -> list[Line]:
    """Split into lines, marking fenced code blocks (fence lines included)."""
    lines: list[Line] = []
    fence: str | None = None
    for number, raw in enumerate(text.splitlines(), 1):
        match = FENCE_OPEN.match(raw)
        if fence is None and match:
            fence = match.group(1)[0]
            lines.append(Line(number, raw, True))
            continue
        if fence is not None:
            if match and match.group(1)[0] == fence:
                fence = None
            lines.append(Line(number, raw, True))
            continue
        lines.append(Line(number, raw, False))
    return lines


def visible_markdown(text: str) -> list[Line]:
    """Lines outside fences, with inline code and HTML comments blanked."""
    without_comments = re.sub(r"<!--.*?-->", lambda m: "".join(
        "\n" if ch == "\n" else " " for ch in m.group(0)), text, flags=re.S)
    return [
        Line(line.number, INLINE_CODE.sub(lambda m: " " * len(m.group(0)), line.text), False)
        for line in markdown_lines(without_comments) if not line.fenced
    ]


def strip_markup(text: str) -> str:
    def link(match: re.Match[str]) -> str:
        return match.group(1) if match.lastindex and match.group(1) else match.group(0)

    text = re.sub(r"\[([^\]]*)\]\([^)]*\)", link, text)
    text = re.sub(r"<[^>]+>", "", text)
    return text.replace("`", "")


# ── anchors ───────────────────────────────────────────────────────────────

def github_slug(heading: str) -> str:
    """GitHub's heading anchor: lowercase, drop punctuation, spaces to '-'."""
    text = strip_markup(heading).strip().lower()
    kept: list[str] = []
    for char in text:
        if char == "-" or char == "_":
            kept.append(char)
        elif char == " ":
            kept.append("-")
        elif unicodedata.category(char)[0] in ("C", "P", "S", "Z"):
            continue
        else:
            kept.append(char)
    return "".join(kept)


def anchors_for(text: str) -> set[str]:
    anchors = set(HTML_ANCHOR.findall(text)) | set(HTML_HEADING_ID.findall(text))
    seen: dict[str, int] = {}
    for line in markdown_lines(text):
        if line.fenced:
            continue
        match = HEADING.match(line.text)
        if not match:
            continue
        heading = re.sub(r"\s+#+\s*$", "", match.group(2))
        base = github_slug(heading)
        slug = base
        while slug in seen:
            seen[base] += 1
            slug = f"{base}-{seen[base]}"
        seen[slug] = 0
        anchors.add(slug)
    return anchors


# ── links and orphans ─────────────────────────────────────────────────────

def _parse_destination(text: str, index: int) -> tuple[str, int] | None:
    """Parse a CommonMark destination at ``text[index]``.

    Angle-bracketed destinations may contain spaces; bare destinations end at
    whitespace or the closing parenthesis, with balanced parentheses allowed.
    """
    size = len(text)
    while index < size and text[index] in " \t":
        index += 1
    if index >= size:
        return None
    if text[index] == "<":
        end = text.find(">", index + 1)
        if end < 0:
            return None
        return text[index + 1:end], end + 1
    start = index
    depth = 0
    while index < size:
        char = text[index]
        if char == "\\":
            index += 2
            continue
        if char == "(":
            depth += 1
        elif char == ")":
            if depth == 0:
                break
            depth -= 1
        elif char.isspace() and depth == 0:
            break
        index += 1
    return text[start:index], index


def _skip_title(text: str, index: int) -> int | None:
    """Skip an optional link title; None when it is unterminated."""
    size = len(text)
    while index < size and text[index] in " \t":
        index += 1
    if index >= size or text[index] not in "\"'(":
        return index
    closing = ")" if text[index] == "(" else text[index]
    index += 1
    while index < size:
        if text[index] == "\\":
            index += 2
            continue
        if text[index] == closing:
            return index + 1
        index += 1
    return None


def _inline_target(text: str, index: int) -> str | None:
    parsed = _parse_destination(text, index)
    if parsed is None:
        return None
    target, after = parsed
    after = _skip_title(text, after)
    if after is None:
        return None
    while after < len(text) and text[after] in " \t":
        after += 1
    if after >= len(text) or text[after] != ")":
        return None
    return target


def _definition_target(text: str, index: int) -> str | None:
    parsed = _parse_destination(text, index)
    if parsed is None:
        return None
    target, after = parsed
    after = _skip_title(text, after)
    if after is None or text[after:].strip():
        return None
    return target


def _normalize_label(label: str) -> str:
    return re.sub(r"\s+", " ", label.strip()).lower()


def iter_links(lines: list[Line]):
    """Yield (line, raw_target) for inline links and used reference definitions.

    Footnote definitions are not links, and a reference definition is only
    checked when a full, collapsed or shortcut reference uses its label.
    """
    definitions: dict[str, tuple[Line, str]] = {}
    body: list[Line] = []
    for line in lines:
        definition = REFERENCE_DEFINITION.match(line.text)
        if definition:
            target = _definition_target(line.text, definition.end())
            if target is not None:
                definitions[_normalize_label(definition.group(1))] = (line, target)
            continue
        body.append(line)
    used: set[str] = set()
    for line in body:
        for match in REFERENCE_USE.finditer(line.text):
            used.add(_normalize_label(match.group(2) or match.group(1)))
        for match in BRACKET.finditer(line.text):
            following = line.text[match.end():match.end() + 1]
            if following not in ("(", "[", ":"):
                used.add(_normalize_label(match.group(1)))
    for line in body:
        for match in LINK_START.finditer(line.text):
            target = _inline_target(line.text, match.end())
            if target is not None:
                yield line, target
    for label, (line, target) in definitions.items():
        if label in used:
            yield line, target


def link_target(doc: Path, target: str) -> tuple[Path, str] | None:
    """Resolve a target to (path, anchor), or None when it is not local."""
    if not target or target.startswith("/") or target.startswith("#"):
        return (doc, target[1:]) if target.startswith("#") else None
    if EXTERNAL_SCHEME.match(target):
        return None
    decoded = urllib.parse.unquote(target)
    path_part, _, anchor = decoded.partition("#")
    path_part = path_part.strip()
    if not path_part:
        return doc, anchor
    if "{" in path_part or "}" in path_part:
        return None
    candidate = (doc.parent / path_part).resolve()
    return candidate, anchor


def check_links(report: Report, documents: dict[Path, str], anchors: dict[Path, set[str]],
                root: Path) -> set[Path]:
    """Check every local link; return the set of documents with inbound links."""
    linked: set[Path] = set()
    for doc, text in documents.items():
        for line, target in iter_links(visible_markdown(text)):
            resolved = link_target(doc, target)
            if resolved is None:
                continue
            path, anchor = resolved
            if path != doc:
                linked.add(path)
            if not path.exists():
                report.add(KIND_LINK, doc, target,
                           f"line {line.number}: link target does not exist", root)
                continue
            if anchor and path.suffix.lower() == ".md":
                if path not in anchors:
                    try:
                        anchors[path] = anchors_for(path.read_text(encoding="utf-8"))
                    except OSError:
                        continue
                if urllib.parse.unquote(anchor) not in anchors[path]:
                    report.add(KIND_ANCHOR, doc, target,
                               f"line {line.number}: no heading or anchor {anchor!r}",
                               root)
    return linked


def check_orphans(report: Report, files: list[Path], linked: set[Path],
                  root: Path) -> None:
    design = root / "docs" / "design"
    if not design.is_dir():
        return
    for path in sorted(design.glob("*.md")):
        rel = relative(path, root)
        if rel in ORPHAN_ENTRY_POINTS:
            continue
        if path.resolve() in linked:
            continue
        report.add(KIND_ORPHAN, path, "",
                   "no scanned document links to this design doc", root,
                   advisory=True)


# ── clap model ────────────────────────────────────────────────────────────

@dataclass
class Flag:
    name: str
    takes_value: bool
    global_: bool = False


@dataclass
class Node:
    name: str
    flags: dict[str, Flag] = field(default_factory=dict)
    aliases: dict[str, str] = field(default_factory=dict)
    subcommands: dict[str, "Node"] = field(default_factory=dict)
    has_positionals: bool = False


@dataclass
class FieldDef:
    name: str
    type: str
    attrs: list[str] = field(default_factory=list)


@dataclass
class VariantDef:
    name: str
    type: str
    attrs: list[str] = field(default_factory=list)
    fields: list[FieldDef] = field(default_factory=list)


@dataclass
class TypeDef:
    name: str
    module: str
    fields: list[FieldDef] = field(default_factory=list)
    variants: list[VariantDef] = field(default_factory=list)

    @property
    def qualified(self) -> str:
        return f"{self.module}::{self.name}" if self.module else self.name


@dataclass
class Module:
    name: str
    types: dict[str, TypeDef] = field(default_factory=dict)
    imports: dict[str, str] = field(default_factory=dict)


def kebab(name: str) -> str:
    return re.sub(r"(?<=[a-z0-9])(?=[A-Z])", "-", name).replace("_", "-").lower()


def _skip_quoted(text: str, index: int) -> int:
    quote = text[index]
    index += 1
    while index < len(text):
        char = text[index]
        if char == "\\":
            index += 2
            continue
        if char == quote:
            return index + 1
        index += 1
    return index


def _block(text: str, start: int) -> tuple[str, int]:
    """Return the brace-delimited block starting at ``text[start]``."""
    assert text[start] == "{"
    depth = 0
    index = start
    while index < len(text):
        char = text[index]
        if char == '"':
            index = _skip_quoted(text, index)
            continue
        if char == "/" and text.startswith("//", index):
            newline = text.find("\n", index)
            index = len(text) if newline < 0 else newline
            continue
        if char == "{":
            depth += 1
        elif char == "}":
            depth -= 1
            if depth == 0:
                return text[start + 1:index], index + 1
        index += 1
    return text[start + 1:], len(text)


def _attribute(lines: list[str], index: int) -> tuple[str, int]:
    """Read one ``#[...]`` attribute, which may span several lines.

    Returns the text between ``#[`` and the matching ``]`` plus the index of
    the next unconsumed line. Accumulating on bracket balance keeps a
    hand-formatted multi-line ``#[arg(`` declaration parseable.
    """
    text = lines[index].strip()
    consumed = 1
    while not _attribute_complete(text) and index + consumed < len(lines):
        text += " " + lines[index + consumed].strip()
        consumed += 1
    text = re.sub(r"\s+", " ", text)
    if text.startswith("#["):
        text = text[2:]
    if text.endswith("]"):
        text = text[:-1]
    return text.strip(), index + consumed


def _attribute_complete(text: str) -> bool:
    """True when the attribute's opening ``[`` is closed.

    Quoted strings are skipped so a help text containing an unmatched
    bracket cannot swallow the following field.
    """
    depth = 0
    index = 0
    while index < len(text):
        char = text[index]
        if char in "\"'":
            index = _skip_quoted(text, index)
            continue
        if char == "\\":
            index += 2
            continue
        if char == "[":
            depth += 1
        elif char == "]":
            depth -= 1
            if depth == 0:
                return True
        index += 1
    return False


def parse_fields(body: str) -> list[FieldDef]:
    fields: list[FieldDef] = []
    pending: list[str] = []
    lines = body.splitlines()
    index = 0
    while index < len(lines):
        stripped = lines[index].strip()
        if not stripped or stripped.startswith("//"):
            index += 1
            continue
        if stripped.startswith("#["):
            attribute, index = _attribute(lines, index)
            pending.append(attribute)
            continue
        match = re.match(
            r"(?:pub(?:\([^)]*\))?\s+)?(?:r#)?([A-Za-z_]\w*)\s*:\s*(.+?)\s*,?\s*$",
            stripped)
        if match:
            fields.append(FieldDef(match.group(1), match.group(2).strip(), pending))
            pending = []
        else:
            # An attribute belongs to the declaration that follows it; a line
            # that is not a field must not leak it onto a later one.
            pending = []
        index += 1
    return fields


def parse_variants(body: str) -> list[VariantDef]:
    variants: list[VariantDef] = []
    pending: list[str] = []
    lines = body.splitlines()
    index = 0
    while index < len(lines):
        stripped = lines[index].strip()
        if stripped.startswith("#["):
            attribute, index = _attribute(lines, index)
            pending.append(attribute)
            continue
        if not stripped or stripped.startswith("//"):
            index += 1
            continue
        match = re.match(r"([A-Z]\w*)\s*(.*)", stripped)
        if not match:
            index += 1
            continue
        name, rest = match.group(1), match.group(2).strip()
        variant = VariantDef(name, "", pending)
        pending = []
        if rest.startswith("{"):
            depth = 0
            chunk: list[str] = []
            while index < len(lines):
                line = lines[index]
                depth += line.count("{") - line.count("}")
                chunk.append(line)
                index += 1
                if depth <= 0:
                    break
            variant.fields = parse_fields("\n".join(chunk[0].split("{", 1)[-1:]
                                                    + chunk[1:]))
        elif rest.startswith("("):
            payload = rest[1:].split(")", 1)[0].strip()
            variant.type = strip_wrappers(payload)
            index += 1
        else:
            index += 1
        variants.append(variant)
    return variants


def strip_wrappers(type_name: str) -> str:
    type_name = type_name.strip()
    match = re.fullmatch(r"(?:[\w:]+::)?(?:Option|Vec|Box|Arc|Cow|Rc)<(.+)>", type_name)
    if match:
        return strip_wrappers(match.group(1))
    return type_name.strip()


DECL = re.compile(
    r"^(?:pub(?:\([^)]*\))?\s+)?(?P<kind>struct|enum)\s+(?P<name>[A-Za-z_]\w*)[^{;]*\{",
    re.MULTILINE,
)


def _module_name(path: Path, source_root: Path) -> str:
    parts = list(path.relative_to(source_root).with_suffix("").parts)
    if parts and parts[-1] in ("mod", "lib", "main"):
        parts.pop()
    return "::".join(parts)


def _normalize_use(module: str, path: str) -> str:
    parts = [part for part in path.split("::") if part]
    if parts[:1] == ["crate"]:
        return "::".join(parts[1:])
    if parts[:1] == ["super"]:
        base = module.split("::")
        while parts and parts[0] == "super":
            base = base[:-1]
            parts = parts[1:]
        return "::".join(base + parts)
    if parts[:1] == ["self"]:
        return "::".join([part for part in module.split("::") + parts[1:] if part])
    return "::".join(parts)


def parse_imports(module: str, text: str) -> dict[str, str]:
    imports: dict[str, str] = {}
    for match in re.finditer(r"^\s*use\s+([^;]+);", text, re.MULTILINE):
        path = match.group(1).strip()
        prefix = ""
        if "{" in path:
            head, _, rest = path.partition("{")
            prefix = head.strip().rstrip(":").strip()
            pieces = [piece.strip() for piece in rest.split("}")[0].split(",")]
            entries = [f"{prefix}::{piece}" if prefix else piece for piece in pieces if piece]
        else:
            entries = [path]
        for entry in entries:
            entry = entry.strip()
            if not entry or entry == "self" or entry.startswith("{"):
                continue
            alias = None
            if " as " in entry:
                entry, alias = [part.strip() for part in entry.split(" as ", 1)]
            qualified = _normalize_use(module, entry)
            name = alias or qualified.split("::")[-1]
            imports[name] = qualified
    return imports


def cli_modules(root: Path) -> dict[str, Module]:
    """Parse the CLI crate into per-module type tables."""
    source = root / "crates" / "remuda" / "src"
    modules: dict[str, Module] = {}
    if not source.is_dir():
        return modules
    for path in sorted(source.rglob("*.rs")):
        try:
            text = path.read_text(encoding="utf-8")
        except (OSError, UnicodeDecodeError):
            continue
        module_name = _module_name(path, source)
        module = Module(module_name, imports=parse_imports(module_name, text))
        for match in DECL.finditer(text):
            body, _ = _block(text, match.end() - 1)
            definition = TypeDef(match.group("name"), module_name)
            if match.group("kind") == "enum":
                definition.variants = parse_variants(body)
            else:
                definition.fields = parse_fields(body)
            if definition.fields or definition.variants:
                module.types[definition.name] = definition
        modules[module_name] = module
    return modules


def resolve_type(modules: dict[str, Module], module: str, reference: str) -> TypeDef | None:
    reference = strip_wrappers(reference)
    if "::" in reference:
        if reference.startswith(("crate::", "super::", "self::")):
            paths = [_normalize_use(module, reference)]
        else:
            paths = [f"{module}::{reference}" if module else reference, reference]
        for path in paths:
            owner, _, name = path.rpartition("::")
            found = modules.get(owner)
            if found and name in found.types:
                return found.types[name]
        return None
    same = modules.get(module)
    if same and reference in same.types:
        return same.types[reference]
    if same:
        imported = same.imports.get(reference)
        if imported:
            owner, _, name = imported.rpartition("::")
            found = modules.get(owner)
            if found and name in found.types:
                return found.types[name]
    matches = [definition for table in modules.values()
               for definition in table.types.values() if definition.name == reference]
    return matches[0] if len(matches) == 1 else None


def _arg_parts(attr: str) -> list[str]:
    parts, depth, current = [], 0, []
    for char in attr:
        if char in "([{":
            depth += 1
        elif char in ")]}":
            depth -= 1
        if char == "," and depth == 0:
            parts.append("".join(current).strip())
            current = []
        else:
            current.append(char)
    if current:
        parts.append("".join(current).strip())
    return [part for part in parts if part]


def flags_for(definition: TypeDef, modules: dict[str, Module],
              seen: set[str] | None = None) -> dict[str, Flag]:
    seen = set() if seen is None else seen
    if definition.qualified in seen:
        return {}
    seen.add(definition.qualified)
    flags: dict[str, Flag] = {}
    for item in definition.fields:
        arg_attrs = [a for a in item.attrs if a.startswith("arg(")]
        if any(a.startswith("command(") and re.search(r"\bflatten\b", a)
               for a in item.attrs):
            nested = resolve_type(modules, definition.module, item.type)
            if nested:
                flags.update(flags_for(nested, modules, seen))
            continue
        if not arg_attrs:
            continue
        args = _arg_parts(arg_attrs[0][len("arg("):-1])
        long_name = None
        short = None
        is_global = False
        aliases: list[str] = []
        for part in args:
            key, _, value = part.partition("=")
            key = key.strip()
            value = value.strip()
            if key == "long":
                long_name = value.strip("\"'") if value else kebab(item.name)
            elif key == "short":
                short = value.strip("'\"") if value else item.name[0]
            elif key in ("visible_alias", "alias"):
                alias = value.strip("\"'")
                if alias:
                    aliases.append(alias)
            elif key == "global" and value == "true":
                is_global = True
        takes_value = strip_wrappers(item.type) != "bool"
        if long_name:
            flags[f"--{long_name}"] = Flag(f"--{long_name}", takes_value, is_global)
        for alias in aliases:
            flags[f"--{alias}"] = Flag(f"--{alias}", takes_value, is_global)
        if short:
            flags[f"-{short}"] = Flag(f"-{short}", takes_value, is_global)
    return flags


def _command_name(variant: VariantDef) -> str:
    for attr in variant.attrs:
        if attr.startswith("command("):
            match = re.search(r"\bname\s*=\s*\"([^\"]+)\"", attr)
            if match:
                return match.group(1)
    return kebab(variant.name)


def _aliases(variant: VariantDef) -> list[str]:
    aliases = []
    for attr in variant.attrs:
        for match in re.finditer(r"(?:visible_alias|alias)\s*=\s*\"([^\"]+)\"", attr):
            aliases.append(match.group(1))
    return aliases


def _is_positional(item: FieldDef) -> bool:
    """A plain field, or an `#[arg(...)]` with neither a long nor a short name."""
    if any(a.startswith("command(") for a in item.attrs):
        return False
    arg_attrs = [a for a in item.attrs if a.startswith("arg(")]
    if not arg_attrs:
        return True
    return not any(re.search(r"\b(?:long|short)\b", attr) for attr in arg_attrs)


def build_node(name: str, definition: TypeDef, modules: dict[str, Module],
               seen: set[str] | None = None) -> Node:
    seen = set() if seen is None else seen
    node = Node(name, flags_for(definition, modules, set(seen)))
    seen.add(definition.qualified)
    for item in definition.fields:
        if any(a.startswith("command(") and re.search(r"\bsubcommand\b", a)
               for a in item.attrs):
            enum = resolve_type(modules, definition.module, item.type)
            if not enum:
                continue
            for variant in enum.variants:
                child_def = (resolve_type(modules, enum.module, variant.type)
                             if variant.type else None)
                fields = variant.fields or (child_def.fields if child_def else [])
                inline = TypeDef(f"{enum.name}{variant.name}", enum.module, fields)
                child_name = _command_name(variant)
                node.subcommands[child_name] = build_node(
                    child_name, inline, modules, set(seen))
                if child_def and child_def.qualified not in seen:
                    deeper = build_node(child_name, child_def, modules, set(seen))
                    child = node.subcommands[child_name]
                    child.flags.update(deeper.flags)
                    child.subcommands.update(deeper.subcommands)
                    child.aliases.update(deeper.aliases)
                    child.has_positionals = child.has_positionals or deeper.has_positionals
                for alias in _aliases(variant):
                    node.aliases[alias] = child_name
                node.subcommands[child_name].has_positionals = (
                    node.subcommands[child_name].has_positionals
                    or any(_is_positional(field) for field in fields))
            continue
        if any(a.startswith("command(") and re.search(r"\bflatten\b", a)
               for a in item.attrs):
            # A flattened struct contributes flags, and possibly its own
            # subcommands and aliases.
            nested = resolve_type(modules, definition.module, item.type)
            if nested and nested.qualified not in seen:
                deeper = build_node(item.name, nested, modules, set(seen))
                node.flags.update(deeper.flags)
                node.subcommands.update(deeper.subcommands)
                node.aliases.update(deeper.aliases)
                node.has_positionals = node.has_positionals or deeper.has_positionals
            continue
        if _is_positional(item):
            node.has_positionals = True
    return node


def clap_root(root: Path) -> Node | None:
    modules = cli_modules(root)
    source = root / "crates" / "remuda" / "src" / "cmd" / "mod.rs"
    if not source.is_file():
        return None
    text = source.read_text(encoding="utf-8")
    match = re.search(r"commands!\s*\{", text)
    if not match:
        return None
    body, _ = _block(text, match.end() - 1)
    root_node = Node("remuda")
    for variant_name, type_name in re.findall(
            r"([A-Z]\w*)\s*\(\s*([\w:]+)\s*\)", body):
        definition = resolve_type(modules, "cmd", type_name)
        if not definition:
            continue
        child = build_node(kebab(variant_name), definition, modules)
        root_node.subcommands[child.name] = child
    # Root flags come from the flattened context on the clap parser struct.
    main = root / "crates" / "remuda" / "src" / "main.rs"
    if main.is_file():
        main_text = main.read_text(encoding="utf-8")
        parser = re.search(r"#\[derive\([^)]*Parser[^)]*\)\][^{]*\{", main_text)
        if parser:
            cli_body, _ = _block(main_text, parser.end() - 1)
            for item in parse_fields(cli_body):
                if any(re.search(r"\bflatten\b", a) for a in item.attrs):
                    nested = resolve_type(modules, "", item.type)
                    if nested:
                        root_node.flags.update(flags_for(nested, modules))
    return root_node


# ── command references ────────────────────────────────────────────────────

def just_recipes(root: Path) -> set[str] | None:
    path = root / "justfile"
    if not path.is_file():
        return None
    names = set()
    # `name [args]: [dependencies] [# comment]`. A bare `:=` assignment is not
    # a recipe.
    recipe = re.compile(
        r"^@?(?P<name>[A-Za-z_][\w-]*)(?:\s+[^:]*)?:\s*(?!=)[^#\n]*(?:#.*)?$")
    alias = re.compile(r"^alias\s+(?P<name>[A-Za-z_][\w-]*)\s*:=")
    for line in path.read_text(encoding="utf-8").splitlines():
        if line.startswith((" ", "\t")):
            continue
        match = recipe.match(line)
        if match:
            names.add(match.group("name"))
            continue
        match = alias.match(line)
        if match:
            names.add(match.group("name"))
    return names


def package_scripts(root: Path) -> set[str] | None:
    path = root / "web" / "package.json"
    if not path.is_file():
        return None
    try:
        data = json.loads(path.read_text(encoding="utf-8"))
    except (OSError, ValueError):
        return None
    scripts = data.get("scripts")
    return set(scripts) if isinstance(scripts, dict) else None


def shell_tokens(text: str) -> list[str]:
    """Split a command reference, keeping <placeholders> and quotes whole."""
    tokens: list[str] = []
    index = 0
    while index < len(text):
        char = text[index]
        if char.isspace():
            index += 1
            continue
        if char == "<":
            end = text.find(">", index)
            end = len(text) - 1 if end < 0 else end
            tokens.append(text[index:end + 1])
            index = end + 1
            continue
        if char in "\"'":
            end = index + 1
            while end < len(text) and text[end] != char:
                end += 2 if text[end] == "\\" else 1
            tokens.append(text[index + 1:end])
            index = end + 1
            continue
        end = index
        while end < len(text) and not text[end].isspace():
            end += 1
        tokens.append(text[index:end])
        index = end
    return tokens


def is_placeholder(token: str) -> bool:
    return (
        token in ("...", "…")
        or token.startswith(("<", "[", "{", "(", "|", "*"))
        or any(ch in token for ch in "<>|/…")
        or "|" in token
        or re.fullmatch(r"[A-Z][A-Z0-9_]*", token) is not None
        or re.fullmatch(r"[A-Z]", token) is not None
    )


def _chain_flags(node_chain: list[Node]) -> dict[str, Flag]:
    """Flags accepted at the deepest command: its own, plus inherited globals."""
    flags: dict[str, Flag] = {}
    last = len(node_chain) - 1
    for depth, node in enumerate(node_chain):
        current = depth == last
        for name, flag in node.flags.items():
            if current or depth == 0 or flag.global_:
                flags.setdefault(name, flag)
    return flags


UNIVERSAL_FLAGS = {"--help", "-h", "--version", "-V"}


def check_remuda(text: str, model: Node) -> list[str]:
    tokens = shell_tokens(text.strip())
    if not tokens or tokens[0] != "remuda":
        return []
    chain = [model]
    problems: list[str] = []
    seen_positional = False
    index = 1
    while index < len(tokens):
        token = tokens[index]
        if token == "--":
            break
        if token.startswith("-") and token != "-":
            if "/" in token.split("=", 1)[0]:
                # A flag list such as `--once/--follow`: cannot judge reliably.
                break
            names = [token.split("=", 1)[0]]
            known = _chain_flags(chain)
            if not token.startswith("--") and len(names[0]) > 2:
                # A combined or value-attached short flag (`-abc`, `-oFILE`).
                # Validate only the first letter so formatting cannot fail.
                if names[0][:2] in known or names[0][:2] in UNIVERSAL_FLAGS:
                    index += 1
                    continue
            if all(name in known or name in UNIVERSAL_FLAGS for name in names):
                flag = known.get(names[-1])
                index += 1
                if (flag is not None and "=" not in token and flag.takes_value
                        and index < len(tokens)):
                    index += 1
                continue
            problems.append(f"unknown flag {token.split('=')[0]}")
            break
        if "/" in token or "|" in token:
            # A shorthand list such as `create/send/wait` cannot be resolved
            # reliably; do not judge the rest of this reference.
            break
        if is_placeholder(token):
            index += 1
            continue
        if seen_positional:
            index += 1
            continue
        node = chain[-1]
        if token == "help":
            # clap's generated help subcommand takes any topic.
            break
        if not node.subcommands:
            break
        child_name = node.aliases.get(token, token)
        child = node.subcommands.get(child_name)
        if child:
            chain.append(child)
            index += 1
            continue
        if node.has_positionals:
            seen_positional = True
            index += 1
            continue
        if not re.fullmatch(r"[a-z][a-z0-9_-]*", token):
            break
        chain_text = " ".join(part.name for part in chain)
        problems.append(f"unknown subcommand {token!r} for `{chain_text}`")
        break
    return problems


# Global just flags whose next token is a value, not a recipe name.
JUST_VALUE_FLAGS = {
    "-f", "--justfile", "-d", "--working-directory", "--set", "--shell",
    "--shell-arg", "--command-color", "--dotenv-path", "--dotenv-filename",
    "--list-heading", "--list-prefix", "--chooser", "--tempdir",
}


def check_just(text: str, recipes: set[str] | None) -> list[str]:
    if recipes is None:
        return []
    tokens = shell_tokens(text.strip())
    if len(tokens) < 2 or tokens[0] != "just":
        return []
    candidates: list[str] = []
    index = 1
    while index < len(tokens):
        token = tokens[index]
        if token.startswith("-"):
            if "=" in token:
                index += 1
            elif token in JUST_VALUE_FLAGS:
                index += 2
            else:
                index += 1
            continue
        candidates.append(token)
        index += 1
    if any(candidate in recipes for candidate in candidates):
        return []
    if candidates:
        return [f"unknown just recipe {candidates[0]!r}"]
    return []


PNPM_BUILTINS = {
    "add", "audit", "bin", "config", "create", "dedupe", "deploy", "dlx",
    "env", "exec", "fetch", "import", "init", "install", "licenses", "link",
    "list", "ls", "outdated", "pack", "patch", "patch-commit", "prune",
    "publish", "rebuild", "recursive", "remove", "root", "server", "store",
    "uninstall", "unlink", "update", "why", "workspace", "run", "run-script",
    "-r", "-w", "-C", "--filter",
}


def check_pnpm(text: str, scripts: set[str] | None) -> list[str]:
    if scripts is None:
        return []
    tokens = shell_tokens(text.strip())
    if len(tokens) < 4 or tokens[0] != "pnpm":
        return []
    rest = tokens[1:]
    if rest[:1] == ["--dir=web"]:
        rest = rest[1:]
    elif rest[:2] == ["--dir", "web"]:
        rest = rest[2:]
    else:
        return []
    if not rest:
        return []
    command = rest[0]
    explicit_run = command in ("run", "run-script")
    if explicit_run:
        if len(rest) < 2:
            return []
        command = rest[1]
    elif command in PNPM_BUILTINS:
        return []
    if not re.fullmatch(r"[A-Za-z0-9:_-]+", command):
        # Prose glued to the reference (`test（全量）`) or a script list.
        return []
    if command in scripts:
        return []
    # `pnpm --dir web vitest run ...` invokes a local binary, not a script.
    if not explicit_run and len(rest) > 1:
        return []
    return [f"unknown web package script {command!r}"]


def iter_commands(text: str):
    """Yield command-reference strings from inline code and fenced lines."""
    for match in CODE_SPAN.finditer(text):
        yield match.group(1).strip()
    for line in markdown_lines(text):
        if not line.fenced:
            continue
        stripped = line.text.strip()
        if stripped.startswith(("$ ", "> ")):
            stripped = stripped[2:].strip()
        if re.match(r"(remuda|just|pnpm)\s", stripped):
            yield stripped


def check_commands(report: Report, documents: dict[Path, str], root: Path) -> None:
    model = clap_root(root)
    recipes = just_recipes(root)
    scripts = package_scripts(root)
    for doc, text in documents.items():
        for reference in iter_commands(text):
            if reference.startswith("remuda"):
                problems = check_remuda(reference, model) if model else []
            elif reference.startswith("just"):
                problems = check_just(reference, recipes)
            elif reference.startswith("pnpm"):
                problems = check_pnpm(reference, scripts)
            else:
                continue
            for problem in problems:
                # Command references are advisory: the clap/just/pnpm models
                # are hand-parsed, and a parser gap must never fail a branch.
                report.add(KIND_COMMAND, doc, reference, problem, root,
                           advisory=True)


# ── file:line anchors ─────────────────────────────────────────────────────

def source_index(root: Path, use_git: bool = True) -> dict[str, list[Path]]:
    """Source files by basename, from the git index when available."""
    tracked = tracked_files(root) if use_git else None
    candidates = tracked if tracked is not None else _walk_files(root)
    index: dict[str, list[Path]] = {}
    for path in candidates:
        rel = path.relative_to(root)
        if _skipped(rel):
            continue
        if path.suffix in (".rs", ".py", ".ts", ".tsx", ".js", ".jsx"):
            index.setdefault(path.name, []).append(path)
    return index


TOP_LEVEL_DIRS = ("crates", "web", "scripts", "deploy", "skills", "docs")


def _suffix_matches(index: dict[str, list[Path]], raw: str) -> list[Path]:
    suffix = "/" + raw
    return [path for paths in index.values() for path in paths
            if path.as_posix().endswith(suffix)]


def resolve_source(doc: Path, raw: str, root: Path,
                   previous: dict[str, Path],
                   index: dict[str, list[Path]]) -> Path | None:
    if raw.split("/", 1)[0] in TOP_LEVEL_DIRS:
        return (root / raw).resolve()
    if "/" in raw:
        beside = (doc.parent / raw).resolve()
        if beside.is_file():
            return beside
        matches = _suffix_matches(index, raw)
        return matches[0] if len(matches) == 1 else None
    if raw in previous:
        return previous[raw]
    # A bare basename is shorthand: resolve it only when the document already
    # named a full path or the repository has exactly one file with that name.
    # An ambiguous shorthand is not a contract violation.
    matches = index.get(raw, [])
    return matches[0] if len(matches) == 1 else None


PATHISH = re.compile(
    r"[/\\]|^[\w.@+-]+\.(?:rs|py|ts|tsx|js|jsx|svelte|toml|sh|json|md|css|html)\b|"
    r":\d+(?:[-–]\d+)?\b|^https?$"
)
# Language and prose tokens that occur all over source files; matching them
# near an anchor proves nothing.
GENERIC_SYMBOLS = {
    "true", "false", "null", "none", "some", "ok", "err", "self", "crate",
    "string", "vec", "option", "result", "bool", "usize", "isize", "u8",
    "u16", "u32", "u64", "u128", "i8", "i16", "i32", "i64", "i128", "str",
    "json", "http", "https", "utf", "ascii", "kib", "mib", "gib", "text",
    "image", "audio", "file", "human", "bot", "none", "default", "new",
}


def _is_specific(name: str) -> bool:
    """True when a token reads like a code symbol, not prose or a builtin."""
    if name.lower() in GENERIC_SYMBOLS:
        return False
    if "_" in name.strip("_"):
        return True
    return len(re.findall(r"[A-Z]", name)) >= 2 or re.search(r"[a-z][A-Z]", name) is not None


def _candidates(line: str, anchor_text: str) -> list[str]:
    names: list[str] = []
    for span in CODE_SPAN.findall(line):
        if anchor_text in span or PATHISH.search(span):
            continue
        for token in IDENTIFIER.findall(span):
            for piece in token.split("::"):
                piece = piece.strip("(){}[],.:'\"` ")
                if len(piece) < 4 or not _is_specific(piece):
                    continue
                if piece not in names:
                    names.append(piece)
    return names


def _explicit_anchor_target(raw: str, doc: Path, root: Path,
                            previous: dict[str, Path]) -> bool:
    """True when the citation names its file without a repo-wide guess.

    A bare basename is explicit only after the document named a full path
    for it. Heuristic matches (unique basename anywhere, partial suffix) may
    change when an unrelated branch adds a same-named file, so their
    existence/line-count results stay advisory.
    """
    if raw.split("/", 1)[0] in TOP_LEVEL_DIRS:
        return True
    if "/" in raw:
        return (doc.parent / raw).resolve().is_file()
    return raw in previous


def check_file_anchors(report: Report, documents: dict[Path, str], root: Path,
                       use_git: bool = True) -> None:
    index = source_index(root, use_git)
    cache: dict[Path, list[str]] = {}

    def source_lines_of(path: Path) -> list[str] | None:
        if path not in cache:
            try:
                cache[path] = path.read_text(encoding="utf-8", errors="replace").splitlines()
            except OSError:
                return None
        return cache[path]

    for doc, text in documents.items():
        previous: dict[str, Path] = {}
        lines = text.splitlines()
        for source_line, line in enumerate(lines, 1):
            for match in FILE_ANCHOR.finditer(line):
                raw = match.group("path")
                start = int(match.group("start"))
                end = int(match.group("end") or start)
                anchor_text = match.group(0)
                if _skipped(Path(raw)):
                    # Generated or vendored trees are not part of the contract.
                    continue
                path = resolve_source(doc, raw, root, previous, index)
                explicit = _explicit_anchor_target(raw, doc, root, previous)
                if path is not None and raw.split("/", 1)[0] in TOP_LEVEL_DIRS:
                    previous[Path(raw).name] = path
                if path is None:
                    # Ambiguous shorthand; the document never named a full path.
                    continue
                if not path.is_file():
                    report.add(KIND_FILE_ANCHOR, doc, anchor_text,
                               f"line {source_line}: file does not exist", root,
                               advisory=not explicit)
                    continue
                source_lines = source_lines_of(path)
                if source_lines is None:
                    report.add(KIND_FILE_ANCHOR, doc, anchor_text,
                               f"line {source_line}: file cannot be read", root,
                               advisory=not explicit)
                    continue
                if "/" not in raw and (start < 1 or end > len(source_lines)):
                    # The most recent full path with this basename can point at
                    # another file with the same name. Before reporting a stale
                    # line, prefer a same-named file that is long enough, first
                    # in a directory a sibling anchor on this line names.
                    candidates = [candidate for candidate in index.get(raw, [])
                                  if candidate != path
                                  and len(source_lines_of(candidate) or []) >= end]
                    hints = {
                        resolved.parent
                        for other in FILE_ANCHOR.finditer(line)
                        if other.group(0) != anchor_text
                        for resolved in [resolve_source(doc, other.group("path"), root,
                                                        previous, index)]
                        if resolved is not None
                    }
                    preferred = [candidate for candidate in candidates
                                 if candidate.parent in hints]
                    if len(preferred) == 1:
                        path = preferred[0]
                    elif len(candidates) == 1:
                        path = candidates[0]
                    elif candidates:
                        # Several plausible files: not a contract violation.
                        continue
                    source_lines = source_lines_of(path)
                    if source_lines is None:
                        continue
                if start < 1 or end > len(source_lines):
                    report.add(KIND_FILE_ANCHOR, doc, anchor_text,
                               f"line {source_line}: {raw} has {len(source_lines)} "
                               f"lines, anchor asks for {start}-{end}", root,
                               advisory=not explicit)
                    continue
                source = "\n".join(source_lines)
                candidates = [name for name in _candidates(line, anchor_text)
                              if re.search(rf"\b{re.escape(name)}\b", source)]
                if not candidates:
                    continue
                low = max(1, start - SYMBOL_WINDOW)
                high = min(len(source_lines), end + SYMBOL_WINDOW)
                window = "\n".join(source_lines[low - 1:high])
                if not any(re.search(rf"\b{re.escape(name)}\b", window)
                           for name in candidates):
                    report.add(
                        KIND_FILE_ANCHOR, doc, anchor_text,
                        f"line {source_line}: {', '.join(candidates[:3])} not "
                        f"within {SYMBOL_WINDOW} lines of {raw}:{start}", root,
                        advisory=True)


# ── driver ────────────────────────────────────────────────────────────────

def scan(root: Path, use_git: bool = True) -> Report:
    files = markdown_files(root, use_git)
    report = Report(files)
    if not files:
        raise SystemExit(f"docs contract: no markdown files scanned under {root}")
    documents: dict[Path, str] = {}
    for path in files:
        try:
            documents[path.resolve()] = path.read_text(encoding="utf-8")
        except (OSError, UnicodeDecodeError) as exc:
            raise SystemExit(f"docs contract: cannot read {relative(path, root)}: {exc}")
    anchors: dict[Path, set[str]] = {
        path: anchors_for(text) for path, text in documents.items()
    }
    linked = check_links(report, documents, anchors, root)
    check_orphans(report, files, linked, root)
    check_file_anchors(report, documents, root, use_git)
    check_commands(report, documents, root)
    return report


def print_report(report: Report, baseline: set[str], strict: bool,
                 stream=sys.stdout) -> bool:
    """Print the report; return True when the mode must fail."""
    enforced = report.enforced()
    advisory = report.advisory()
    new = sorted(enforced - baseline)
    vanished = sorted(baseline - enforced)
    print(f"docs contract: scanned {len(report.files)} markdown files", file=stream)
    for kind in KINDS:
        total = report.count(kind)
        held = report.count(kind, advisory=False)
        warning = report.count(kind, advisory=True)
        fresh = sum(1 for key in new if key.split("|", 1)[0] == kind)
        detail = f"{total} violation(s) ({held} enforced"
        if warning:
            detail += f", {warning} advisory"
        detail += ")"
        print(f"  {kind}: {detail}, {fresh} new enforced", file=stream)
    by_key = report.violations
    for key in new:
        print(f"NEW {key}: {by_key[key].message}", file=stream)
    if strict:
        for key in vanished:
            print(f"STALE {key}: baselined violation is gone; "
                  f"run --update-baseline to shrink", file=stream)
        for key in sorted(advisory):
            print(f"ADVISORY {key}: {by_key[key].message}", file=stream)
    else:
        print(f"docs contract: advisory: {len(advisory)} item(s) "
              f"(symbol proximity, command references, orphan docs) and "
              f"{len(vanished)} vanished baseline "
              f"entr{'y' if len(vanished) == 1 else 'ies'} (warnings only)",
              file=stream)
        if vanished:
            print("docs contract: run --update-baseline to shrink the baseline",
                  file=stream)
        print("docs contract: run --strict for the advisory details", file=stream)
    failed = bool(new) or (strict and bool(vanished or advisory))
    if not failed:
        print("docs contract: no new enforced violations", file=stream)
    return failed


def run_check(root: Path, baseline_path: Path, stream=sys.stdout,
              strict: bool = False) -> int:
    report = scan(root)
    if not baseline_path.is_file():
        print(f"docs contract: baseline missing: {relative(baseline_path, root)}",
              file=stream)
        return 2
    try:
        baseline = load_baseline(baseline_path)
    except (OSError, ValueError, json.JSONDecodeError) as exc:
        print(f"docs contract: cannot read baseline: {exc}", file=stream)
        return 2
    return 1 if print_report(report, baseline, strict, stream) else 0


def update_baseline(root: Path, baseline_path: Path, stream=sys.stdout) -> int:
    if not baseline_path.is_file():
        print("docs contract: no baseline to shrink; refusing to create one",
              file=stream)
        return 2
    try:
        baseline = load_baseline(baseline_path)
    except (OSError, ValueError, json.JSONDecodeError) as exc:
        print(f"docs contract: cannot read baseline: {exc}", file=stream)
        return 2
    report = scan(root)
    current = report.enforced()
    added = sorted(current - baseline)
    if added:
        print("docs contract: --update-baseline may only remove entries; "
              "fix these new violations instead:", file=stream)
        for key in added:
            print(f"NEW {key}: {report.violations[key].message}", file=stream)
        return 1
    removed = sorted(baseline - current)
    if removed:
        dump_baseline(baseline_path, current)
        for key in removed:
            print(f"REMOVED {key}", file=stream)
    print(f"docs contract: baseline shrunk by {len(removed)} "
          f"({len(current)} remaining)", file=stream)
    return 0


FIXTURE_ROOT = Path(__file__).resolve().parent / "fixtures" / "docs_contract"


class DocsContractFixtureTests(unittest.TestCase):
    """The synthetic corpus under scripts/tests/fixtures/docs_contract."""

    # Every fixture violation, by stable baseline key. Exact equality is the
    # point: a new false positive fails this test just like a missed check.
    EXPECTED = {
        "anchor|README.md|docs/guide.md#no-such-heading",
        "command|README.md|just nope",
        "command|README.md|pnpm --dir web nope",
        "command|README.md|remuda demo nope",
        "command|README.md|remuda demo run --slow",
        "file-anchor|docs/anchors.md|crates/demo/src/lib.rs:11",
        "file-anchor|docs/anchors.md|crates/demo/src/lib.rs:900",
        "file-anchor|docs/anchors.md|crates/demo/src/nope.rs:1",
        "file-anchor|docs/anchors.md|only.rs:900",
        "link|README.md|docs/missing.md",
        "orphan|docs/design/orphan.md|",
    }

    def test_fixture_scan_reports_exactly_the_known_violations(self):
        report = scan(FIXTURE_ROOT, use_git=False)
        self.assertEqual(set(report.violations), self.EXPECTED)
        advisory = {
            "file-anchor|docs/anchors.md|crates/demo/src/lib.rs:11",
            "file-anchor|docs/anchors.md|only.rs:900",
            "orphan|docs/design/orphan.md|",
            "command|README.md|just nope",
            "command|README.md|pnpm --dir web nope",
            "command|README.md|remuda demo nope",
            "command|README.md|remuda demo run --slow",
        }
        self.assertEqual(report.advisory(), advisory)
        self.assertEqual(report.enforced(), self.EXPECTED - advisory)
        self.assertGreater(len(report.files), 0)

    def test_paths_with_spaces_and_cjk_are_scanned_and_resolve(self):
        report = scan(FIXTURE_ROOT, use_git=False)
        scanned = {relative(path, FIXTURE_ROOT) for path in report.files}
        self.assertIn("docs/设计 笔记.md", scanned)
        self.assertIn("docs/space dir/notes.md", scanned)
        self.assertIn("docs/目录 空格/说明.md", scanned)
        flagged = " ".join(f"{v.doc} {v.ref}" for v in report.violations.values())
        self.assertNotIn("设计 笔记", flagged)
        self.assertNotIn("space dir", flagged)
        self.assertNotIn("目录 空格", flagged)

    def test_footnotes_and_unused_definitions_are_not_links(self):
        text = (FIXTURE_ROOT / "README.md").read_text(encoding="utf-8")
        targets = [target for _, target in iter_links(visible_markdown(text))]
        self.assertIn("docs/guide.md#setup-steps", targets)  # shortcut [guide-ref]
        self.assertIn("docs/space dir/notes.md", targets)  # angled definition
        self.assertNotIn("See", targets)  # footnote body is prose
        self.assertNotIn("nowhere.md", targets)  # definition nothing uses

    def test_commonmark_destinations_with_titles_and_parens(self):
        text = (FIXTURE_ROOT / "README.md").read_text(encoding="utf-8")
        targets = [target for _, target in iter_links(visible_markdown(text))]
        self.assertIn("docs/a_(b).md", targets)
        # plain, titled, and via the angled reference definition
        self.assertEqual(targets.count("docs/space dir/notes.md"), 3)

    def test_enforced_record_replaces_an_advisory_with_the_same_key(self):
        report = Report([])
        report.add(KIND_FILE_ANCHOR, "docs/a.md", "x.rs:1", "symbol moved",
                   ROOT, advisory=True)
        report.add(KIND_FILE_ANCHOR, "docs/a.md", "x.rs:1", "file missing", ROOT)
        self.assertEqual(report.enforced(), {"file-anchor|docs/a.md|x.rs:1"})
        self.assertEqual(report.advisory(), set())
        self.assertEqual(report.violations["file-anchor|docs/a.md|x.rs:1"].message,
                         "file missing")

    def test_just_recipe_with_dependencies_is_registered(self):
        recipes = just_recipes(FIXTURE_ROOT)
        self.assertIn("check", recipes)  # `check: build` in the fixture justfile
        self.assertIn("c", recipes)  # `alias c := check`
        self.assertIn("gen-types", recipes)
        self.assertEqual(check_just("just check", recipes), [])
        self.assertEqual(check_just("just c", recipes), [])
        self.assertEqual(check_just("just nope", recipes),
                         ["unknown just recipe 'nope'"])

    @unittest.skipUnless(shutil.which("git"), "needs git")
    def test_untracked_markdown_does_not_change_the_corpus(self):
        with tempfile.TemporaryDirectory() as tmp:
            repo = Path(tmp) / "repo"
            (repo / "docs").mkdir(parents=True)
            (repo / "docs" / "kept.md").write_text("# Kept\n", encoding="utf-8")
            (repo / "README.md").write_text(
                "# Fixture\n\n[kept](docs/kept.md)\n", encoding="utf-8")
            for command in (["init", "-q"], ["add", "README.md", "docs/kept.md"]):
                subprocess.run(["git", "-C", str(repo), *command], check=True,
                               capture_output=True)
            (repo / "docs" / "local.md").write_text(
                "# Local only\n\n[broken](nowhere.md)\n", encoding="utf-8")
            tracked = scan(repo)
            self.assertEqual(
                {relative(path, repo) for path in tracked.files},
                {"README.md", "docs/kept.md"})
            self.assertEqual(tracked.violations, {})
            walked = scan(repo, use_git=False)
            self.assertIn("link|docs/local.md|nowhere.md", walked.violations)

    def test_scan_never_passes_vacuously(self):
        with tempfile.TemporaryDirectory() as empty:
            with self.assertRaises(SystemExit) as raised:
                scan(Path(empty), use_git=False)
            self.assertIn("no markdown files scanned", str(raised.exception))

    def test_heading_slugs_follow_github(self):
        self.assertEqual(github_slug("Setup steps"), "setup-steps")
        self.assertEqual(github_slug("1.4 Space / Tabs 项目工作台（D-024）"),
                         "14-space--tabs-项目工作台d-024")
        self.assertEqual(github_slug("Terminal → agent promotion"),
                         "terminal--agent-promotion")
        self.assertEqual(github_slug("`/m` 与 `/sessions`"), "m-与-sessions")
        anchors = anchors_for("# Repeat\n\n## Repeat\n\n### Repeat\n\n<a id=\"explicit\"></a>")
        self.assertEqual(anchors, {"repeat", "repeat-1", "repeat-2", "explicit"})

    def _baselined_copy(self, tmp: str) -> tuple[Path, Path]:
        repo = Path(tmp) / "repo"
        shutil.copytree(FIXTURE_ROOT, repo)
        baseline = repo / "scripts" / "tests" / "docs_contract_baseline.json"
        baseline.parent.mkdir(parents=True)
        dump_baseline(baseline, scan(repo, use_git=False).enforced())
        return repo, baseline

    def test_baseline_only_shrinks(self):
        with tempfile.TemporaryDirectory() as tmp:
            repo, baseline = self._baselined_copy(tmp)
            self.assertEqual(run_check(repo, baseline, io.StringIO()), 0)

            # Removing an enforced violation leaves a stale baseline entry:
            # advisory in normal mode, a failure under --strict, and removed
            # (only) by --update-baseline.
            readme = repo / "README.md"
            kept = [line for line in readme.read_text(encoding="utf-8").splitlines()
                    if "docs/missing.md" not in line]
            readme.write_text("\n".join(kept) + "\n", encoding="utf-8")
            out = io.StringIO()
            self.assertEqual(run_check(repo, baseline, out), 0)
            self.assertIn("vanished baseline", out.getvalue())
            strict_out = io.StringIO()
            self.assertEqual(run_check(repo, baseline, strict_out, strict=True), 1)
            self.assertIn("STALE", strict_out.getvalue())
            self.assertEqual(update_baseline(repo, baseline, io.StringIO()), 0)
            self.assertNotIn("link|README.md|docs/missing.md", load_baseline(baseline))

            # A new violation is never absorbed by --update-baseline.
            (repo / "docs" / "new.md").write_text(
                "# New\n\n[broken](nowhere.md)\n", encoding="utf-8")
            self.assertEqual(run_check(repo, baseline, io.StringIO()), 1)
            before = baseline.read_text(encoding="utf-8")
            self.assertEqual(update_baseline(repo, baseline, io.StringIO()), 1)
            self.assertEqual(baseline.read_text(encoding="utf-8"), before)

    def test_code_shift_warns_but_does_not_fail_enforced_mode(self):
        with tempfile.TemporaryDirectory() as tmp:
            repo, baseline = self._baselined_copy(tmp)
            before = scan(repo, use_git=False).advisory()
            source = repo / "crates" / "demo" / "src" / "lib.rs"
            lines = source.read_text(encoding="utf-8").splitlines()
            # Code-only change: every anchored symbol moves ten lines down.
            source.write_text("\n".join(lines[:1] + [""] * 10 + lines[1:]) + "\n",
                              encoding="utf-8")
            after = scan(repo, use_git=False).advisory()
            # Prove the shift itself produces the warnings: the anchors that
            # used to resolve now mismatch, and the old mismatch resolves.
            self.assertIn("file-anchor|docs/anchors.md|crates/demo/src/lib.rs:3",
                          after - before)
            self.assertIn("file-anchor|docs/anchors.md|crates/demo/src/lib.rs:6",
                          after - before)
            self.assertNotIn("file-anchor|docs/anchors.md|crates/demo/src/lib.rs:11",
                             after)
            out = io.StringIO()
            self.assertEqual(run_check(repo, baseline, out), 0)
            self.assertIn("advisory", out.getvalue())
            strict_out = io.StringIO()
            self.assertEqual(run_check(repo, baseline, strict_out, strict=True), 1)
            self.assertIn("ADVISORY file-anchor|docs/anchors.md|crates/demo/src/lib.rs:6",
                          strict_out.getvalue())

    def test_new_orphan_design_doc_is_advisory(self):
        with tempfile.TemporaryDirectory() as tmp:
            repo, baseline = self._baselined_copy(tmp)
            before = scan(repo, use_git=False).advisory()
            (repo / "docs" / "design" / "newdoc.md").write_text(
                "# New design\n", encoding="utf-8")
            after = scan(repo, use_git=False).advisory()
            key = "orphan|docs/design/newdoc.md|"
            self.assertIn(key, after - before)
            self.assertEqual(run_check(repo, baseline, io.StringIO()), 0)
            strict_out = io.StringIO()
            self.assertEqual(run_check(repo, baseline, strict_out, strict=True), 1)
            self.assertIn(f"ADVISORY {key}", strict_out.getvalue())

    def test_new_broken_link_fails(self):
        with tempfile.TemporaryDirectory() as tmp:
            repo, baseline = self._baselined_copy(tmp)
            (repo / "docs" / "new.md").write_text(
                "# New\n\n[broken](nowhere.md)\n", encoding="utf-8")
            out = io.StringIO()
            self.assertEqual(run_check(repo, baseline, out), 1)
            self.assertIn("NEW link|docs/new.md|nowhere.md", out.getvalue())

    def test_removed_doc_file_referenced_by_link_fails(self):
        with tempfile.TemporaryDirectory() as tmp:
            repo, baseline = self._baselined_copy(tmp)
            (repo / "docs" / "space dir" / "notes.md").unlink()
            out = io.StringIO()
            self.assertEqual(run_check(repo, baseline, out), 1)
            self.assertIn("NEW link|README.md|docs/space dir/notes.md", out.getvalue())

    def test_strict_fails_on_symbol_mismatches(self):
        with tempfile.TemporaryDirectory() as tmp:
            repo, baseline = self._baselined_copy(tmp)
            self.assertEqual(run_check(repo, baseline, io.StringIO()), 0)
            out = io.StringIO()
            self.assertEqual(run_check(repo, baseline, out, strict=True), 1)
            self.assertIn("ADVISORY file-anchor|docs/anchors.md|crates/demo/src/lib.rs:11",
                          out.getvalue())

    def test_update_baseline_refuses_without_a_baseline(self):
        with tempfile.TemporaryDirectory() as tmp:
            repo = Path(tmp) / "repo"
            shutil.copytree(FIXTURE_ROOT, repo)
            missing = repo / "scripts" / "tests" / "docs_contract_baseline.json"
            missing.parent.mkdir(parents=True)
            self.assertEqual(update_baseline(repo, missing, io.StringIO()), 2)
            self.assertFalse(missing.exists())


class RepositoryContractTests(unittest.TestCase):
    """The checkout's own corpus against its committed baseline.

    Only NEW enforced violations fail here. A branch that fixes a baselined
    violation must stay green in CI; the vanished entry is an advisory
    warning (and a --strict failure) until a maintenance pass shrinks the
    baseline, which is covered by the fixture test instead.
    """

    def test_repository_matches_the_committed_baseline(self):
        report = scan(ROOT)
        self.assertGreater(len(report.files), 0)
        baseline = load_baseline(BASELINE)
        new = sorted(report.enforced() - baseline)
        self.assertEqual(new, [], f"new enforced docs-contract violations: {new}")
        self.assertEqual(run_check(ROOT, BASELINE, io.StringIO()), 0)


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    parser.add_argument("--root", type=Path, default=ROOT,
                        help="repository root to scan (default: this checkout)")
    parser.add_argument("--baseline", type=Path, default=None,
                        help="baseline JSON (default: scripts/tests/docs_contract_baseline.json)")
    mode = parser.add_mutually_exclusive_group()
    mode.add_argument("--check", action="store_true",
                      help="fail on new enforced violations (default)")
    mode.add_argument("--update-baseline", action="store_true",
                      help="remove baseline entries whose violation is gone")
    parser.add_argument("--strict", action="store_true",
                        help="also fail on symbol-proximity mismatches and "
                             "vanished baseline entries (maintenance only)")
    arguments = parser.parse_args(argv)
    root = arguments.root.resolve()
    baseline = arguments.baseline or root / "scripts" / "tests" / "docs_contract_baseline.json"
    try:
        if arguments.update_baseline:
            return update_baseline(root, baseline)
        return run_check(root, baseline, strict=arguments.strict)
    except SystemExit as exc:
        print(str(exc), file=sys.stderr)
        return 2


if __name__ == "__main__":
    sys.exit(main())
