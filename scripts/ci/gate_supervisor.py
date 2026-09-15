"""Step supervision for the Remuda merge gate (``scripts/ci/gate.sh``).

A gate step is spawned in its own process group and watched by a polling
loop that enforces a wall-clock deadline and notices when the direct child
has vanished without ``waitpid(2)`` returning it ("child-lost": an external
kill combined with reaping/reparenting anomalies on a shared host left the
production gate stuck in ``wait4`` for 79 minutes on 2026-09-15).

On timeout or child loss the whole step process group is signalled
(SIGTERM grace, then SIGKILL) so cargo/pnpm grandchild builds cannot leak.
"""

from __future__ import annotations

import ctypes
import errno
import json
import os
import signal
import subprocess
import sys
import time
from dataclasses import dataclass
from pathlib import Path

# Default per-step wall-clock budgets (seconds). The Hub e2e name must win
# over the generic web- prefix; scans and everything else get the 30 minute
# outer bound as well.
DEFAULT_TIMEOUTS = (
    ("web-hub-e2e", 30 * 60),
    ("cargo-", 30 * 60),
    ("web-", 20 * 60),
)
FALLBACK_TIMEOUT = 30 * 60

POLL_INTERVAL = 0.2
KILL_GRACE = 3.0


@dataclass
class StepResult:
    returncode: int | None
    # None on a normal exit, otherwise "timeout" or "child-lost".
    reason: str | None = None
    detail: str = ""


def default_timeout(name: str) -> int:
    for prefix, seconds in DEFAULT_TIMEOUTS:
        if name == prefix or name.startswith(prefix):
            return seconds
    return FALLBACK_TIMEOUT


def timeout_overrides() -> dict[str, float]:
    """``REMUDA_GATE_STEP_TIMEOUTS`` is a JSON object of step name to seconds.

    A value of 0 disables the timeout for that step. Garbage values are
    ignored so a typo never silently widens the budget.
    """
    raw = os.environ.get("REMUDA_GATE_STEP_TIMEOUTS")
    if not raw:
        return {}
    try:
        values = json.loads(raw)
    except (ValueError, TypeError):
        return {}
    if not isinstance(values, dict):
        return {}
    overrides = {}
    for key, value in values.items():
        try:
            seconds = float(value)
        except (TypeError, ValueError):
            continue
        if isinstance(key, str) and seconds >= 0:
            overrides[key] = seconds
    return overrides


def step_timeout(name: str, overrides: dict[str, float] | None = None) -> float | None:
    if overrides is None:
        overrides = timeout_overrides()
    if name in overrides:
        seconds = overrides[name]
        return None if seconds == 0 else seconds
    return float(default_timeout(name))


def pid_visible(pid: int) -> bool:
    """True while a pid has a /proc entry (alive or zombie).

    A zombie still has a /proc entry, so this only goes false once the child
    is truly gone (reaped) — which is exactly when a blocking wait elsewhere
    would never return. Falls back to signal 0 off Linux.
    """
    if Path("/proc").is_dir():
        return (Path("/proc") / str(pid)).exists()
    try:
        os.kill(pid, 0)
    except ProcessLookupError:
        return False
    except PermissionError:
        return True
    except OSError:
        return True
    return True


def kill_process_group(pgid: int) -> None:
    """SIGTERM the group, wait a grace period, then SIGKILL stragglers."""
    for sig in (signal.SIGTERM, signal.SIGKILL):
        try:
            os.killpg(pgid, sig)
        except ProcessLookupError:
            return
        except OSError as exc:
            if exc.errno == errno.ESRCH:
                return
            raise
        deadline = time.monotonic() + KILL_GRACE
        while time.monotonic() < deadline:
            try:
                os.killpg(pgid, 0)
            except ProcessLookupError:
                return
            except OSError as exc:
                if exc.errno == errno.ESRCH:
                    return
                raise
            time.sleep(0.1)


def supervise(
    proc: subprocess.Popen,
    pgid: int,
    timeout: float | None,
    started: float,
    *,
    poll_interval: float = POLL_INTERVAL,
) -> StepResult:
    """Wait for ``proc`` with a wall-clock deadline and child-loss guard.

    The caller spawns ``proc`` with ``start_new_session=True`` and passes the
    resulting process-group id. Never blocks indefinitely: returns
    ``child-lost`` if ``waitpid`` raises ECHILD or the pid disappears from
    ``/proc`` while still "running", and ``timeout`` once the deadline
    passes. In both cases the group is killed before returning.
    """
    deadline = started + timeout if timeout is not None else None
    while True:
        try:
            returncode = proc.poll()
        except ChildProcessError as exc:
            # ECHILD: something else reaped the direct child. Nothing will
            # ever wake a blocking wait; tear the group down.
            result = StepResult(
                proc.returncode, "child-lost", f"waitpid returned ECHILD: {exc}"
            )
            break
        if returncode is not None:
            return StepResult(returncode)
        if not pid_visible(proc.pid):
            result = StepResult(
                proc.returncode,
                "child-lost",
                f"pid {proc.pid} vanished while the step was running",
            )
            break
        if deadline is not None and time.monotonic() >= deadline:
            result = StepResult(
                proc.returncode, "timeout", f"step timed out after {int(timeout)}s"
            )
            break
        time.sleep(poll_interval)
    # The monitored leader is gone/overdue; reap every process in its group.
    kill_process_group(pgid)
    try:
        proc.wait(timeout=KILL_GRACE + 1.0)
    except (subprocess.TimeoutExpired, ChildProcessError, OSError):
        pass
    result.returncode = proc.returncode
    return result


def run_step(
    command: list[str],
    cwd: Path,
    timeout: float | None,
    *,
    stdin=None,
    stdout=None,
    stderr=None,
    on_group=None,
) -> StepResult:
    """Spawn one gate step in its own group and supervise it.

    ``on_group(pgid)`` runs immediately after the group is known, so a
    parent-death signal handler can tear the active group down.
    """
    proc = subprocess.Popen(
        command,
        cwd=cwd,
        stdin=stdin if stdin is not None else subprocess.DEVNULL,
        stdout=stdout if stdout is not None else None,
        stderr=stderr if stderr is not None else None,
        start_new_session=True,
    )
    try:
        pgid = os.getpgid(proc.pid)
    except ProcessLookupError:
        # Exited between spawn and getpgid; fall back to the pid itself.
        pgid = proc.pid
    if on_group is not None:
        on_group(pgid)
    return supervise(proc, pgid, timeout, time.monotonic())


def install_parent_death_guard(get_active_pgid) -> None:
    """Kill the active step group if this process's parent dies (Linux only).

    A SIGKILLed lane otherwise leaves its gate python and cargo grandchildren
    running under reparenting; ``PR_SET_PDEATHSIG`` turns parent death into a
    SIGTERM this process turns into a group teardown.
    """
    if not sys.platform.startswith("linux"):
        return

    def handle(signum, _frame):  # pragma: no cover - signal path
        active = get_active_pgid()
        if active is not None:
            try:
                kill_process_group(active)
            except OSError:
                pass
        os._exit(128 + signum)

    try:
        libc = ctypes.CDLL(None, use_errno=True)
        # PR_SET_PDEATHSIG = 1; deliver SIGTERM (15).
        if libc.prctl(1, signal.SIGTERM, 0, 0, 0) != 0:
            return
        # The parent may already have died before prctl ran.
        ppid = os.getppid()
        if ppid == 1 or not pid_visible(ppid):
            handle(signal.SIGTERM, None)
    except Exception:
        return
    for sig in (signal.SIGTERM, signal.SIGINT):
        try:
            signal.signal(sig, handle)
        except (ValueError, OSError):
            pass
