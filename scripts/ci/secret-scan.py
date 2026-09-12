#!/usr/bin/env python3
"""Scan git worktree / index / given paths for undeclared secrets.

Patterns (task impl-acceptance-m0): agk_, sk-, Bearer[space],
ANTHROPIC_AUTH_TOKEN=, api_key. First-4-character redactions and obvious
placeholders/dummies are allowed.
"""

from __future__ import annotations

import os
import re
import subprocess
import sys
from pathlib import Path

# Concatenate so this file does not itself contain the search needles.
P_AGK = "agk" + "_"
P_SK = "sk" + "-"
P_BEARER = "Bearer" + " "
P_TOKEN = "ANTHROPIC_AUTH_TOKEN" + "="
P_API = "api" + "_" + "key"
P_BOOT = "REMUDA_BOOTSTRAP_TOKEN" + "="
P_OPENAI = "OPENAI_API_KEY" + "="
P_XAI = "xai" + "-"
P_GHP = "ghp" + "_"
P_GPAT = "github" + "_pat_"
P_APPSEC = "app" + "_secret"
P_PEM = "BEGIN" + " " + "PRIVATE" + " " + "KEY"

SKIP_DIR_NAMES = {
    ".git",
    "target",
    "node_modules",
    "dist",
    ".grok",
}
SKIP_SUFFIXES = {
    ".png",
    ".jpg",
    ".jpeg",
    ".gif",
    ".ico",
    ".webp",
    ".wasm",
    ".woff",
    ".woff2",
    ".ttf",
    ".sqlite",
    ".db",
    ".lock",
}
SKIP_FILE_NAMES = {
    "secret-scan.py",
    "secret-scan.sh",
    "pnpm-lock.yaml",
    "package-lock.json",
    "Cargo.lock",
    "yarn.lock",
    "composer.lock",
}
MAX_BYTES = 2_000_000
MASK = set("*xX•.…][)(") | {"…"}
DUMMY_PART = re.compile(
    r"(?i)(^|[-_/])(secret|example|placeholder|dummy|redacted|fake|sample|yourkey|xxx+|should-not)([-_]|$)"
)
PLACEHOLDER = re.compile(
    r"^(<[^>]+>|\$\{[^}]+\}|\$[A-Za-z_][A-Za-z0-9_]*|\{[^}]+\})"
)

RULES = [
    (
        "agk_",
        re.compile(r"(?<![A-Za-z0-9_])" + re.escape(P_AGK) + r"[A-Za-z0-9_\-]{8,}"),
    ),
    (
        "sk-",
        re.compile(r"(?<![A-Za-z0-9_])" + re.escape(P_SK) + r"[A-Za-z0-9_\-]{8,}"),
    ),
    (
        "Bearer ",
        re.compile(r"(?<![A-Za-z])" + re.escape(P_BEARER) + r"([A-Za-z0-9._\-+=/]{8,})"),
    ),
    (
        "ANTHROPIC_AUTH_TOKEN=",
        re.compile(re.escape(P_TOKEN) + r"['\"]?([A-Za-z0-9_\-]{8,})"),
    ),
    (
        "api_key",
        re.compile(
            r"\b" + re.escape(P_API) + r"\b\s*[=:]\s*['\"]?([A-Za-z0-9_\-]{8,})"
        ),
    ),
    (
        "REMUDA_BOOTSTRAP_TOKEN=",
        re.compile(re.escape(P_BOOT) + r"['\"]?([A-Za-z0-9_\-]{8,})"),
    ),
    (
        "OPENAI_API_KEY=",
        re.compile(re.escape(P_OPENAI) + r"['\"]?([A-Za-z0-9_\-]{8,})"),
    ),
    (
        "xai-",
        re.compile(r"(?<![A-Za-z0-9_])" + re.escape(P_XAI) + r"[A-Za-z0-9_\-]{8,}"),
    ),
    (
        "ghp_",
        re.compile(r"(?<![A-Za-z0-9_])" + re.escape(P_GHP) + r"[A-Za-z0-9]{8,}"),
    ),
    (
        "github_pat_",
        re.compile(r"(?<![A-Za-z0-9_])" + re.escape(P_GPAT) + r"[A-Za-z0-9_]{8,}"),
    ),
    (
        "app_secret",
        re.compile(
            r"\b" + re.escape(P_APPSEC) + r"\b\s*[=:]\s*['\"]?([A-Za-z0-9_\-]{8,})"
        ),
    ),
    (
        "BEGIN PRIVATE KEY",
        re.compile(r"-----" + re.escape(P_PEM) + r"-----"),
    ),
]


def is_redacted(match: str, payload: str) -> bool:
    body = payload.strip().strip("\"'")
    if not body:
        return True
    if PLACEHOLDER.match(body):
        return True
    if DUMMY_PART.search(body):
        return True
    visible = match[:4]
    rest = match[4:]
    if rest and all((ch in MASK) or ch.isspace() for ch in rest):
        return True
    if len(body) <= 4 and all((ch in MASK) or ch.isalnum() for ch in body):
        # four-or-fewer chars, treat as truncated/redacted unless it looks random-long
        if any(ch in MASK for ch in body) or visible.endswith(("*", "x", "X")):
            return True
    return False


def payload_for(name: str, m: re.Match[str]) -> str:
    if name in {
        "Bearer ",
        "ANTHROPIC_AUTH_TOKEN=",
        "api_key",
        "REMUDA_BOOTSTRAP_TOKEN=",
        "OPENAI_API_KEY=",
        "app_secret",
    } and m.lastindex:
        return m.group(1)
    text = m.group(0)
    if name == "agk_":
        return text[len(P_AGK) :]
    if name == "sk-":
        return text[len(P_SK) :]
    if name == "xai-":
        return text[len(P_XAI) :]
    if name == "ghp_":
        return text[len(P_GHP) :]
    if name == "github_pat_":
        return text[len(P_GPAT) :]
    return text


def looks_like_secret(name: str, m: re.Match[str]) -> bool:
    raw = m.group(0)
    payload = payload_for(name, m)
    if is_redacted(raw, payload):
        return False
    if name == "api_key":
        body = payload.strip().strip("\"'")
        if len(body) < 8:
            return False
    if name == "sk-":
        body = payload_for(name, m)
        if len(body) < 8:
            return False
    if name == "agk_":
        if len(payload) < 8:
            return False
    if name == "Bearer ":
        if len(payload.strip().strip("\"'")) < 8:
            return False
    if name == "ANTHROPIC_AUTH_TOKEN=":
        if len(payload.strip().strip("\"'")) < 8:
            return False
    return True


def preview(match: str) -> str:
    return match[:4] + "***"


def iter_git_paths(root: Path) -> list[Path]:
    try:
        out = subprocess.check_output(
            ["git", "ls-files", "-z", "--cached", "--others", "--exclude-standard"],
            cwd=root,
            stderr=subprocess.DEVNULL,
        )
    except (subprocess.CalledProcessError, FileNotFoundError):
        return []
    paths = []
    for raw in out.split(b"\0"):
        if not raw:
            continue
        paths.append(root / raw.decode("utf-8", "surrogateescape"))
    return paths


def staged_diff(root: Path) -> str:
    try:
        return subprocess.check_output(
            ["git", "diff", "--cached", "-U0", "--no-color"],
            cwd=root,
            stderr=subprocess.DEVNULL,
            text=True,
            errors="replace",
        )
    except (subprocess.CalledProcessError, FileNotFoundError):
        return ""


def should_skip_file(path: Path) -> bool:
    parts = set(path.parts)
    if parts & SKIP_DIR_NAMES:
        return True
    if path.suffix.lower() in SKIP_SUFFIXES:
        return True
    if path.name in SKIP_FILE_NAMES:
        return True
    return False


def scan_text(label: str, text: str, findings: list[str]) -> None:
    for lineno, line in enumerate(text.splitlines(), 1):
        for name, rx in RULES:
            for m in rx.finditer(line):
                if looks_like_secret(name, m):
                    findings.append(
                        f"{label}:{lineno}: {name} {preview(m.group(0))}"
                    )


def scan_file(path: Path, findings: list[str]) -> None:
    if should_skip_file(path) or not path.is_file():
        return
    try:
        if path.stat().st_size > MAX_BYTES:
            return
        data = path.read_bytes()
    except OSError:
        return
    if b"\0" in data[:4096]:
        return
    try:
        text = data.decode("utf-8")
    except UnicodeDecodeError:
        text = data.decode("utf-8", "replace")
    scan_text(str(path), text, findings)


def walk_path(root: Path, findings: list[str]) -> None:
    if root.is_file():
        scan_file(root, findings)
        return
    for dirpath, dirnames, filenames in os.walk(root):
        dirnames[:] = [d for d in dirnames if d not in SKIP_DIR_NAMES]
        for name in filenames:
            scan_file(Path(dirpath) / name, findings)


def main(argv: list[str]) -> int:
    root = Path.cwd()
    findings: list[str] = []
    targets = [Path(a) for a in argv]
    if targets:
        for target in targets:
            walk_path(target if target.is_absolute() else root / target, findings)
    else:
        paths = iter_git_paths(root)
        if paths:
            for path in paths:
                scan_file(path, findings)
        else:
            walk_path(root, findings)
        diff = staged_diff(root)
        if diff:
            scan_text(":staged", diff, findings)
    if findings:
        print("secret-scan: undeclared secrets", file=sys.stderr)
        for item in findings:
            print(item, file=sys.stderr)
        return 1
    print("secret-scan: pass", file=sys.stderr)
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
