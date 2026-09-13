"""Synthetic tests: candidate edits and rollback must respect reviewed boundaries."""
import importlib.util
from contextlib import ExitStack
import json
from pathlib import Path
import tempfile
import unittest
from unittest.mock import patch

ROOT = Path(__file__).resolve().parents[2]
SPEC = importlib.util.spec_from_file_location("caddy_change", ROOT / "deploy/intranet/caddy-change.py")
MODULE = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(MODULE)


class IntranetCaddyTests(unittest.TestCase):
    def paths(self, root):
        stack = ExitStack()
        for name in ("MAIN", "BACKUP", "PREVIEW", "STATE", "INCLUDE"):
            stack.enter_context(patch.object(MODULE, name, root / name.lower()))
        return stack

    def test_candidate_only_changes_admin_and_adds_import(self):
        original = b"{\n\tadmin off\n}\ngateway.invalid {\n reverse_proxy gateway:8080\n}\n"
        self.assertEqual(MODULE.candidate(original),
                         original.replace(b"admin off", b"admin localhost:2019") + b"\n" + MODULE.IMPORT)
        for invalid in [original + MODULE.IMPORT, original.replace(b"admin off", b"admin localhost:2019"),
                        original + b"admin off\n"]:
            with self.assertRaises(ValueError):
                MODULE.candidate(invalid)

    def test_rollback_refuses_to_clobber_subsequent_operator_edits(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            original = b"{\n admin off\n}\n"
            (root / "backup").write_bytes(original)
            (root / "main").write_bytes(b"unrelated operator update")
            (root / "state").write_text(json.dumps({
                "original_sha256": MODULE.digest(original),
                "candidate_sha256": MODULE.digest(MODULE.candidate(original)),
            }))
            with patch.object(MODULE, "MAIN", root / "main"), \
                 patch.object(MODULE, "BACKUP", root / "backup"), \
                 patch.object(MODULE, "STATE", root / "state"), \
                 patch.object(MODULE, "run") as run:
                with self.assertRaisesRegex(RuntimeError, "subsequent edits"):
                    MODULE.rollback({})
                run.assert_not_called()
                self.assertEqual((root / "main").read_bytes(), b"unrelated operator update")

    def test_prepare_never_changes_active_file_or_restarts(self):
        with tempfile.TemporaryDirectory() as directory, self.paths(Path(directory)):
            original = b"{\n admin off\n}\n"
            MODULE.MAIN.write_bytes(original)
            MODULE.INCLUDE.write_bytes(b"hub.invalid {}\n")
            settings = {"baseline_caddy_sha256": MODULE.digest(original),
                        "baseline_started_at": "baseline",
                        "include_sha256": MODULE.digest(MODULE.INCLUDE.read_bytes())}
            with patch.object(MODULE, "container_state", return_value={"Running": True, "StartedAt": "baseline"}), \
                 patch.object(MODULE, "gateway"), patch.object(MODULE, "run") as run:
                MODULE.prepare(settings)
                self.assertEqual(MODULE.MAIN.read_bytes(), original)
                self.assertFalse(MODULE.BACKUP.exists())
                commands = [call.args[0] for call in run.call_args_list]
                self.assertTrue(any("validate" in command for command in commands))
                self.assertFalse(any("restart" in command or "kill" in command for command in commands))

    def test_failed_restart_rolls_back_even_when_container_is_stopped(self):
        with tempfile.TemporaryDirectory() as directory, self.paths(Path(directory)):
            original = b"{\n admin off\n}\n"
            MODULE.MAIN.write_bytes(original)
            with patch.object(MODULE, "prepare", return_value=(original, MODULE.candidate(original))), \
                 patch.object(MODULE, "gateway"), patch.object(MODULE, "wait_healthy"), \
                 patch.object(MODULE, "run", side_effect=[RuntimeError("container stopped"), b""]) as run:
                with self.assertRaisesRegex(RuntimeError, "container stopped"):
                    MODULE.apply({})
                self.assertEqual(MODULE.MAIN.read_bytes(), original)
                self.assertEqual([call.args[0] for call in run.call_args_list],
                                 [["docker", "restart", MODULE.CONTAINER]] * 2)

    def test_partial_active_write_restores_baseline_before_restart(self):
        with tempfile.TemporaryDirectory() as directory, self.paths(Path(directory)):
            original = b"{\n admin off\n}\n"
            updated = MODULE.candidate(original)
            MODULE.MAIN.write_bytes(original)
            write_bytes = Path.write_bytes

            def failing_write(path, data):
                if path == MODULE.MAIN and data == updated:
                    write_bytes(path, data[:8])
                    raise OSError("synthetic partial write")
                return write_bytes(path, data)

            with patch.object(MODULE, "prepare", return_value=(original, updated)), \
                 patch.object(MODULE, "gateway"), patch.object(MODULE, "wait_healthy"), \
                 patch.object(MODULE, "run", return_value=b"") as run, \
                 patch.object(Path, "write_bytes", failing_write):
                with self.assertRaisesRegex(OSError, "partial write"):
                    MODULE.apply({})
                self.assertEqual(MODULE.MAIN.read_bytes(), original)
                run.assert_called_once_with(["docker", "restart", MODULE.CONTAINER], timeout=90)
