"""Conservative workspace test selection from Cargo's resolved dependency graph."""

from collections import defaultdict
import json
from pathlib import Path
import subprocess


def select_crates(metadata, changed_paths):
    """Return sorted workspace names and why; include every reverse dependency.

    Cargo metadata is collected with all features and without a target filter,
    so optional, development, build and platform dependencies all participate.
    Unknown shared inputs select the entire workspace instead of guessing.
    """
    root = Path(metadata["workspace_root"])
    members = set(metadata["workspace_members"])
    packages = {package["id"]: package for package in metadata["packages"]}
    all_names = sorted(packages[member]["name"] for member in members)
    directories = []
    for package in packages.values():
        directory = Path(package["manifest_path"]).parent
        if directory.is_relative_to(root):
            directories.append((directory.relative_to(root), package["id"]))
    # A nested crate owns its own files, even when a workspace member is at root.
    directories.sort(key=lambda item: len(item[0].parts), reverse=True)
    changed = set()
    for raw_path in changed_paths:
        path = Path(raw_path)
        if path.is_absolute() or ".." in path.parts:
            raise ValueError(f"expected a repository-relative changed path: {raw_path}")
        if str(path) in {"Cargo.toml", "Cargo.lock", "rust-toolchain", "rust-toolchain.toml"}:
            return all_names, f"shared input: {path}"
        owner = next((owner for directory, owner in directories
                      if path.is_relative_to(directory)), None)
        if owner is not None:
            changed.add(owner)
        elif path.parts and path.parts[0] in {"docs", "web", "skills"}:
            continue
        elif len(path.parts) == 1 and path.suffix == ".md":
            continue
        else:
            # Includes removed crates, scripts, .cargo config and shared fixtures.
            return all_names, f"shared or unowned input: {path}"
    reverse = defaultdict(set)
    resolve = metadata.get("resolve")
    if resolve is None:
        raise ValueError("cargo metadata must include its resolved dependency graph")
    for node in resolve["nodes"]:
        for dependency in node["dependencies"]:
            reverse[dependency].add(node["id"])
    pending = list(changed)
    while pending:
        for dependent in reverse[pending.pop()] - changed:
            changed.add(dependent)
            pending.append(dependent)
    names = sorted(packages[member]["name"] for member in members & changed)
    return names, "changed crates and reverse dependencies" if names else "no Rust changes"


def test_selection(root, base=None, head="HEAD"):
    """Inspect the candidate checkout, never the coordinator's source checkout."""
    metadata = json.loads(subprocess.check_output(
        ["cargo", "metadata", "--format-version", "1", "--locked", "--all-features"],
        cwd=root,
    ))
    if base is None:
        members = set(metadata["workspace_members"])
        return sorted(package["name"] for package in metadata["packages"]
                      if package["id"] in members), "full workspace"
    paths = subprocess.check_output(
        ["git", "diff", "--name-only", "--no-renames", "-z", base, head, "--"],
        cwd=root,
    ).decode("utf-8").split("\0")
    return select_crates(metadata, [path for path in paths if path])
