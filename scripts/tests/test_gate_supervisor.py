"""Tests for the gate step supervisor: timeouts, child-loss, group teardown."""

import importlib.util
import json
import os
from pathlib import Path
import subprocess
import sys
import tempfile
import time
import unittest

spec = importlib.util.spec_from_file_location(
    "gate_supervisor",
    Path(__file__).resolve().parents[1] / "ci" / "gate_supervisor.py",
)
supervisor = importlib.util.module_from_spec(spec)
# dataclasses resolve string annotations through sys.modules, so the module
# must be registered before exec under Python 3.10.
sys.modules["gate_supervisor"] = supervisor
spec.loader.exec_module(supervisor)

GATE_SH = Path(__file__).resolve().parents[1] / "ci" / "gate.sh"
REPO_ROOT = GATE_SH.parents[1]


def find_unused_pid():
    for pid in range(100000, 200000):
        if not supervisor.pid_visible(pid):
            return pid
    raise RuntimeError("no unused pid available")


class VanishedProc:
    """A Popen stand-in whose child has vanished but poll never reaps."""

    def __init__(self, pid):
        self.pid = pid
        self.returncode = None

    def poll(self):
        return None

    def wait(self, timeout=None):
        self.returncode = -1
        return -1


class TimeoutConfigTests(unittest.TestCase):
    def test_defaults_match_the_documented_budgets(self):
        self.assertEqual(supervisor.default_timeout("cargo-fmt"), 30 * 60)
        self.assertEqual(supervisor.default_timeout("cargo-test"), 30 * 60)
        self.assertEqual(supervisor.default_timeout("web-install"), 20 * 60)
        # The Hub e2e step keeps the 30 minute budget despite the web- prefix.
        self.assertEqual(supervisor.default_timeout("web-hub-e2e"), 30 * 60)
        self.assertEqual(supervisor.default_timeout("secret-scan"), 30 * 60)

    def test_env_overrides_and_zero_means_disabled(self):
        os.environ["REMUDA_GATE_STEP_TIMEOUTS"] = json.dumps(
            {"cargo-test": 1, "web-build": 0, "nope": "x"}
        )
        try:
            overrides = supervisor.timeout_overrides()
            self.assertEqual(overrides["cargo-test"], 1.0)
            self.assertEqual(overrides["web-build"], 0.0)
            self.assertNotIn("nope", overrides)
            self.assertEqual(supervisor.step_timeout("cargo-test", overrides), 1.0)
            self.assertIsNone(supervisor.step_timeout("web-build", overrides))
            self.assertEqual(supervisor.step_timeout("cargo-fmt", overrides), 30 * 60.0)
        finally:
            del os.environ["REMUDA_GATE_STEP_TIMEOUTS"]

    def test_garbage_override_env_is_ignored(self):
        for raw in ["not json", "[]", "12"]:
            os.environ["REMUDA_GATE_STEP_TIMEOUTS"] = raw
            try:
                self.assertEqual(supervisor.timeout_overrides(), {})
            finally:
                del os.environ["REMUDA_GATE_STEP_TIMEOUTS"]


class SuperviseTests(unittest.TestCase):
    def test_normal_step_passes_through_its_exit_code(self):
        result = supervisor.run_step(
            [sys.executable, "-c", "import sys; sys.exit(7)"],
            Path.cwd(),
            None,
        )
        self.assertEqual(result.returncode, 7)
        self.assertIsNone(result.reason)

    def test_timeout_kills_the_whole_step_process_group(self):
        # The direct child ignores SIGTERM, so cleanup needs the group
        # SIGKILL escalation to actually end the step.
        script = (
            "import signal,time;"
            "signal.signal(signal.SIGTERM, signal.SIG_IGN);"
            "time.sleep(300)"
        )
        proc = subprocess.Popen(
            [sys.executable, "-c", script],
            stdin=subprocess.DEVNULL,
            stdout=subprocess.DEVNULL,
            stderr=subprocess.DEVNULL,
            start_new_session=True,
        )
        pgid = os.getpgid(proc.pid)
        try:
            started = time.monotonic()
            result = supervisor.supervise(proc, pgid, 0.5, started)
            self.assertEqual(result.reason, "timeout")
            self.assertLess(time.monotonic() - started, 15)
            self._assert_group_gone(pgid)
        finally:
            try:
                os.killpg(pgid, 9)
            except OSError:
                pass

    def test_timeout_reaps_grandchildren_inside_the_group(self):
        # Grandchild stays in the group (no new session): the negative-pid
        # kill must reach it.
        script = (
            "import os,time;"
            "p=os.fork();"
            "(time.sleep(300) if p==0 else time.sleep(300))"
        )
        proc = subprocess.Popen(
            [sys.executable, "-c", script],
            stdin=subprocess.DEVNULL,
            stdout=subprocess.DEVNULL,
            stderr=subprocess.DEVNULL,
            start_new_session=True,
        )
        pgid = os.getpgid(proc.pid)
        try:
            result = supervisor.supervise(proc, pgid, 0.5, time.monotonic())
            self.assertEqual(result.reason, "timeout")
            self._assert_group_gone(pgid)
        finally:
            try:
                os.killpg(pgid, 9)
            except OSError:
                pass

    def _assert_group_gone(self, pgid):
        deadline = time.monotonic() + 5
        while time.monotonic() < deadline:
            remaining = set()
            for entry in Path("/proc").iterdir():
                if not entry.name.isdigit():
                    continue
                try:
                    stat = (entry / "stat").read_text()
                    fields = stat[stat.rfind(")") + 2:].split()
                    if int(fields[3]) == pgid:
                        remaining.add(entry.name)
                except (OSError, ValueError, IndexError):
                    continue
            if not remaining:
                return
            time.sleep(0.05)
        self.fail(f"processes survived in group {pgid}: {remaining}")

    def test_child_lost_when_pid_vanishes_without_a_reap(self):
        proc = VanishedProc(find_unused_pid())
        result = supervisor.supervise(proc, proc.pid, None, time.monotonic())
        self.assertEqual(result.reason, "child-lost")
        self.assertIn("vanished", result.detail)

    def test_kill_process_group_on_a_vanished_group_is_not_an_error(self):
        supervisor.kill_process_group(find_unused_pid())


@unittest.skipUnless(Path("/proc").is_dir(), "gate.sh needs /proc")
class GateDriverEndToEndTests(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        self.root = Path(self.tmp.name)
        self.stub = self.root / "stub.py"
        self.report = self.root / "gate.jsonl"
        self.stub.write_text(
            "#!" + sys.executable + "\n"
            "import os, sys, time\n"
            "if sys.argv[1] == 'web-install':\n"
            "    # A grandchild that escapes the process group. It must also\n"
            "    # detach its std fds: an orphan inheriting the gate's pipes\n"
            "    # would keep subprocess.run() waiting for EOF forever.\n"
            "    if os.fork() == 0:\n"
            "        os.setsid()\n"
            "        null = os.open(os.devnull, os.O_RDWR)\n"
            "        os.dup2(null, 0); os.dup2(null, 1); os.dup2(null, 2)\n"
            "        time.sleep(300)\n"
            "        os._exit(0)\n"
            "    time.sleep(300)\n"
            "sys.exit(0)\n"
        )
        self.stub.chmod(0o755)

    def tearDown(self):
        # The orphan is reparented to init; clean it up explicitly.
        subprocess.run(["pkill", "-9", "-f", "time.sleep(300)"], check=False)
        self.tmp.cleanup()

    def test_hanging_step_times_out_and_fails_the_gate(self):
        env = dict(os.environ)
        env["REMUDA_MERGE_GATE_COMMAND"] = str(self.stub)
        env["REMUDA_GATE_STEP_TIMEOUTS"] = json.dumps({"web-install": 1})
        env["CARGO_TARGET_DIR"] = str(self.root / "target")
        proc = subprocess.run(
            ["bash", str(GATE_SH), "--web-only", "--report", str(self.report)],
            cwd=REPO_ROOT,
            env=env,
            stdin=subprocess.DEVNULL,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            timeout=60,
        )
        self.assertNotEqual(proc.returncode, 0, proc.stderr.decode())
        steps = [
            json.loads(line) for line in self.report.read_text().splitlines()
        ]
        install = next(step for step in steps if step["name"] == "web-install")
        self.assertEqual(install["status"], "failed")
        self.assertEqual(install["reason"], "timeout")
        self.assertTrue(
            all(
                step["status"] == "skipped"
                for step in steps
                if step["name"] in {"web-build", "web-test", "web-hub-e2e"}
            )
        )


if __name__ == "__main__":
    unittest.main()
