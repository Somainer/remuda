#!/usr/bin/env python3
"""Scan git worktree / index / given paths for undeclared secrets.

Patterns (task impl-acceptance-m0): agk_, sk-, Bearer[space],
ANTHROPIC_AUTH_TOKEN=, api_key. First-4-character redactions and obvious
placeholders/dummies are allowed.
"""

from __future__ import annotations

import hashlib
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
SKIP_SECRET_FILE_NAMES = {
    "secret-scan.py",
    "secret-scan.sh",
    "pnpm-lock.yaml",
    "package-lock.json",
    "Cargo.lock",
    "yarn.lock",
    "composer.lock",
    "private-tokens.sha256",
}
# Lockfiles still get the private-token hostname scan (bnpm-style registries).
SKIP_PRIVATE_FILE_NAMES = {
    "secret-scan.py",
    "secret-scan.sh",
    "private-tokens.local",
    "private-tokens.sha256",
}
SOURCE_SUFFIX_LABELS = {
    "rs",
    "ts",
    "tsx",
    "js",
    "jsx",
    "py",
    "md",
    "toml",
    "json",
    "yml",
    "yaml",
    "css",
    "lock",
    "c",
    "h",
    "go",
    "rb",
}
HOST_RE = re.compile(
    r"(?i)\b(?:[a-z0-9](?:[a-z0-9-]{0,61}[a-z0-9])?\.)+[a-z]{2,24}\b"
)
EMAIL_RE = re.compile(r"(?i)\b[a-z0-9._%+-]+@[a-z0-9.-]+\.[a-z]{2,24}\b")
HOME_RE = re.compile(r"(?i)(?:/home|/Users)/([A-Za-z0-9._-]+)")
IDENT_RE = re.compile(r"(?i)\b[a-z][a-z0-9_-]{4,39}\b")
HASHES_NAME = "private-tokens.sha256"
LOCAL_NAME = "private-tokens.local"
MAX_BYTES = 2_000_000
MASK = set("*xX•.…][)(") | {"…"}
DUMMY_PART = re.compile(
    r"(?i)(^|[-_/])(secret|example|placeholder|dummy|redacted|fake|sample|yourkey|xxx+|should-not|bootstrap|leak)([-_]|$)"
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
    body = payload.strip().strip("\"'")
    # Short literals are placeholders (docs/fixtures), not live tokens.
    if name in {
        "sk-",
        "agk_",
        "xai-",
        "ghp_",
        "github_pat_",
        "Bearer ",
        "ANTHROPIC_AUTH_TOKEN=",
        "REMUDA_BOOTSTRAP_TOKEN=",
        "OPENAI_API_KEY=",
        "api_key",
        "app_secret",
    } and len(body) < 16:
        return False
    return True


def preview(match: str) -> str:
    return match[:4] + "***"


def mask_private(token: str) -> str:
    if not token:
        return "***"
    return token[0] + "***"


def sha256_text(value: str) -> str:
    return hashlib.sha256(value.encode("utf-8")).hexdigest()


def hashes_path(root: Path) -> Path:
    return root / "scripts" / "ci" / HASHES_NAME


def local_path(root: Path) -> Path:
    return root / "scripts" / "ci" / LOCAL_NAME


def write_hashes_from_local(root: Path) -> int:
    src = local_path(root)
    if not src.is_file():
        return 0
    seen: set[str] = set()
    for line in src.read_text(encoding="utf-8").splitlines():
        item = line.strip().lower()
        if not item or item.startswith("#"):
            continue
        seen.add(sha256_text(item))
    dest = hashes_path(root)
    dest.parent.mkdir(parents=True, exist_ok=True)
    body = (
        "# sha256 of lowercase denylist strings (plaintext is gitignored)\n"
        + "\n".join(sorted(seen))
        + "\n"
    )
    dest.write_text(body, encoding="utf-8")
    return len(seen)


def load_private_hashes(root: Path) -> set[str]:
    write_hashes_from_local(root)
    path = hashes_path(root)
    if not path.is_file():
        print("secret-scan: missing scripts/ci/private-tokens.sha256", file=sys.stderr)
        sys.exit(2)
    out: set[str] = set()
    for line in path.read_text(encoding="utf-8").splitlines():
        item = line.strip().lower()
        if not item or item.startswith("#"):
            continue
        if re.fullmatch(r"[0-9a-f]{64}", item):
            out.add(item)
    if not out:
        print("secret-scan: empty private-token denylist", file=sys.stderr)
        sys.exit(2)
    return out


def host_expansions(host: str) -> set[str]:
    host = host.lower().strip(".")
    labels = [p for p in host.split(".") if p]
    out = {host}
    if labels and labels[-1] in SOURCE_SUFFIX_LABELS:
        return set()
    for label in labels:
        if len(label) >= 3 and label not in SOURCE_SUFFIX_LABELS:
            out.add(label)
    for i in range(len(labels) - 1):
        suffix = ".".join(labels[i:])
        if suffix:
            out.add(suffix)
    return out


def expansions_for(raw: str) -> set[str]:
    value = raw.strip().strip("'\"")
    if not value:
        return set()
    lower = value.lower()
    out = {lower}
    if "@" in lower:
        local, _, domain = lower.partition("@")
        if local:
            out.add(local)
        if domain:
            out |= host_expansions(domain)
    else:
        out |= host_expansions(lower)
    return {item for item in out if len(item) >= 3}


def extract_private_candidates(line: str) -> list[str]:
    found: list[str] = []
    for rx in (EMAIL_RE, HOST_RE):
        found.extend(m.group(0) for m in rx.finditer(line))
    found.extend(m.group(1) for m in HOME_RE.finditer(line))
    found.extend(m.group(0) for m in IDENT_RE.finditer(line))
    # Preserve order, drop duplicates.
    seen: set[str] = set()
    ordered: list[str] = []
    for item in found:
        key = item.lower()
        if key in seen:
            continue
        seen.add(key)
        ordered.append(item)
    return ordered


def scan_private_text(
    label: str, text: str, findings: list[str], denylist: set[str]
) -> None:
    for lineno, line in enumerate(text.splitlines(), 1):
        for cand in extract_private_candidates(line):
            for piece in expansions_for(cand):
                if sha256_text(piece) in denylist:
                    findings.append(
                        f"{label}:{lineno}: private-token {mask_private(cand)}"
                    )
                    break


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


def should_skip_common(path: Path) -> bool:
    parts = set(path.parts)
    if parts & SKIP_DIR_NAMES:
        return True
    if path.suffix.lower() in SKIP_SUFFIXES:
        return True
    return False


def should_skip_secrets(path: Path) -> bool:
    return should_skip_common(path) or path.name in SKIP_SECRET_FILE_NAMES


def should_skip_private(path: Path) -> bool:
    return should_skip_common(path) or path.name in SKIP_PRIVATE_FILE_NAMES


def scan_text(label: str, text: str, findings: list[str]) -> None:
    for lineno, line in enumerate(text.splitlines(), 1):
        for name, rx in RULES:
            for m in rx.finditer(line):
                if looks_like_secret(name, m):
                    findings.append(
                        f"{label}:{lineno}: {name} {preview(m.group(0))}"
                    )


def scan_file(path: Path, findings: list[str], denylist: set[str]) -> None:
    if not path.is_file():
        return
    skip_secrets = should_skip_secrets(path)
    skip_private = should_skip_private(path)
    if skip_secrets and skip_private:
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
    if not skip_secrets:
        scan_text(str(path), text, findings)
    if not skip_private:
        scan_private_text(str(path), text, findings, denylist)


def walk_path(root: Path, findings: list[str], denylist: set[str]) -> None:
    if root.is_file():
        scan_file(root, findings, denylist)
        return
    for dirpath, dirnames, filenames in os.walk(root):
        dirnames[:] = [d for d in dirnames if d not in SKIP_DIR_NAMES]
        for name in filenames:
            scan_file(Path(dirpath) / name, findings, denylist)


def self_test() -> int:
    dummy = "forbidden.invalid"
    digest = sha256_text(dummy)
    denylist = {digest}
    line = f"mirror https://{dummy}/pkg.tgz user /home/ci-runner"
    hits: list[str] = []
    scan_private_text("t", line, hits, denylist)
    if not hits:
        print("secret-scan self-test: missed dummy host", file=sys.stderr)
        return 1
    joined = "\n".join(hits)
    if dummy in joined:
        print("secret-scan self-test: dummy leaked in output", file=sys.stderr)
        return 1
    if "private-token" not in joined:
        print("secret-scan self-test: missing kind", file=sys.stderr)
        return 1
    if sha256_text("example.com") in denylist:
        print("secret-scan self-test: unexpected denylist", file=sys.stderr)
        return 1
    print("secret-scan self-test: pass", file=sys.stderr)
    return 0


def main(argv: list[str]) -> int:
    if argv[:1] == ["--self-test"]:
        return self_test()
    root = Path.cwd()
    if argv[:1] == ["--write-hashes"]:
        n = write_hashes_from_local(root)
        if n == 0:
            print("secret-scan: no private-tokens.local", file=sys.stderr)
            return 1
        print(f"secret-scan: wrote {n} hashes", file=sys.stderr)
        return 0
    denylist = load_private_hashes(root)
    findings: list[str] = []
    targets = [Path(a) for a in argv]
    if targets:
        for target in targets:
            walk_path(target if target.is_absolute() else root / target, findings, denylist)
    else:
        paths = iter_git_paths(root)
        if paths:
            for path in paths:
                scan_file(path, findings, denylist)
        else:
            walk_path(root, findings, denylist)
        diff = staged_diff(root)
        if diff:
            scan_text(":staged", diff, findings)
            scan_private_text(":staged", diff, findings, denylist)
    if findings:
        print("secret-scan: undeclared secrets", file=sys.stderr)
        for item in findings:
            print(item, file=sys.stderr)
        return 1
    print("secret-scan: pass", file=sys.stderr)
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
