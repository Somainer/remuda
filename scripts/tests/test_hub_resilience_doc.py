"""Static checks for the c-hubresil hub-resilience spec (docs-only).

No network, no subprocess: this test only reads files.

It verifies that ``docs/design/hub-resilience.md``:

1. Anchors every code claim to a real ``path.rs:line`` (or line range) — the
   doc's whole point is "current behavior, with file:line", so a drifting anchor
   is a failing claim;
2. Covers the required resilience vocabulary (buffering, backoff, jitter, the
   four phone states, egress);
3. Keeps the hard-failure contract and the owner-ruling boundaries visible;
4. Leaks no hostnames/domains/IP literals/home paths/token-shaped secrets;
5. Lands the egress invariant in ``api-routing.md`` and anchors the four
   permitted .rs files with doc comments only.
"""

import re
import unittest
from pathlib import Path

ROOT = Path(__file__).resolve().parents[2]
DESIGN = ROOT / "docs" / "design"

DOC = DESIGN / "hub-resilience.md"
API_ROUTING = DESIGN / "api-routing.md"

# Doc-comment-only anchors c-hubresil was allowed to touch.
ANCHOR_FILES = [
    ROOT / "crates/remuda-node/src/daemon.rs",
    ROOT / "crates/remuda-node/src/transport/mod.rs",
    ROOT / "crates/remuda-hub/src/api_relay.rs",
    ROOT / "crates/remuda-hub/src/ws.rs",
]

# path/name with a tracked source suffix, followed by :line or :line-line
REFERENCE = re.compile(
    r"(?P<ref>[A-Za-z0-9_./-]+\.(?:rs|md|py|ts|toml)):(?P<start>\d+)(?:-(?P<end>\d+))?"
)

REQUIRED_KEYWORDS = [
    # Buffering spec.
    "缓冲", "journal.softfull", "node-buffer-full",
    # Reconnect storm.
    "退避", "jitter", "retryAfterMs", "2–8 s", "60 s",
    # Honest staleness states (English wire states + Chinese copy).
    "live", "stale", "disconnected", "recovering",
    "follow.tick", "follow.degraded",
    # Egress invariant.
    "egress", "EgressSnapshot",
    # Anchored contracts that must survive.
    "journal acknowledgement timed out; reconnect required",
    "D-035", "D-049",
]

# Fragments that must appear at specific cited lines, so an anchor cannot drift
# onto a neighboring unrelated line unnoticed.
LINE_FRAGMENTS = [
    ("crates/remuda-node/src/daemon.rs", 672,
     "journal acknowledgement timed out; reconnect required"),
    ("crates/remuda-node/src/daemon.rs", 852, "pending.len() >= 16"),
    ("crates/remuda-node/src/transport/mod.rs", 66, "from_secs(1)"),
    ("crates/remuda-node/src/transport/mod.rs", 68, "jitter_ppt: 250"),
    ("crates/remuda-node/src/transport/wss.rs", 529, "Message::Ping(payload)"),
    ("crates/remuda-node/src/transport/wss.rs", 1295,
     "hub wss reconnecting; commands are not replayed"),
    ("crates/remuda-hub/src/ws.rs", 1247, "lease_ttl_ms: 60_000"),
    ("crates/remuda-hub/src/ws.rs", 1815, '"type": "gap"'),
    ("crates/remuda-hub/src/store.rs", 4869, "PRIMARY KEY (instance_id, seq)"),
    ("crates/remuda-hub/src/store.rs", 2013, "'offline'"),
    ("crates/remuda-hub/src/config.rs", 143, "600_000"),
    ("crates/remuda-hub/src/api_relay.rs", 130, "struct EgressSnapshot"),
    ("crates/remuda-hub/src/api_relay.rs", 351, "auth_token: snapshot.secret"),
    ("crates/remuda-journal/src/store.rs", 461, '"WAL"'),
    ("crates/remuda-driver/src/interaction.rs", 20, "15 * 60"),
    ("web/src/features/workspaces/follow.ts", 26, "setTimeout(connect, 1000)"),
    ("web/src/lib/api.ts", 1590, "new WebSocket(followUrl(instanceId))"),
]


def iter_references(text):
    """Yield (ref, start, end); only repo-rooted or slash-relative paths."""
    for match in REFERENCE.finditer(text):
        ref = match.group("ref")
        if "/" in ref and not ref.startswith(
            ("crates/", "docs/", "scripts/", "deploy/", "web/")
        ):
            continue
        start = int(match.group("start"))
        end = int(match.group("end") or start)
        yield ref, start, end


def resolve_references(text):
    """Resolve every reference, mapping bare ``name.rs`` shorthands to the
    most recently seen full path with that basename (document-order)."""
    shorthand: dict[str, Path] = {}
    resolved = []
    problems = []
    for ref, start, end in iter_references(text):
        if "/" in ref:
            path = ROOT / ref
            shorthand[path.name] = path
        else:
            if ref not in shorthand:
                problems.append(f"shorthand {ref!r}:{start} used before any full path")
                continue
            path = shorthand[ref]
        resolved.append((ref, path, start, end))
    return resolved, problems


class HubResilienceDocTests(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        cls.text = DOC.read_text(encoding="utf-8")
        cls.api = API_ROUTING.read_text(encoding="utf-8")

    # ── 1. every file:line reference resolves ──────────────────────────────

    def test_every_file_line_reference_exists(self):
        resolved, problems = resolve_references(self.text)
        for _ref, path, start, end in resolved:
            if not path.is_file():
                problems.append(f"{path}: file does not exist")
                continue
            line_count = len(path.read_text(encoding="utf-8").splitlines())
            if start < 1 or end < start or end > line_count:
                problems.append(
                    f"{path}:{start}-{end} outside file (1..{line_count})"
                )
        self.assertEqual([], problems, "\n".join(problems))

    def test_pinned_line_fragments_hold(self):
        for relative, number, fragment in LINE_FRAGMENTS:
            lines = (ROOT / relative).read_text(encoding="utf-8").splitlines()
            self.assertIn(
                fragment,
                lines[number - 1],
                f"{relative}:{number} = {lines[number - 1]!r}, "
                f"expected fragment {fragment!r}",
            )

    def test_doc_cites_a_representative_anchor_for_every_finding(self):
        # The 现状 section must ground each headline claim with at least one
        # anchor to the file that implements it.
        must_cite = [
            "crates/remuda-node/src/transport/mod.rs:",
            "crates/remuda-node/src/transport/wss.rs:",
            "crates/remuda-node/src/daemon.rs:",
            "crates/remuda-node/src/journal_flush.rs:",
            "crates/remuda-node/src/transport/wss/runtime_wss.rs:",
            "crates/remuda-journal/src/store.rs:",
            "crates/remuda-node/src/interactions.rs:",
            "crates/remuda-driver/src/interaction.rs:",
            "crates/remuda-hub/src/ws.rs:",
            "crates/remuda-hub/src/store.rs:",
            "crates/remuda-hub/src/api_relay.rs:",
            "crates/remuda-hub/src/alerts.rs:",
            "crates/remuda-hub/src/config.rs:",
            "web/src/lib/api.ts:",
            "web/src/features/workspaces/follow.ts:",
        ]
        missing = [ref for ref in must_cite if ref not in self.text]
        self.assertEqual([], missing, f"uncited implementation files: {missing}")

    # ── 2. keyword coverage ────────────────────────────────────────────────

    def test_required_keywords_are_covered(self):
        missing = [w for w in REQUIRED_KEYWORDS if w not in self.text]
        self.assertEqual([], missing, f"spec missing keywords: {missing}")

    def test_four_states_have_exact_criteria_rows(self):
        for state in ("`live`", "`stale`", "`disconnected`", "`recovering`"):
            self.assertIn(state, self.text)
        # Thresholds and their rationale must be stated, not implied.
        for token in ("15 s", "45 s", "5 s"):
            self.assertIn(token, self.text)

    def test_disconnected_refuses_new_sessions_with_required_copy(self):
        self.assertIn("拒绝", self.text)
        self.assertIn("不静默入队", self.text)
        self.assertIn("会话不会在离线时排队", self.text)
        self.assertIn("恢复连接后再发送", self.text)

    def test_egress_section_is_an_invariant_not_a_front_door_argument(self):
        marker = self.text.index("## 6.")
        section = self.text[marker:self.text.index("## 7.")]
        self.assertIn("不得以明文经过任何既不是 Hub、也不是目标 Node 的第三方",
                      section)
        self.assertIn("未实现", section)
        # It must explicitly disclaim the front-door argument.
        self.assertIn("不", section)
        self.assertIn("前门", section)

    # ── 3. honesty: code-has vs plan-says stays explicit ───────────────────

    def test_reality_table_distinguishes_code_from_plan(self):
        self.assertIn("代码里有", self.text)
        self.assertIn("只是计划里写过", self.text)
        for claim in (
            "journal 容量水位 / 「过夜」承诺",
            "服务端重连退让",
            "手机 live/stale/断开/恢复四态",
            "主动 Ping / keepalive",
        ):
            self.assertIn(claim, self.text)

    def test_overnight_buffer_is_not_claimed_as_existing(self):
        # The doc must say the 60-minute promise is absent from code today.
        self.assertIn("「过夜缓冲」（60 分钟目标）在代码里不存在为承诺",
                      self.text)

    # ── 4. leak scan: no hosts/IPs/home paths/secrets in new prose ────────

    def test_new_docs_leak_no_environment_artifacts(self):
        api_section = self.api[self.api.index("## 11."):]
        for text in (self.text, api_section):
            self.assertNotIn("/" + "home/", text)
            self.assertNotIn("/" + "Users/", text)
            self.assertNotIn("~" + "/", text)
            ip_literal = re.compile(r"(?<!\d)(?:\d{1,3}\.){3}\d{1,3}(?!\d)")
            match = ip_literal.search(text)
            self.assertIsNone(
                match,
                f"IP literal in prose: {match.group(0) if match else ''}",
            )
            for secret_prefix in ("agk" + "_", "sk" + "-",
                                  "ghp" + "_", "xai" + "-", "hf_" ):
                self.assertNotIn(secret_prefix, text)
            # No real domains (placeholders like <data_dir> are fine).
            for tld in (".com", ".cn", ".io", ".net", ".org"):
                self.assertNotIn(tld, text)

    # ── 5. api-routing section + doc comments land ─────────────────────────

    def test_api_routing_carries_the_invariant_section(self):
        marker = "## 11. egress 载荷保护不变量"
        self.assertIn(marker, self.api)
        section = self.api[self.api.index(marker):]
        for ref in (
            "crates/remuda-hub/src/api_relay.rs:130-135",
            "crates/remuda-hub/src/api_relay.rs:426-453",
        ):
            self.assertIn(ref, section)
        # Every code anchor in the section must resolve, exactly like the
        # main doc's anchors; any doc-internal markdown reference must stay
        # in-file.
        resolved, problems = resolve_references(section)
        api_line_count = len(API_ROUTING.read_text(encoding="utf-8").splitlines())
        for _ref, path, start, end in resolved:
            self.assertTrue(path.is_file(), f"{path} does not exist")
            line_count = (api_line_count if path == API_ROUTING
                          else len(path.read_text(encoding="utf-8").splitlines()))
            self.assertGreaterEqual(start, 1)
            self.assertLessEqual(end, line_count)
        self.assertEqual([], problems)

    def test_anchor_files_point_at_the_spec(self):
        # The added paragraph may sit after a pre-existing module header
        # (api_relay.rs), so search the whole file, not just its first lines.
        for path in ANCHOR_FILES:
            content = path.read_text(encoding="utf-8")
            self.assertIn("hub-resilience.md", content,
                          f"{path} not anchored to the spec")

    def test_only_doc_comments_changed_in_anchor_files(self):
        # Every added non-blank line in the four .rs files must be a comment.
        import subprocess
        result = subprocess.run(
            ["git", "diff", "--unified=0", "origin/main", "--",
             *[str(p.relative_to(ROOT)) for p in ANCHOR_FILES]],
            cwd=ROOT, capture_output=True, text=True, check=True,
        )
        for line in result.stdout.splitlines():
            if not line.startswith("+") or line.startswith("+++"):
                continue
            body = line[1:].strip()
            if not body:
                continue
            self.assertTrue(
                body.startswith(("//!", "///", "//")),
                f"non-comment code added: {line!r}",
            )


if __name__ == "__main__":
    unittest.main()
