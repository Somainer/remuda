#!/usr/bin/env python3
"""Fixed-seed generator for the c-jevspike offline corpus and Jev response fixture.

Everything here is SYNTHETIC. Every template family is hand-authored after a
real gate-failure shape seen in this repository (see the ``modeled_on`` field
in manifest.json); the gold label is fixed per family by construction, so no
model ever produces a label. Variant expansion (line numbers, timings, file
paths drawn from a bank) is the only thing the seeded RNG does.

Outputs, written next to this file:
  corpus.jsonl             one record per line
  manifest.json            seed, counts per category/family, sha256 digests
  responses.fixture.json   a SYNTHETIC stand-in for Jev API responses

The synthetic response predictor inside this file is a *harness self-check*:
it lets test_jev_spike_eval.py exercise the full gate math offline. It is not,
and must never be presented as, evidence about the real Jev model.

Stdlib only. Deterministic: same seed -> byte-identical outputs.
"""

from __future__ import annotations

import hashlib
import json
import math
import random
from pathlib import Path

SEED = 20260923
RESPONSE_SEED = SEED ^ 0x5EED17
# Negative-control profile seed (responses.fail.fixture.json).
RESPONSE_FAIL_SEED = SEED ^ 0xFA17ED
# Number of flake records the negative control answers confidently wrong.
FAIL_PROFILE_WRONG_FLAKES = 14
SCHEMA_VERSION = 1

CATEGORIES = ["cargo-test", "clippy", "fmt", "vitest", "playwright", "infra"]
GOLDS = ["regression", "flake"]
QUESTION_KEY = "triage"
OPTIONS = ["flake", "regression"]
MODEL_PIN = "jev-1.13.0"  # D6 in briefs/jev-plugin.md: pin, never jev-latest


# ---------------------------------------------------------------------------
# Shared banks
# ---------------------------------------------------------------------------

# Repo-relative paths only. No absolute paths, no machine names, no domains.
RUST_PATHS = [
    "crates/remuda-hub/src/gate.rs",
    "crates/remuda-hub/src/supply.rs",
    "crates/remuda-hub/src/store.rs",
    "crates/remuda-hub/src/interactions.rs",
    "crates/remuda-hub/src/advisory.rs",
    "crates/remuda-node/src/runtime/mod.rs",
    "crates/remuda-node/src/runtime/pty.rs",
    "crates/remuda-node/src/api_relay/egress.rs",
    "crates/remuda-node/src/api_relay/relay_tests.rs",
    "crates/remuda-node/src/journal/mod.rs",
    "crates/remuda-protocol/src/frames.rs",
    "crates/remuda/src/cmd/mcp/attachment.rs",
]
WEB_PATHS = [
    "src/features/session/EffortSlider.tsx",
    "src/features/session/transcript.tsx",
    "src/features/tasks/TaskComposer.tsx",
    "src/features/tasks/taskRows.ts",
    "src/lib/store.ts",
    "src/lib/debounce.ts",
    "src/lib/formatTime.ts",
    "src/lib/poll.ts",
]
SPEC_PATHS = [
    "tests/e2e/task-model-list.hub.spec.ts",
    "tests/e2e/effort-sync.hub.spec.ts",
    "tests/e2e/spaces.hub.spec.ts",
    "tests/e2e/approval-card.hub.spec.ts",
]
# Regression families may never render on the rostered flaky spec file: the
# whole point of that roster entry is that the file's tests are known-flaky.
REG_SPEC_PATHS = [p for p in SPEC_PATHS if "effort-sync" not in p]

# Test names that a deterministic rule baseline is allowed to "know" as
# historically flaky. The first six are real flaky tests from this repo's
# history (the task statement names two of them); the last two are invented
# names used only by the synthetic timing families.
FLAKY_ROSTER = [
    "cmd::mcp::attachment::tests::an_agent_naming_another_session_never_reaches_the_hub",
    "runtime::tests::create_is_accepted_before_a_slow_launch_finishes",
    "api_relay::relay_tests::credit_starved_egress_ends_at_the_hard_cap_after_four_chunks",
    "a_reopened_store_vouches_for_an_empty_inventory",
    "lets a free-typed id through as the verbatim fallback",
    "effort-sync.hub.spec.ts",
    "timing_tests::debounce_coalesces_rapid_signals",
    "timing_tests::watch_notify_arrives_before_deadline",
]


def line_no(rng: random.Random) -> int:
    return rng.choice(
        [rng.randint(40, 399), rng.randint(400, 899), rng.randint(900, 1390)]
    )


def col_no(rng: random.Random) -> int:
    return rng.randint(2, 64)


# ---------------------------------------------------------------------------
# fmt families
# ---------------------------------------------------------------------------

FMT_DIFF_LINES = [
    (
        "     if signal.is_tighten() {\n"
        "-        Some(Outcome::escalate(reason.borrow()))\n"
        "+        Some(Outcome::escalate(reason.clone()))\n"
        "     } else {"
    ),
    (
        "     let frame = serde_json::from_slice(bytes)?;\n"
        "-    Ok(frame)\n"
        "+    Ok(frame)\n"
        "}"
    ),
    (
        "-    pub fn verdict(&self) -> &'static str {\n"
        "-        match self {\n"
        "-            Verdict::Flake => \"flake\",\n"
        "+    pub fn verdict(&self) -> &'static str {\n"
        "+        match self {\n"
        "+            Verdict::Flake => \"flake\",\n"
        "             Verdict::Regression => \"regression\","
    ),
    (
        " pub(crate) fn tail_summary(raw: &str) -> Cow<'_, str> {\n"
        "-    raw.trim_end_matches('\\n').into()\n"
        "+    raw.trim_end().into()\n"
        " }"
    ),
    (
        "-use std::collections::HashMap;\n"
        " use serde::{Deserialize, Serialize};\n"
        "+use std::collections::HashMap;"
    ),
]


def f_fmt_diff(rng: random.Random) -> str:
    path = rng.choice(RUST_PATHS)
    ln = line_no(rng)
    nfiles = rng.choice([1, 1, 2, 3])
    return (
        f"Diff in {path} at line {ln}:\n"
        f"{rng.choice(FMT_DIFF_LINES)}\n"
        f"Diff in {rng.choice(RUST_PATHS)} at line {line_no(rng)}:\n"
        "     fn classify(tail: &str) -> Verdict {\n"
        "-        Verdict::Unknown\n"
        "+        Verdict::Abstain\n"
        "     }\n"
        f"\ncargo fmt --all --check: {nfiles} file(s) contain formatting differences"
    )


def f_fmt_toolchain_drift(rng: random.Random) -> str:
    # Environment flake: a runner whose pinned toolchain drifted formats the
    # same file differently; the pinned runner passes on the scheduled rerun.
    a = f"1.{rng.randint(80, 84)}.{rng.randint(0, 2)}"
    b = f"1.{rng.randint(80, 84)}.{rng.randint(0, 2)}"
    return (
        f"rustfmt edition2024 grammar differs between toolchains ({a} vs {b})\n"
        f"Diff in {rng.choice(RUST_PATHS)} at line {line_no(rng)}:\n"
        "     const RETRY_BUDGET_MS: u64 = 30 * 60 * 1000;\n"
        "-    const RETRY_BUDGET_MS: u64=30*60*1000;\n"
        "+    const RETRY_BUDGET_MS: u64 = 30 * 60 * 1000;\n"
        "cargo fmt --all --check: differences only on runner with unpinned rustfmt"
    )


# ---------------------------------------------------------------------------
# clippy families
# ---------------------------------------------------------------------------

CLIPPY_LINTS = [
    (
        "needless borrow of a reference",
        "clippy::needless-borrow",
        "    if rate_limited(&&family.text) {\n  |                   ^^^^^^^^^^^^^^ help: remove this borrow",
    ),
    (
        "redundant clone of a `Copy` type",
        "clippy::redundant-clone",
        "         let seq = event.seq.clone();\n  |                       ^^^^^^^^ help: remove this `.clone()`",
    ),
    (
        "this `if` branch can be collapsed",
        "clippy::collapsible-if",
        "     if signal.is_some() {\n  |     ^^^^^^^^^^^^^^^^^^^ help: merge nested `if` conditions",
    ),
    (
        "useless use of `format!`",
        "clippy::useless-format",
        '         let label = format!("{}", reason);\n  |                     ^^^^^^^^^^^^^^^^^^^^^^^^ help: use `reason.to_string()` or `&reason`',
    ),
    (
        "large enum variant difference",
        "clippy::large-enum-variant",
        " enum AdvisorySignal {\n  | ^^^^^^^^^^^^^^^\n  = note: ... variant size 232 bytes",
    ),
    (
        "this loop seems to only iterate over indices",
        "clippy::needless-range-loop",
        "     for i in 0..frames.len() {\n  |     ^^^^^^^^^^^^^^^^^^^^^^^^^^ help: iterate over `frames`",
    ),
    (
        "unused import: `std::borrow::Borrow`",
        "unused-imports",
        " use std::borrow::Borrow;\n  | ^^^^^^^^^^^^^^^^^^^^^^^^ help: remove it",
    ),
    (
        "function is never used: `legacy_verdict`",
        "dead-code",
        " fn legacy_verdict(tail: &str) -> &'static str {\n  | ^^^^^^^^^^^^^^^^^^^^^^^",
    ),
]


def f_clippy_lint(rng: random.Random) -> str:
    msg, lint, body = rng.choice(CLIPPY_LINTS)
    path = rng.choice(RUST_PATHS)
    ln, col = line_no(rng), col_no(rng)
    n = rng.choice([1, 1, 2, 3])
    return (
        f"error: {msg}\n"
        f" --> {path}:{ln}:{col}\n  |\n"
        f"{ln} |{body.splitlines()[0]}\n"
        f"  |{body.splitlines()[1] if chr(10) in body else ''}\n  |\n"
        f"  = note: `-D {lint}` implied by `-D warnings`\n"
        f"error: could not compile `remuda-hub` (lib) due to {n} previous error(s)"
    )


def f_clippy_cfg_target(rng: random.Random) -> str:
    path = rng.choice(RUST_PATHS)
    ln, col = line_no(rng), col_no(rng)
    return (
        "error: function `spawn_pty_master` is never used\n"
        f" --> {path}:{ln}:{col}\n  |\n"
        f"{ln} | fn spawn_pty_master() -> std::io::Result<RawFd> {{\n"
        "  | ^^^^^^^^^^^^^^^^^^^^^^\n  |\n"
        "note: the item is gated `#[cfg(target_os = \"linux\")]` but clippy ran the other target set\n"
        "  = note: `-D dead-code` implied by `-D warnings`\n"
        "error: could not compile `remuda-node` (lib) due to 1 previous error"
    )


def f_clippy_ice(rng: random.Random) -> str:
    path = rng.choice(RUST_PATHS)
    return (
        "error: internal compiler error: encountered incremental compilation corruption\n"
        f" --> {path}:{line_no(rng)}:1\n  |\n"
        "thread 'rustc' panicked at src/librustc_query_system/dep_graph/serialized.rs:226:13:\n"
        "assertion failed: `left == right`\n"
        "note: the compiler unexpectedly panicked. this is a bug.\n"
        "note: artifacts in the incremental cache are being removed; a clean rerun usually succeeds"
    )


def f_clippy_registry(rng: random.Random) -> str:
    tries = rng.choice([2, 3])
    return (
        "    Updating registry index\n"
        f"warning: spurious network error ({tries} tries remaining): [7] Couldn't connect to server: operation timed out\n"
        "error: failed to query replaced source registry `sparse-mirror`\n"
        "Caused by:\n"
        "  [7] Couldn't connect to server: operation timed out against the configured mirror"
    )


# ---------------------------------------------------------------------------
# cargo-test families (regression)
# ---------------------------------------------------------------------------

CT_REG_TESTS = [
    "retry_budget_resets_between_attempts",
    "report_marks_skipped_steps",
    "event_seq_is_gap_free",
    "follow_frame_settles_optimistic_pending",
    "store_flush_is_bounded_by_ack_deadline",
    "frame_order_is_rebuilt_after_gap",
    "approval_card_keeps_verb_kind",
    "split_children_share_parent_space",
]
CT_CRATES = ["remuda-node", "remuda-hub", "remuda-protocol", "remuda"]


def ct_header(test: str, path: str, ln: int, col: int, body: str,
              passed: int) -> str:
    return (
        f"test {test} ... FAILED\n\nfailures:\n\n"
        f"---- {test} stdout ----\n"
        f"thread '{test}' panicked at {path}:{ln}:{col}:\n"
        f"{body}\n"
        "note: run with `RUST_BACKTRACE=1` environment variable to display a backtrace\n\n"
        f"failures:\n    {test}\n\n"
        f"test result: FAILED. {passed} passed; 1 failed; 0 ignored; 0 measured; 0 filtered out\n"
        "error: test failed, take a look at the output for more details"
    )


def f_ct_assert_eq(rng: random.Random) -> str:
    test = f"{rng.choice(['runtime::tests', 'store::tests', 'gate::tests', 'frames::tests'])}::{rng.choice(CT_REG_TESTS)}"
    left, right = rng.choice(
        [("3", "2"), ("2", "3"), ("Some(RetryPlan::Drop)", "None"),
         ("412", "411"), ("true", "false"), (r'"esc"', r'"human"'),
         ("18", "17"), ("Ok(Ack::Committed)", "Err(Duplicate)")]
    )
    body = (
        "assertion `left == right` failed\n"
        f"  left: {left}\n right: {right}"
    )
    return ct_header(
        test, rng.choice(RUST_PATHS), line_no(rng), col_no(rng), body,
        rng.randint(80, 480),
    )


def f_ct_assert_custom(rng: random.Random) -> str:
    test = f"gate::tests::{rng.choice(CT_REG_TESTS)}"
    missing = rng.choice(
        ['skipped step "web-test" missing from report',
         'attempt 2 recorded although maxAttempts is 1',
         'step "secret-scan" ran after a failed step',
         'report line count is 9, expected 10']
    )
    body = f"assertion failed: report invariant broken: {missing}"
    return ct_header(test, "crates/remuda-node/src/gate.rs", line_no(rng),
                     col_no(rng), body, rng.randint(90, 200))


def f_ct_wire_golden(rng: random.Random) -> str:
    col = rng.choice([6, 14, 22])
    body = (
        "called `Result::unwrap()` on an `Err` value: "
        f'Error("trailing characters", line: 1, column: {col})'
    )
    return ct_header(
        "wire::every_specification_json_frame_has_one_lossless_golden",
        "crates/remuda-protocol/tests/wire_golden.rs", rng.randint(96, 118),
        rng.randint(20, 60), body, 0,
    )


def f_ct_sequence_wrong(rng: random.Random) -> str:
    test = "cmd::mcp::attachment::tests::an_agent_post_is_routed_to_own_session"
    left, right = rng.choice(
        [('["attachment.offer", "attachment.cancel"]',
          '["attachment.offer", "attachment.accept"]'),
         ('["attachment.offer", "attachment.accept"]',
          '["attachment.offer"]'),
         ('["session.bind", "attachment.offer"]',
          '["attachment.offer", "session.bind"]')]
    )
    body = (
        "assertion `left == right` failed: hub request sequence mismatch\n"
        f"  left: {left}\n right: {right}"
    )
    return ct_header(test, "crates/remuda/src/cmd/mcp/attachment.rs",
                     rng.randint(400, 520), col_no(rng), body,
                     rng.randint(120, 260))


def f_ct_compile(rng: random.Random) -> str:
    code, msg, detail = rng.choice(
        [("E0308", "mismatched types", "expected `u64`, found `u32`"),
         ("E0599", "no method named `remaining_budget` found for struct `Runtime` in the current scope", "method not found"),
         ("E0061", "this method takes 2 arguments but 3 arguments were supplied", "argument count"),
         ("E0425", "cannot find value `retry_budget` in this scope", "missing identifier"),
         ("E0382", "borrow of moved value: `frame`", "use after move"),
         ("E0277", "the trait bound `Verdict: From<u8>` is not satisfied", "trait bound")]
    )
    path = rng.choice(RUST_PATHS)
    ln, col = line_no(rng), col_no(rng)
    n = rng.choice([1, 2, 3])
    crate = rng.choice(CT_CRATES)
    return (
        f"error[{code}]: {msg}\n"
        f" --> {path}:{ln}:{col}\n  |\n"
        f"{ln} |             budget: Duration::from_millis(remaining),\n"
        f"  |                              ^^^^^^^^^ {detail}\n"
        f"error: could not compile `{crate}` (lib test) due to {n} previous error(s)"
    )


def f_ct_result_business(rng: random.Random) -> str:
    test = "store::tests::migration_0017_keeps_frame_kind_invariant"
    body = (
        "called `Result::unwrap()` on an `Err` value: "
        "CHECK constraint failed: frame_kind\n"
        "while appending the first event to a fresh in-memory journal"
    )
    return ct_header(test, "crates/remuda-hub/src/store.rs",
                     rng.randint(2100, 2400), col_no(rng), body, 0)


def f_ct_perf_regression(rng: random.Random) -> str:
    # Trap case: looks like a timing assertion, but the budget miss is caused
    # by newly added blocking work. Deterministic, so gold = regression.
    ms = rng.choice([640, 710, 862, 940])
    test = "runtime::tests::create_ack_stays_under_budget"
    body = (
        f"assertion failed: create() ack elapsed {ms}ms exceeds 200ms budget\n"
        "the reply path now performs a blocking store flush before answering"
    )
    return ct_header(test, "crates/remuda-node/src/runtime/mod.rs",
                     rng.randint(880, 960), col_no(rng), body,
                     rng.randint(60, 140))


# ---------------------------------------------------------------------------
# cargo-test families (flake)
# ---------------------------------------------------------------------------

def f_ct_known_timing(rng: random.Random) -> str:
    test = rng.choice(
        FLAKY_ROSTER[1:3]
        + [f"{rng.choice(['pty_queue::tests', 'scheduler::tests'])}::{rng.choice(FLAKY_ROSTER[6:])}"]
    )
    if "credit_starved" in test:
        body = (
            "assertion failed: hard cap respected\n"
            "  bytes_down: 0, elapsed_ms: %d, chunks_seen: 0\n"
            "  expected first byte within 1000ms of stream OPEN" % rng.choice([1310, 1430, 1504, 1462])
        )
        path = "crates/remuda-node/src/api_relay/relay_tests.rs"
    elif "slow_launch" in test:
        body = (
            f"assertion failed: create() ack elapsed {rng.choice([201, 205, 214, 220])}ms "
            "exceeds 200ms budget before the launch gate released"
        )
        path = "crates/remuda-node/src/runtime/mod.rs"
    else:
        body = (
            f"assertion failed: observed {rng.choice([3, 4])} wakeups in 50ms window, "
            f"expected 1; scheduling slice {rng.choice([11, 14, 19])}ms"
        )
        path = rng.choice(RUST_PATHS)
    return ct_header(test, path, rng.randint(1200, 3800), col_no(rng), body,
                     rng.randint(40, 220))


def f_ct_attachments_order(rng: random.Random) -> str:
    body = (
        "assertion failed: hub saw 0 unexpected requests\n"
        f"unexpected request #{rng.choice([2, 2, 3])}: PostAttachment {{ session: \"sess_other\" }}\n"
        "the duplicate post is only observed while another test still holds the hub fixture"
    )
    return ct_header(
        FLAKY_ROSTER[0], "crates/remuda/src/cmd/mcp/attachment.rs",
        rng.choice([379, 381, 384]), col_no(rng), body, rng.randint(150, 240),
    )


def f_ct_journal_race(rng: random.Random) -> str:
    body = (
        "journal append failed: json: EOF while parsing a string at line 1 column %d\n"
        "sibling writer still holds journal.lock after a hard node drop\n"
        "UNIQUE constraint failed: events.instance_id, events.seq" % rng.choice([712, 842, 901])
    )
    return ct_header(
        FLAKY_ROSTER[3], "crates/remuda-node/tests/journal_reopen.rs",
        rng.randint(170, 230), col_no(rng), body, 0,
    )


def f_ct_port_bind(rng: random.Random) -> str:
    test = "runtime::tests::cleanup_joins_before_port_rebind"
    body = (
        "called `Result::unwrap()` on an `Err` value: "
        'Os { code: 98, kind: AddrInUse, message: "Address already in use" }\n'
        "listener bind raced a previous test binary releasing the port"
    )
    return ct_header(test, "crates/remuda-node/src/runtime/mod.rs",
                     line_no(rng), col_no(rng), body, 0)


def f_ct_lock_wait(rng: random.Random) -> str:
    test = rng.choice(
        ["journal::tests::writer_lock_wait_is_bounded",
         "store::tests::second_opener_gets_locked_error",
         "journal::tests::reopen_after_drop_relinquishes_lock"]
    )
    wait = rng.choice([5, 5, 10])
    body = (
        "called `Result::unwrap()` on an `Err` value: "
        f'Error("Unable to acquire lock within {wait}s: another writer holds journal.lock")\n'
        "database is locked"
    )
    return ct_header(test, "crates/remuda-node/src/journal/mod.rs",
                     line_no(rng), col_no(rng), body, 0)


def f_ct_env_tzumask(rng: random.Random) -> str:
    test = rng.choice(
        ["fmt::tests::stamped_filename_uses_local_date",
         "store::tests::journal_file_is_created_with_private_mode",
         "format::tests::stamp_uses_runner_locale"]
    )
    body = rng.choice(
        ["assertion `left == right` failed\n  left: \"2026-09-23\"\n right: \"09/23/2026\"",
         "assertion `left == right` failed\n  left: 0o600 (384)\n right: 0o640 (416)",
         "assertion `left == right` failed\n  left: \"Sep 23 2026\"\n right: \"23 Sep 2026\""]
    )
    return ct_header(test, rng.choice(RUST_PATHS), line_no(rng), col_no(rng),
                     body, rng.randint(30, 90))


def f_ct_oom(rng: random.Random) -> str:
    return (
        "running 1 test binary\n"
        f"test binary target/debug/deps/remuda_node-{rng.randrange(16**6):06x}\n"
        "error: test failed: could not execute process (signal: 9 (SIGKILL))\n"
        "memory allocation of 16777216 bytes failed\n"
        "runner reported resident set at the cgroup ceiling across concurrent test binaries"
    )


# ---------------------------------------------------------------------------
# vitest families (regression)
# ---------------------------------------------------------------------------

def vt_fail_block(path: str, line: int, col: int, title: str, body: str) -> str:
    return (
        f" FAIL  {path} > {title}\n"
        f"{body}\n"
        f"  \u276f {path}:{line}:{col}\n"
        "      \u250c\n"
        f" {line:4d} |   it(\"{title}\", async () => {{\n"
        f"      \u2502            {body.splitlines()[0][:70]}\n"
        "      \u2514\n\n"
        " Test Files  1 failed (1)\n      Tests  1 failed (1) | 218 passed (219)"
    )


def f_vt_expect(rng: random.Random) -> str:
    path = rng.choice(WEB_PATHS)
    title = rng.choice(
        ["save draft keeps the verbatim title",
         "blocked row carries the space id",
         "mid tier shows no ember field",
         "compact fold keeps three tool calls",
         "pending chip settles from the durable projection",
         "approval card keeps the verb kind",
         "task rows sort blocked above working"])
    left, right = rng.choice(
        [('"Saved"', '"Saving\u2026"'), ('"esc"', '"human"'), ('3', '2'),
         ('"ultracode"', '"pending"'), ('true', 'false'),
         ('2', '3'), ('"regression"', '"unknown"')]
    )
    body = (
        f"AssertionError: expected {left} to be {right}\n"
        f"expected element [data-testid=\"state-chip\"] to have text {right}"
    )
    return vt_fail_block(path, line_no(rng), col_no(rng), title, body)


def f_vt_role(rng: random.Random) -> str:
    path = "src/features/tasks/TaskComposer.tsx"
    title = "split dialog exposes a save draft control"
    name = rng.choice(["save draft", "confirm split", "add child", "submit"])
    body = (
        f'TestingLibraryElementError: Unable to find an accessible element with the role "button" and name `/{name}/i`\n'
        "there were no matches for the queried role after the dialog opened"
    )
    return vt_fail_block(path, rng.randint(60, 220), col_no(rng), title, body)


def f_vt_transform(rng: random.Random) -> str:
    path = rng.choice(WEB_PATHS)
    ln, col = line_no(rng), col_no(rng)
    kind = rng.choice(
        [("error: Expected '}' to match '{'", "unbalanced braces after editing the reducer"),
         ("error: Expected ',' but found ')'", "missing argument separator"),
         ("error TS1005: ';' expected", "parser fell out of sync at this token"),
         ("error: Unterminated string literal", "string opened on a previous line never closes")]
    )
    return (
        f" FAIL  {path} > module transform\n"
        "Transform failed with 1 error:\n"
        f"{path}:{ln}:{col} - {kind[0]}\n"
        f"      {kind[1]}\n"
        " Test Files  1 failed (1)"
    )


def f_vt_snapshot(rng: random.Random) -> str:
    path = "src/features/session/transcript.tsx"
    title = "compact fold keeps the failed tool call visible"
    n = rng.choice([2, 3])
    return (
        f" FAIL  {path} > {title}\n"
        "toMatchSnapshot: snapshot does not match\n"
        "- Snapshot\n"
        "+ Received\n"
        f"- `<div class=\"fold\"> \u2026 {n + 1} calls </div>`\n"
        f"+ `<div class=\"fold\"> \u2026 {n} calls </div>`\n"
        f"  \u276f {path}:{rng.randint(800, 860)}:10\n"
        " Test Files  1 failed (1)\n      Tests  1 failed (1) | 191 passed (192)"
    )


def f_vt_throw_business(rng: random.Random) -> str:
    path = "src/features/tasks/taskRows.ts"
    title = "blocked count excludes child spaces"
    got = rng.choice([3, 4, 5])
    body = (
        f"Unhandled rejection: Error: expected blockedCount to equal 1, got {got}\n"
        "buildSpaces() counted split children before the parent space filter ran"
    )
    return vt_fail_block(path, rng.randint(240, 330), col_no(rng), title, body)


# ---------------------------------------------------------------------------
# vitest families (flake)
# ---------------------------------------------------------------------------

def f_vt_effort_roster(rng: random.Random) -> str:
    path = "src/features/session/EffortSlider.test.tsx"
    title = f"model mismatch (model-pin-1) > {FLAKY_ROSTER[4]}"
    left, right = rng.choice(
        [('"claude-sonnet[1m]"', '"anthropic:claude-sonnet[1m]"'),
         ('"anthropic:claude-sonnet[1m]"', '"claude-sonnet[1m]"')]
    )
    body = (
        f"AssertionError: expected {left} to be {right} (verbatim read-back)\n"
        "the catalog resolver spells the pin differently on this runner"
    )
    return vt_fail_block(path, rng.choice([300, 301, 303]), col_no(rng), title, body)


def f_vt_fake_timer(rng: random.Random) -> str:
    path = rng.choice(["src/lib/debounce.ts", "src/lib/poll.ts", "src/lib/store.ts"])
    title = rng.choice(
        ["trailing edge fires once under rapid input",
         "poll backoff resets after a visible frame",
         "hydrate cancels the stale refresh"])
    body = (
        f"AssertionError: expected {rng.choice([2, 3])} settle calls to be 1\n"
        "vi fake timers interleaved with a worker steal; the queued flush ran twice"
    )
    return vt_fail_block(path, rng.randint(50, 260), col_no(rng), title, body)


def f_vt_tz(rng: random.Random) -> str:
    path = "src/lib/formatTime.ts"
    title = "formats stamp in the project locale"
    body = rng.choice(
        ['AssertionError: expected "9/23/2026, 08:00 AM" to be "2026-09-23 08:00"',
         'AssertionError: expected "23.09.2026, 08:00" to be "2026-09-23 08:00"']
    )
    return vt_fail_block(path, line_no(rng), col_no(rng), title, body)


def f_vt_teardown_leak(rng: random.Random) -> str:
    path = "src/lib/poll.ts"
    return (
        f" FAIL  {path} > poll resumes after tab visibility returns\n"
        "Warning: a timer scheduled in the test was still pending in teardown\n"
        "AssertionError: expected promise to settle within 1000ms, it never did\n"
        f"  \u276f {path}:{rng.randint(70, 140)}:8\n"
        " Test Files  1 failed (1)\n      Tests  1 failed (1) | 88 passed (89)"
    )


def f_vt_worker_stall(rng: random.Random) -> str:
    path = "src/lib/store.ts"
    return (
        f" FAIL  {path} > hydrate merges a follow frame arriving early\n"
        "Error: Worker did not exit gracefully within 10s, forcing teardown of the thread stream\n"
        "the forced teardown dropped the assertion's last message\n"
        f"  \u276f {path}:{rng.randint(900, 1100)}:14\n"
        " Test Files  1 failed (1)\n      Tests  1 failed (1)"
    )


# ---------------------------------------------------------------------------
# playwright families (regression)
# ---------------------------------------------------------------------------

def pw_block(spec: str, line: int, title: str, detail: str, code_line: str) -> str:
    return (
        f"  1) [chromium] \u203a {spec}:{line}:5 \u203a {title}\n\n"
        f"    {detail}\n\n"
        f"  > {line} |   {code_line}\n"
        "        |                 ^\n"
        "\n  Slow test: 12.4s\n"
        "  1 failed (1)\n"
    )


def f_pw_text(rng: random.Random) -> str:
    expected, received = rng.choice(
        [("Saved", "Saving\u2026"), ("Sent", "Draft"),
         ("ultracode", "high"), ("Running", "Blocked"),
         ("3 splits", "2 splits"), ("Connected", "Replaying"),
         ("Approved", "Pending review")]
    )
    detail = (
        "Error: expect(received).toHaveText with timeout 10000ms\n\n"
        f"Expected string: \"{expected}\"\n"
        f"Received string: \"{received}\"\n\n"
        "Call log:\n"
        "  - waiting for locator('[data-testid=\"state-chip\"]')"
    )
    return pw_block(rng.choice(REG_SPEC_PATHS), rng.randint(60, 320),
                    rng.choice(["row renders the settled verdict",
                                "chip shows the final state",
                                "header carries the saved title"]),
                    detail, "await expect(chip).toHaveText('%s')" % expected)


def f_pw_missing_node(rng: random.Random) -> str:
    sel = rng.choice(
        ['aside [data-space-id] >> nth=2',
         '[data-testid="approval-row"] >> nth=1',
         'role=dialog[name="Split task"i]',
         '[data-effort-effective="ultracode"]']
    )
    detail = (
        f"Timed out 10000ms waiting for locator('{sel}')\n"
        "Expected a visible node for the split child\n"
        "DOM snapshot shows the node absent from the tree, not just off-screen"
    )
    return pw_block(rng.choice(REG_SPEC_PATHS), rng.randint(70, 200),
                    "split child renders in the parent space group",
                    detail, "await expect(childRows.nth(2)).toBeVisible()")


def f_pw_count(rng: random.Random) -> str:
    expected = rng.choice([3, 4, 5])
    got = expected - rng.choice([1, 1, 2])
    detail = (
        f"Error: expect(locator).toHaveCount: expected {expected}, received {got}\n"
        "locator: [data-testid=\"approval-row\"]\n"
        "the missing row was filtered out by the new space predicate"
    )
    return pw_block(rng.choice(REG_SPEC_PATHS), rng.randint(180, 320),
                    "every pending approval is listed", detail,
                    f"await expect(rows).toHaveCount({expected})")


def f_pw_webserver_build(rng: random.Random) -> str:
    ln = line_no(rng)
    detail = (
        "[webServer] dev build failed before serving the test page\n"
        f"src/features/session/store.ts:{ln}:7 - error TS2345: "
        "Argument of type 'PendingEffort' is not assignable to parameter of type 'SettledEffort'\n"
        "vite dev server exited with code 1 before tests started"
    )
    return pw_block("playwright.hub.config.ts", rng.randint(80, 110),
                    "global setup boots the hub web server", detail,
                    "webServer: viteDevServer('--strictPort')")


def f_pw_storage_state(rng: random.Random) -> str:
    detail = (
        "Error: setup invariant failed while loading storage state\n"
        "fixtures/hub-storage.json lists project member \"wsp_e2e\" but the "
        "member endpoint rejects legacy labels: expected a branded UUID\n"
        "no authenticated context was created for this spec"
    )
    return pw_block(rng.choice(REG_SPEC_PATHS), rng.randint(25, 60),
                    "test uses the branded hub storage state", detail,
                    "test.use({ storageState: 'fixtures/hub-storage.json' })")


# ---------------------------------------------------------------------------
# playwright families (flake)
# ---------------------------------------------------------------------------

def f_pw_load_timeout(rng: random.Random) -> str:
    ms = rng.choice([30000, 30000, 60000])
    blocked = rng.choice([24, 27, 31, 52])
    detail = (
        f"Test timeout of {ms}ms exceeded while running the spec.\n\n"
        f"page.goto: Timeout {ms}ms exceeded while waiting for event \"load\"\n"
        f"main thread was blocked for {blocked}s under concurrent build load"
    )
    return pw_block(rng.choice(SPEC_PATHS), rng.randint(40, 180),
                    "cold load reaches the inbox", detail,
                    "await page.goto('/')")


def f_pw_effort_pending(rng: random.Random) -> str:
    line = rng.choice([70, 226])
    want = rng.choice(["ultracode", "xhigh"])
    detail = (
        "Error: expect(received).toHaveAttribute\n"
        "Expected attribute: data-effort-effective\n"
        f'Expected value: "{want}"\n'
        'Received value: "pending"\n\n'
        "the configure ack was observed but the follow frame did not arrive"
    )
    return pw_block("tests/e2e/effort-sync.hub.spec.ts", line,
                    "slider settles after a push-down under load", detail,
                    "await expect(chip).toHaveAttribute('data-effort-effective', '%s')" % want)


def f_pw_port(rng: random.Random) -> str:
    port = rng.choice([59310, 59311, 59319, 41302])
    detail = (
        f"[webServer] Error: listen TCP: port {port} occupied by another process\n"
        "Error: Address already in use (os error 98)\n"
        f"    at globalSetup (playwright.hub.config.ts:{rng.randint(80, 96)}:14)"
    )
    return pw_block("playwright.hub.config.ts", 88,
                    "global setup reserves the hub e2e port", detail,
                    "await reservePort(%d)" % port)


def f_pw_browser_crash(rng: random.Random) -> str:
    pick = rng.choice(["closed", "missing"])
    if pick == "closed":
        detail = (
            "browserType.launch: Target page, context or browser has been closed unexpectedly\n"
            "the renderer process exited during startup; the browser service "
            "restarted on the host while this worker was scheduling"
        )
    else:
        build = rng.choice([1124, 1129, 1135])
        detail = (
            "browserType.launch: Executable doesn't exist at "
            f".ms-playwright/chromium-{build}/chrome-linux/chrome\n"
            "the browser cache for this runner was reclaimed before the run"
        )
    return pw_block(rng.choice(SPEC_PATHS), rng.randint(1, 30),
                    "spec worker launches chromium", detail,
                    "await chromium.launch()")


def f_pw_socket_gap(rng: random.Random) -> str:
    detail = (
        "Error: Timed out 5000ms waiting for a websocket frame matching effort\n"
        "Received frames before timeout: 0\n"
        "socket state stayed OPEN; the single read-back frame fell in the "
        "follow socket gap window"
    )
    return pw_block(rng.choice(SPEC_PATHS), rng.randint(100, 260),
                    "effort frame reaches the waiter", detail,
                    "await expect.poll(() => observed.effort).toBe('ultracode')")


def f_pw_trace_lock(rng: random.Random) -> str:
    detail = (
        "Error: EBUSY: resource busy or locked, unlink "
        "'test-results/.last-run/trace.zip'\n"
        "a previous worker's trace writer still held the file"
    )
    return pw_block(rng.choice(SPEC_PATHS), rng.randint(30, 240),
                    "worker tears down its trace output", detail,
                    "await context.close()")


# ---------------------------------------------------------------------------
# infra families (regression)
# ---------------------------------------------------------------------------

def f_infra_lockfile(rng: random.Random) -> str:
    pkg = rng.choice(["vitest", "@playwright/test", "vite", "@vitejs/plugin-react"])
    ver = rng.choice(["5.0.1", "1.64.0", "6.0.3", "4.3.2"])
    return (
        f"ERR_PNPM_OUT_OF_LOCKFILE: Cannot install with \"frozen-lockfile\" because pnpm-lock.yaml is not up to date with package.json\n"
        f"specifiers in the manifest are missing from the lockfile:\n  {pkg}: {ver}\n"
        "pnpm install exited with code 1"
    )


def f_infra_tsc(rng: random.Random) -> str:
    code, msg = rng.choice(
        [("TS2322", "Type 'string | undefined' is not assignable to type 'string'"),
         ("TS2345", "Argument of type 'PendingEffort' is not assignable to parameter of type 'SettledEffort'"),
         ("TS2339", "Property 'spaceId' does not exist on type 'TaskRow'"),
         ("TS18048", "'frame' is possibly 'null'")]
    )
    path = rng.choice(WEB_PATHS)
    ln, col = line_no(rng), col_no(rng)
    return (
        "> web build (tsc -b && vite build)\n"
        f"{path}:{ln}:{col} - error {code}: {msg}.\n"
        f"{ln}   return verbatimId;\n"
        "         ~~~~~~~~~~\n"
        "Found 1 error. Type check failed; vite build never started"
    )


def f_infra_secret_scan(rng: random.Random) -> str:
    rule = rng.choice(
        ["aws-access-key-id", "private-token-denylist",
         "anthropic-auth-token-assignment", "pem-private-key-block"]
    )
    path = rng.choice(
        ["scripts/tests/fixtures/example-cred.txt",
         "deploy/templates/bootstrap.env.example",
         "docs/design/evidence/example-runbook.md"]
    )
    return (
        "secret-scan: FAIL\n"
        f"{path}:12: matched rule {rule}\n"
        "the offending payload is withheld; the expected marker is redacted-secret-placeholder\n"
        "resolve before pushing: remove the literal or replace it with an approved placeholder"
    )


def f_infra_no_tunnel(rng: random.Random) -> str:
    return (
        "no-tunnel-scan: FAIL\n"
        "forbidden listener matched on the gate runner: an ssh process was "
        "started with a port-forwarding argument\n"
        "stop the forwarding process before rerunning the gate"
    )


def f_infra_vite_resolve(rng: random.Random) -> str:
    missing = rng.choice(
        ["./missing-composer-panel", "../stores/effortProjection",
         "./SplitDialogFooter", "@/features/tasks/useBlockedRows"]
    )
    return (
        "vite build for production\n"
        "error during build:\n"
        f'Rollup failed to resolve import "{missing}" from "src/features/tasks/index.ts".\n'
        "is the file spelled correctly? vite build exited with code 1"
    )


def f_infra_migration(rng: random.Random) -> str:
    n = rng.randint(11, 19)
    table = rng.choice(["frame_kind", "effort_tier_range", "space_kind_not_null", "seq_positive"])
    return (
        "hub e2e setup failed while applying migrations to the fresh temp database:\n"
        f"migration 00{n:02d}_projection failed at statement 4: "
        f"CHECK constraint failed: {table}\n"
        "every worker boots an empty database, so this failure reproduces on rerun"
    )


# ---------------------------------------------------------------------------
# infra families (flake)
# ---------------------------------------------------------------------------

def f_infra_disk(rng: random.Random) -> str:
    target = rng.choice(["pnpm package store", "cargo target directory", "playwright cache"])
    return (
        f"error: failed to write an artifact to the {target}: "
        "No space left on device (os error 28)\n"
        "the runner scratch volume filled during a concurrent warm build; "
        "retrying after cleanup succeeds"
    )


def f_infra_mirror(rng: random.Random) -> str:
    retry = rng.choice([2, 3, 5])
    return (
        "WARN fetch retried: the registry mirror closed the connection mid-tarball (ECONNRESET)\n"
        f"ERR_PNPM_FETCH_RETRY: request retry {retry}/5: spurious network error while resuming the download\n"
        "the mirror recovered on the next attempt"
    )


def f_infra_flock(rng: random.Random) -> str:
    wait = rng.choice([120, 120, 300])
    return (
        f"gate: web-hub-e2e: timed out after {wait}s waiting on file lock "
        "gate-e2e.lock (another gate worker is still running)\n"
        "no browser server was started; rerun once the other worker exits"
    )


def f_infra_oom_build(rng: random.Random) -> str:
    crate = rng.choice(["remuda-hub", "remuda-node", "remuda-protocol"])
    return (
        f"error: rustc was killed by signal 9 (SIGKILL) while linking {crate}\n"
        "fatal: memory exhausted spawning parallel codegen units while the "
        "machine was pinned to two cores for an e2e run"
    )


def f_infra_cache(rng: random.Random) -> str:
    pkg = rng.choice(["syn-2.0.1", "serde-1.0.210", "tokio-1.40.0", "vite-6.0.3"])
    return (
        f"error: archive index failed verification: malformed gzip header in the "
        f"cached registry entry for {pkg}\n"
        "the cached partial download is unreadable; deleting the cache entry fixes it"
    )


def f_infra_eacces(rng: random.Random) -> str:
    return (
        "ERR_PNPM_STORE_EACCES: EACCES: permission denied, open "
        "'.pnpm-store/v3/tmp/4d91/tarball.part'\n"
        "a leftover temp file was owned by a previous runner user; once "
        "cleaned the same install succeeds"
    )


# ---------------------------------------------------------------------------
# Family table: (signature, category, gate step, gold, count, modeled_on, fn)
# ---------------------------------------------------------------------------

FAMILIES = [
    # fmt (20): 18 regression / 2 flake
    ("fmt-diff", "fmt", "cargo-fmt", "regression", 18,
     "cargo fmt --check 'Diff in ... at line 919:' (deterministic)", f_fmt_diff),
    ("fmt-toolchain-drift", "fmt", "cargo-fmt", "flake", 2,
     "rustfmt version drift between runners (environment)", f_fmt_toolchain_drift),
    # clippy (28): 24 regression / 4 flake
    ("clippy-lint", "clippy", "cargo-clippy", "regression", 22,
     "clippy -D warnings lint/error output (deterministic)", f_clippy_lint),
    ("clippy-cfg-target", "clippy", "cargo-clippy", "regression", 2,
     "target-gated code triggers -D warnings on the gate target", f_clippy_cfg_target),
    ("clippy-ice", "clippy", "cargo-clippy", "flake", 2,
     "rustc incremental-cache ICE, clean rerun passes", f_clippy_ice),
    ("clippy-registry", "clippy", "cargo-clippy", "flake", 2,
     "registry mirror spurious network error", f_clippy_registry),
    # cargo-test (48): 27 regression / 21 flake
    ("ct-assert-eq", "cargo-test", "cargo-test", "regression", 6,
     "libtest left/right assertion failures", f_ct_assert_eq),
    ("ct-assert-custom", "cargo-test", "cargo-test", "regression", 4,
     "libtest assert!/invariant failures", f_ct_assert_custom),
    ("ct-wire-golden", "cargo-test", "cargo-test", "regression", 2,
     "wire_golden Error(\"trailing characters\") panic (this week, real)", f_ct_wire_golden),
    ("ct-sequence-wrong", "cargo-test", "cargo-test", "regression", 4,
     "deterministic hub request sequence mismatch (variant is wrong)", f_ct_sequence_wrong),
    ("ct-compile", "cargo-test", "cargo-test", "regression", 6,
     "error[E....] compile failures surfaced by cargo test", f_ct_compile),
    ("ct-result-business", "cargo-test", "cargo-test", "regression", 3,
     "deterministic Err from a fresh in-memory journal", f_ct_result_business),
    ("ct-perf-regression", "cargo-test", "cargo-test", "regression", 2,
     "timing assertion failing because new blocking code was added (trap case)", f_ct_perf_regression),
    ("ct-known-timing", "cargo-test", "cargo-test", "flake", 5,
     "create_is_accepted_before_a_slow_launch_finishes / credit_starved timing flakes (real)", f_ct_known_timing),
    ("ct-attachments-order", "cargo-test", "cargo-test", "flake", 4,
     "never_reaches_the_hub: duplicate post only while fixture shared (real)", f_ct_attachments_order),
    ("ct-journal-race", "cargo-test", "cargo-test", "flake", 3,
     "a_reopened_store... json EOF / UNIQUE seq dual-writer race (real)", f_ct_journal_race),
    ("ct-port-bind", "cargo-test", "cargo-test", "flake", 2,
     "AddrInUse racing port release between test binaries", f_ct_port_bind),
    ("ct-lock-wait", "cargo-test", "cargo-test", "flake", 2,
     "bounded journal lock wait / database is locked", f_ct_lock_wait),
    ("ct-env-tzumask", "cargo-test", "cargo-test", "flake", 3,
     "TZ/locale/umask sensitive assertions (machine-dependent)", f_ct_env_tzumask),
    ("ct-oom", "cargo-test", "cargo-test", "flake", 2,
     "SIGKILL at cgroup memory ceiling under concurrent binaries", f_ct_oom),
    # vitest (36): 21 regression / 15 flake
    ("vt-expect", "vitest", "web-test", "regression", 7,
     "vitest AssertionError on rendered value", f_vt_expect),
    ("vt-role", "vitest", "web-test", "regression", 4,
     "TestingLibrary unable to find role/name", f_vt_role),
    ("vt-transform", "vitest", "web-test", "regression", 4,
     "esbuild/TS transform/parse failure (deterministic)", f_vt_transform),
    ("vt-snapshot", "vitest", "web-test", "regression", 3,
     "inline snapshot drift (deterministic)", f_vt_snapshot),
    ("vt-throw-business", "vitest", "web-test", "regression", 3,
     "unhandled rejection carrying a real invariant mismatch", f_vt_throw_business),
    ("vt-effort-roster", "vitest", "web-test", "flake", 4,
     "EffortSlider verbatim fallback: red on one machine, green on another (real)", f_vt_effort_roster),
    ("vt-fake-timer", "vitest", "web-test", "flake", 5,
     "fake timers interleaved with worker steal (load timing)", f_vt_fake_timer),
    ("vt-tz", "vitest", "web-test", "flake", 2,
     "locale/date formatting differs across runners", f_vt_tz),
    ("vt-teardown-leak", "vitest", "web-test", "flake", 2,
     "pending timer in teardown, promise never settles", f_vt_teardown_leak),
    ("vt-worker-stall", "vitest", "web-test", "flake", 2,
     "vitest worker forced teardown dropped last message", f_vt_worker_stall),
    # playwright (44): 21 regression / 23 flake
    ("pw-text", "playwright", "web-hub-e2e", "regression", 7,
     "toHaveText Expected/Received mismatch (product changed)", f_pw_text),
    ("pw-missing-node", "playwright", "web-hub-e2e", "regression", 4,
     "node absent from DOM tree (not merely slow)", f_pw_missing_node),
    ("pw-count", "playwright", "web-hub-e2e", "regression", 4,
     "toHaveCount wrong count after predicate change", f_pw_count),
    ("pw-webserver-build", "playwright", "web-hub-e2e", "regression", 3,
     "playwright webServer fails on tsc/vite build error", f_pw_webserver_build),
    ("pw-storage-state", "playwright", "web-hub-e2e", "regression", 3,
     "storage state/fixture invariant edited incorrectly", f_pw_storage_state),
    ("pw-load-timeout", "playwright", "web-hub-e2e", "flake", 6,
     "page load 30s timeout under concurrent build load", f_pw_load_timeout),
    ("pw-effort-pending", "playwright", "web-hub-e2e", "flake", 5,
     "effort-sync.hub.spec.ts pending chip, frame lost in follow gap (real)", f_pw_effort_pending),
    ("pw-port", "playwright", "web-hub-e2e", "flake", 4,
     "hub e2e port occupied / AddrInUse in globalSetup", f_pw_port),
    ("pw-browser-crash", "playwright", "web-hub-e2e", "flake", 3,
     "renderer closed at startup or browser binary cache reclaimed", f_pw_browser_crash),
    ("pw-socket-gap", "playwright", "web-hub-e2e", "flake", 3,
     "single read-back frame lost in follow socket gap (hard case)", f_pw_socket_gap),
    ("pw-trace-lock", "playwright", "web-hub-e2e", "flake", 2,
     "EBUSY trace.zip still held by previous worker", f_pw_trace_lock),
    # infra (28): 15 regression / 13 flake
    ("infra-lockfile", "infra", "web-install", "regression", 4,
     "pnpm ERR_PNPM_OUT_OF_LOCKFILE frozen install", f_infra_lockfile),
    ("infra-tsc", "infra", "web-build", "regression", 4,
     "web build tsc error TS....", f_infra_tsc),
    ("infra-secret-scan", "infra", "secret-scan", "regression", 2,
     "secret-scan matched a literal that must not ship", f_infra_secret_scan),
    ("infra-no-tunnel", "infra", "no-tunnel-scan", "regression", 1,
     "no-tunnel-scan found a forwarding listener", f_infra_no_tunnel),
    ("infra-vite-resolve", "infra", "web-build", "regression", 2,
     "Rollup failed to resolve a missing import", f_infra_vite_resolve),
    ("infra-migration", "infra", "web-hub-e2e", "regression", 2,
     "fresh-database migration fails deterministically", f_infra_migration),
    ("infra-disk", "infra", "web-install", "flake", 3,
     "ENOSPC scratch volume under concurrent build", f_infra_disk),
    ("infra-mirror", "infra", "web-install", "flake", 3,
     "registry mirror ECONNRESET / spurious network error", f_infra_mirror),
    ("infra-flock", "infra", "web-hub-e2e", "flake", 2,
     "gate-e2e.lock contention between gate workers (real)", f_infra_flock),
    ("infra-oom-build", "infra", "cargo-check", "flake", 2,
     "rustc SIGKILL linking under cpu pin", f_infra_oom_build),
    ("infra-cache", "infra", "web-install", "flake", 2,
     "corrupted registry cache archive", f_infra_cache),
    ("infra-eacces", "infra", "web-install", "flake", 1,
     "leftover store temp file owned by another runner", f_infra_eacces),
]


def build_corpus(seed: int = SEED) -> list[dict]:
    """Generate every corpus record. Family order and ids are deterministic."""
    rng = random.Random(seed)
    records = []
    seq = 0
    for signature, category, step, gold, count, _modeled, fn in FAMILIES:
        for _ in range(count):
            seq += 1
            records.append(
                {
                    "id": f"c{seq:04d}",
                    "category": category,
                    "step": step,
                    "signature": signature,
                    "gold": gold,
                    "log": fn(rng),
                }
            )
    return records


# ---------------------------------------------------------------------------
# Synthetic Jev-style response predictor (harness self-check only)
# ---------------------------------------------------------------------------

# Hand-set per-family "separability": how strongly the family pushes toward
# its gold label for an imagined reader that understands log text. Easy
# families have structural cues; hard families are the whole reason a model
# might beat rules.
FAMILY_STRENGTH = {
    "fmt-diff": 2.4, "fmt-toolchain-drift": 1.0,
    "clippy-lint": 2.4, "clippy-cfg-target": 2.2,
    "clippy-ice": 1.8, "clippy-registry": 1.7,
    "ct-assert-eq": 1.25, "ct-assert-custom": 1.15,
    "ct-wire-golden": 1.5, "ct-sequence-wrong": 1.3,
    "ct-compile": 2.4, "ct-result-business": 1.2,
    "ct-perf-regression": 0.55,
    "ct-known-timing": 1.7, "ct-attachments-order": 1.8,
    "ct-journal-race": 1.7, "ct-port-bind": 2.0,
    "ct-lock-wait": 1.9, "ct-env-tzumask": 0.7, "ct-oom": 1.9,
    "vt-expect": 1.2, "vt-role": 1.25, "vt-transform": 2.2,
    "vt-snapshot": 1.3, "vt-throw-business": 1.15,
    "vt-effort-roster": 1.8, "vt-fake-timer": 0.9,
    "vt-tz": 0.7, "vt-teardown-leak": 0.9, "vt-worker-stall": 1.0,
    "pw-text": 1.35, "pw-missing-node": 1.1, "pw-count": 1.3,
    "pw-webserver-build": 2.2, "pw-storage-state": 0.7,
    "pw-load-timeout": 1.35, "pw-effort-pending": 1.8, "pw-port": 2.0,
    "pw-browser-crash": 1.8, "pw-socket-gap": 0.6, "pw-trace-lock": 1.9,
    "infra-lockfile": 2.3, "infra-tsc": 2.3, "infra-secret-scan": 2.1,
    "infra-no-tunnel": 2.1, "infra-vite-resolve": 2.2, "infra-migration": 1.9,
    "infra-disk": 1.9, "infra-mirror": 1.8, "infra-flock": 2.0,
    "infra-oom-build": 1.9, "infra-cache": 0.9, "infra-eacces": 0.8,
}

# Cue substrings scored on the lowercased log. Positive = regression,
# negative = flake. Deliberately imperfect: generic words are weak,
# structural phrases are strong, and a few cues conflict (timeouts appear
# on both sides).
CUES = [
    (2.0, ("error[e", "could not compile")),
    (1.7, ("diff in",)),
    (1.7, ("-d clippy::", "-d warnings", "-d dead-code")),
    (1.7, ("error ts", "transform failed")),
    (1.8, ("frozen-lockfile",)),
    (1.8, ("rollup failed to resolve",)),
    (1.7, ("secret-scan: fail", "no-tunnel-scan: fail")),
    (1.4, ("mismatched types", "no method named", "cannot find value",
           "trait bound", "trailing characters")),
    (1.6, ("migration 00",)),
    (0.9, ("- snapshot",)),
    (0.35, ("assertion",)),
    (0.25, ("expected",)),
    (0.15, ("received",)),
    (0.1, ("failed", "error:")),
    (-1.8, ("internal compiler error", "thread 'rustc' panicked")),
    (-1.6, ("address already in use", "os error 98", "kind: addrinuse")),
    (-1.5, ("occupied by another process",)),
    (-1.4, ("database is locked", "unable to acquire lock")),
    (-1.5, ("no space left on device", "os error 28")),
    (-1.5, ("spurious network",)),
    (-1.5, ("signal: 9 (sigkill)", "killed by signal 9")),
    (-1.4, ("browser has been closed", "executable doesn't exist")),
    (-1.4, ("resource busy or locked",)),
    (-1.5, ("gate-e2e.lock",)),
    (-1.3, ("pending timer", "worker did not exit gracefully")),
    (-0.55, ("timed out", "timeout of", "test timeout of")),
    (-0.35, ("econnreset",)),
    (-0.5, ("readonly-stale", "frame lost in the follow socket gap",
             "fell in the follow socket gap", "follow frame did not arrive")),
]

ROSTER_CUE_WEIGHT = -1.4
NOISE_SIGMA = 1.05
# Calibration intercept for the corpus regression prior (126/204 ≈ 0.618).
INTERCEPT = 0.52


def _sigmoid(x: float) -> float:
    return 1.0 / (1.0 + math.exp(-x))


def _cue_score(log: str) -> float:
    text = log.lower()
    score = 0.0
    for weight, needles in CUES:
        if any(n in text for n in needles):
            score += weight
    if any(name in log for name in FLAKY_ROSTER):
        score += ROSTER_CUE_WEIGHT
    return score


def _question_block() -> dict:
    return {
        "key": QUESTION_KEY,
        "primitive": "choice",
        "options": OPTIONS,
        "wording": (
            "Given this failed gate step's log tail, if the gate is rerun "
            "unchanged on a healthy runner, does the same failure recur "
            "because of the code under test?"
        ),
        "regression_option_meaning": (
            "failure recurs: a real regression. Conservatively safe use: "
            "drop the scheduled automatic retry."
        ),
        "flake_option_meaning": (
            "failure is transient: timing, environment, or contention."
        ),
    }


def _response_entry(rec, p_reg, confidence, latency):
    p_flake = round(1.0 - p_reg, 4)
    p_reg = round(1.0 - p_flake, 4)
    return {
        "id": rec["id"],
        "latency_ms": latency,
        "answers": {
            QUESTION_KEY: {
                "choice": "regression" if p_reg >= 0.5 else "flake",
                "probabilities": {"flake": p_flake, "regression": p_reg},
                "confidence": confidence,
            }
        },
    }


def _pass_profile_entry(rng: random.Random, rec: dict) -> dict:
    direction = 1.0 if rec["gold"] == "regression" else -1.0
    latent = (
        INTERCEPT
        + direction * FAMILY_STRENGTH[rec["signature"]]
        + 0.55 * _cue_score(rec["log"])
        + rng.gauss(0.0, NOISE_SIGMA)
    )
    p_reg = _sigmoid(latent)
    p_flake = round(1.0 - p_reg, 4)
    p_reg = round(1.0 - p_flake, 4)
    conf = abs(p_reg - p_flake) + max(0.0, rng.gauss(0.0, 0.05))
    confidence = round(min(1.0, max(0.0, conf)), 3)
    # Latency is independent of correctness: a bounded, mildly skewed
    # spread well inside the 900ms p95 budget the real run must prove.
    latency = 220 + int(520 * rng.random() ** 2.1 + rng.gauss(0.0, 45.0))
    latency = min(880, max(150, latency))
    return _response_entry(rec, p_reg, confidence, latency)


def _fail_profile_entries(corpus: list[dict], rng: random.Random) -> list[dict]:
    """Negative control: decisively fails thresholds 1 and 3.

    A block of flake records gets high regression probabilities that sit
    ABOVE every regression record's probability, so no threshold can both
    keep precision >= 0.95 and predict anything. A tenth of latencies are
    injected above the 900ms p95 budget. The point is to prove the judge
    prints no-go when a run actually deserves it.
    """
    flake_records = [rec for rec in corpus if rec["gold"] == "flake"]
    wrong = {
        rec["id"]
        for rec in rng.sample(flake_records, FAIL_PROFILE_WRONG_FLAKES)
    }
    out = []
    for rec in corpus:
        if rec["id"] in wrong:
            p_reg = rng.uniform(0.985, 0.995)
        elif rec["gold"] == "regression":
            p_reg = rng.uniform(0.60, 0.92)
        else:
            p_reg = rng.uniform(0.02, 0.28)
        if rng.random() < 0.10:
            latency = int(rng.uniform(950, 1400))
        else:
            latency = 220 + int(520 * rng.random() ** 2.1
                                + rng.gauss(0.0, 45.0))
            latency = min(880, max(150, latency))
        confidence = round(abs(p_reg - (1.0 - p_reg)), 3)
        out.append(_response_entry(rec, p_reg, confidence, latency))
    return out


def build_responses(corpus: list[dict], seed: int = RESPONSE_SEED,
                    profile: str = "pass") -> dict:
    """Produce a synthetic response fixture (one envelope entry per record).

    profile="pass" mirrors a plausible strong model: the latent score is
    gold-correlated through the per-family strength, plus the same cue text
    a reader sees, plus Gaussian noise. profile="fail" is a negative
    control that must trip the go/no-go judge. Neither profile is model
    output; both exercise the evaluation harness only.
    """
    rng = random.Random(seed)
    if profile == "pass":
        out = [_pass_profile_entry(rng, rec) for rec in corpus]
        artifact_kind = "synthetic-offline-fixture-not-model-output"
        warning = (
            "Responses are generated by generate_corpus.py's seeded synthetic "
            "predictor. They exercise the evaluation harness only and say "
            "nothing about the real Jev model. No network was ever used."
        )
        designed_to_fail = []
    elif profile == "fail":
        out = _fail_profile_entries(corpus, rng)
        artifact_kind = "synthetic-offline-negative-control-not-model-output"
        warning = (
            "Responses are deliberately degraded synthetic data, generated by "
            "generate_corpus.py. They must make the offline judge print "
            "no-go. Nothing here is model output and no network was ever used."
        )
        designed_to_fail = [
            "threshold-1-regression-precision: confident wrong answers on "
            f"{FAIL_PROFILE_WRONG_FLAKES} flake records sit above every "
            "regression probability, so no operating point reaches 0.95",
            "threshold-2-coverage-gain: undefined once no precision-compliant "
            "operating point exists",
            "threshold-3-p95-latency: roughly one response in ten is injected "
            "above 900ms",
        ]
    else:  # pragma: no cover - defensive
        raise ValueError(f"unknown profile: {profile!r}")
    envelope = {
        "artifact_kind": artifact_kind,
        "warning": warning,
        "model": MODEL_PIN,
        "profile": profile,
        "seed": seed,
        "question": _question_block(),
        "responses": out,
    }
    if designed_to_fail:
        envelope["designed_to_fail"] = designed_to_fail
    return envelope


# ---------------------------------------------------------------------------
# Manifest + writing
# ---------------------------------------------------------------------------

def _sha256_text(text: str) -> str:
    return hashlib.sha256(text.encode("utf-8")).hexdigest()


def build_manifest(corpus: list[dict], corpus_sha: str,
                   responses_sha: str,
                   responses_fail_sha: str) -> dict:
    by_cat: dict[str, dict] = {
        c: {"regression": 0, "flake": 0, "total": 0} for c in CATEGORIES
    }
    families = []
    for signature, category, step, gold, count, modeled, _fn in FAMILIES:
        families.append(
            {
                "signature": signature,
                "category": category,
                "step": step,
                "gold": gold,
                "count": count,
                "modeled_on": modeled,
            }
        )
    for rec in corpus:
        by_cat[rec["category"]][rec["gold"]] += 1
        by_cat[rec["category"]]["total"] += 1
    return {
        "schema_version": SCHEMA_VERSION,
        "artifact": "c-jevspike synthetic gate-failure triage corpus (offline)",
        "seed": SEED,
        "generator": "scripts/tests/fixtures/jev-spike/generate_corpus.py",
        "record_schema": {
            "id": "c#### sequential, stable across regenerations",
            "category": "one of the six reporting categories",
            "step": "gate.sh step name where the failure surfaced",
            "signature": "hand-authored template family; gold is fixed per family",
            "gold": "regression (fails again unchanged) or flake (transient)",
            "log": "synthetic multi-line log tail, repo-relative paths only",
        },
        "categories": CATEGORIES,
        "total": len(corpus),
        "counts_by_category": by_cat,
        "families": families,
        "known_flaky_roster": FLAKY_ROSTER,
        "sanitization_rules": [
            "no /home/ or /Users/ home paths",
            "no C:\\\\Users or $HOME or ~/ home forms",
            "no IPv4 dotted addresses",
            "no @ email forms (scoped package names such as @playwright/test "
            "contain @ but are not emails)",
            "no hostnames or domains",
            "no tokens; secret-like output is always a described rule name",
        ],
        "corpus_sha256": corpus_sha,
        "responses_fixture_sha256": responses_sha,
        "responses_fail_fixture_sha256": responses_fail_sha,
    }


def _dumps(obj, *, indent=None):
    return json.dumps(obj, ensure_ascii=True, indent=indent, sort_keys=True)


def write_all(outdir: Path) -> None:
    corpus = build_corpus()
    corpus_text = "".join(
        json.dumps(rec, ensure_ascii=True, sort_keys=True) + "\n"
        for rec in corpus
    )
    responses = build_responses(corpus, profile="pass")
    responses_text = _dumps(responses, indent=2) + "\n"
    responses_fail = build_responses(corpus, profile="fail")
    responses_fail_text = _dumps(responses_fail, indent=2) + "\n"
    manifest = build_manifest(
        corpus, _sha256_text(corpus_text), _sha256_text(responses_text),
        _sha256_text(responses_fail_text),
    )
    manifest_text = _dumps(manifest, indent=2) + "\n"
    (outdir / "corpus.jsonl").write_text(corpus_text, encoding="utf-8")
    (outdir / "responses.fixture.json").write_text(responses_text, encoding="utf-8")
    (outdir / "responses.fail.fixture.json").write_text(
        responses_fail_text, encoding="utf-8")
    (outdir / "manifest.json").write_text(manifest_text, encoding="utf-8")


def main() -> None:
    outdir = Path(__file__).resolve().parent
    write_all(outdir)
    corpus = build_corpus()
    print(f"wrote {len(corpus)} records, {len(FAMILIES)} families to {outdir}")


if __name__ == "__main__":
    main()
