"""Synthetic Cargo metadata: no binaries or model credentials required."""

import importlib.util
from pathlib import Path
import unittest

spec = importlib.util.spec_from_file_location(
    "affected", Path(__file__).resolve().parents[1] / "ci" / "affected.py"
)
affected = importlib.util.module_from_spec(spec)
spec.loader.exec_module(affected)


def metadata(edges, extra=()):
    root = Path("/repo")
    return {
        "workspace_root": str(root),
        "workspace_members": [f"pkg:{name}" for name in edges if name not in extra],
        "packages": [{"id": f"pkg:{name}", "name": name,
                      "manifest_path": str(root / "crates" / name / "Cargo.toml")}
                     for name in edges],
        "resolve": {"nodes": [{"id": f"pkg:{name}",
                               "dependencies": [f"pkg:{dep}" for dep in deps]}
                              for name, deps in edges.items()]},
    }


class AffectedTests(unittest.TestCase):
    def setUp(self):
        self.graph = metadata({"core": [], "left": ["core"], "right": ["core"],
                               "cli": ["left", "right"], "isolated": []})

    def test_transitive_diamond_and_dependency_direction(self):
        self.assertEqual(affected.select_crates(self.graph, ["crates/core/src/lib.rs"])[0],
                         ["cli", "core", "left", "right"])
        self.assertEqual(affected.select_crates(self.graph, ["crates/cli/src/main.rs"])[0],
                         ["cli"])

    def test_cycles_and_non_workspace_path_dependencies(self):
        graph = metadata({"a": ["b"], "b": ["a", "helper"], "helper": []}, extra=["helper"])
        self.assertEqual(affected.select_crates(graph, ["crates/helper/build.rs"])[0],
                         ["a", "b"])

    def test_shared_inputs_and_deleted_crates_are_conservative(self):
        for path in ["Cargo.lock", "Cargo.toml", ".cargo/config.toml", "rust-toolchain.toml",
                     "scripts/ci/gate.sh", "crates/deleted/src/lib.rs"]:
            with self.subTest(path=path):
                self.assertEqual(len(affected.select_crates(self.graph, [path])[0]), 5)

    def test_docs_web_empty_and_crate_local_docs(self):
        self.assertEqual(affected.select_crates(self.graph, ["docs/design/a.md", "web/a.ts",
                                                            "skills/remuda/SKILL.md"])[0], [])
        self.assertEqual(affected.select_crates(self.graph, [])[0], [])
        self.assertEqual(affected.select_crates(self.graph, ["crates/cli/README.md"])[0], ["cli"])

    def test_nested_packages_use_deepest_owner_and_paths_have_boundaries(self):
        self.assertEqual(len(affected.select_crates(self.graph, ["crates/cli-other/a.rs"])[0]), 5)
        self.graph["packages"][0]["manifest_path"] = "/repo/Cargo.toml"
        self.assertEqual(affected.select_crates(self.graph, ["crates/cli/tests/a.rs"])[0], ["cli"])

    def test_invalid_metadata_or_paths_fail(self):
        self.graph["resolve"] = None
        with self.assertRaises(ValueError):
            affected.select_crates(self.graph, ["crates/core/src/lib.rs"])
        with self.assertRaises(ValueError):
            affected.select_crates(self.graph, ["../src/lib.rs"])


if __name__ == "__main__":
    unittest.main()
