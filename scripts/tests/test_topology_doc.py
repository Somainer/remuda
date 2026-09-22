"""Static checks for the docs-only security specs (c-hubidentity, c-nodecosign).

No network, no subprocess: these tests only read files. Two independent
suites share this module:

- ``TopologyDocTests`` (c-hubidentity): every ``path.rs:line`` /
  ``path.md:line-range`` reference made by protocol §7.7 and
  evidence/hub-identity-1.md resolves to a real file/line range (shorthand
  ``name.rs:line`` references resolve against the most recently seen full
  path with that base name), and the Hub identity spec covers its required
  security vocabulary.
- ``NodeCosignDocTests`` (c-nodecosign): the Node device-passkey co-sign
  spec's file:line anchors contain the asserted code, its request list is
  exhaustive, canonicalization/refusal/unattended language is present, and
  the added prose carries no host/home/IP/secret artifacts.

It deliberately does NOT run a node, a Hub, or any program: both batches are
docs-only and the proof is that the prose's claims stay anchored to real code.
"""
import re
import unittest
from pathlib import Path

ROOT = Path(__file__).resolve().parents[2]
DESIGN = ROOT / "docs" / "design"

# c-hubidentity fixtures.
PROTOCOL = DESIGN / "protocol.md"
HUB_IDENTITY_EVIDENCE = DESIGN / "evidence" / "hub-identity-1.md"
HUB_IDENTITY_ANCHOR_FILES = [
    ROOT / "crates/remuda-hub/src/store.rs",
    ROOT / "crates/remuda-node/src/transport/hubnode_codec.rs",
    ROOT / "crates/remuda-node/src/origin.rs",
]

# c-nodecosign fixtures.
PASSKEY = DESIGN / "passkey-login.md"
DECISIONS = DESIGN / "decisions.md"
COSIGN_EVIDENCE = DESIGN / "evidence" / "node-cosign-1.md"

# path/name with a tracked source/doc suffix, followed by :line or :line-line
REFERENCE = re.compile(
    r"(?P<ref>[A-Za-z0-9_./-]+\.(?:rs|md|py|toml)):(?P<start>\d+)(?:-(?P<end>\d+))?"
)

HUB_IDENTITY_REQUIRED_KEYWORDS = [
    "nonce", "seq", "fail-loud", "TOFU", "轮转", "备份", "D-035", "ed25519",
]


def read(relative: str) -> str:
    return (ROOT / relative).read_text(encoding="utf-8")


def line(relative: str, number: int) -> str:
    return read(relative).splitlines()[number - 1]


def extract_section(markdown: str, heading: str) -> str:
    """Return one ### (or ##) section body, from its heading to the next one."""
    lines = markdown.splitlines()
    begin = next((i for i, line in enumerate(lines) if line.startswith(heading)), None)
    if begin is None:
        raise AssertionError(f"missing heading {heading!r} in {PROTOCOL}")
    level = len(lines[begin]) - len(lines[begin].lstrip("#"))
    end = len(lines)
    for i in range(begin + 1, len(lines)):
        stripped = lines[i].lstrip("#")
        if lines[i].startswith("#") and len(lines[i]) - len(stripped) <= level:
            end = i
            break
    return "\n".join(lines[begin:end])


def iter_references(text):
    """Yield (raw_ref, start, end) in document order."""
    for match in REFERENCE.finditer(text):
        # Reject URLs: require the char before the match to not be part of a URL
        # scheme ('https:/...') — full paths start with a known top dir or have
        # no slash at all (shorthand), which this filter enforces loosely.
        ref = match.group("ref")
        if "/" in ref and not ref.startswith(("crates/", "docs/", "scripts/", "deploy/", "web/")):
            continue
        start = int(match.group("start"))
        end = int(match.group("end") or start)
        yield ref, start, end


class TopologyDocTests(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        cls.section = extract_section(PROTOCOL.read_text(encoding="utf-8"), "### 7.7")
        cls.evidence = HUB_IDENTITY_EVIDENCE.read_text(encoding="utf-8")
        # Protocol first: its full-path references teach the resolver the
        # shorthand map before the evidence doc (scanned second) uses bare names.
        cls.text = cls.section + "\n" + cls.evidence

    def test_section_structure_is_complete(self):
        for subheading in ("#### 7.7.1", "#### 7.7.2", "#### 7.7.3", "#### 7.7.4", "#### 7.7.5"):
            self.assertIn(subheading, self.section, f"{subheading} missing")

    def test_every_file_line_reference_exists(self):
        shorthand: dict[str, Path] = {}
        problems = []
        for ref, start, end in iter_references(self.text):
            if "/" in ref:
                path = ROOT / ref
                shorthand[path.name] = path
            else:
                if ref not in shorthand:
                    problems.append(f"shorthand {ref!r} used before any full path")
                    continue
                path = shorthand[ref]
            if not path.is_file():
                problems.append(f"{ref}: file does not exist")
                continue
            line_count = len(path.read_text(encoding="utf-8").splitlines())
            if start < 1 or end < start or end > line_count:
                problems.append(f"{ref}:{start}-{end} outside file (1..{line_count})")
        self.assertEqual([], problems, "\n".join(problems))

    def test_required_keywords_are_covered(self):
        missing = [word for word in HUB_IDENTITY_REQUIRED_KEYWORDS if word not in self.section]
        self.assertEqual([], missing, f"spec missing required keywords: {missing}")

    def test_signature_tuple_is_itemized(self):
        # The four-tuple must appear verbatim and each member must be explained
        # with its own bold lead (逐项理由 lives in §7.7.4).
        self.assertIn("(nonce, hostId, seq, frame_type)", self.section)
        for member in ("nonce", "hostId", "seq", "frame_type"):
            self.assertRegex(self.section, rf"\*\*{member}.*——")

    def test_fail_loud_rules_outline_the_d035_shape(self):
        # Mismatch must be refused, never silently accepted or downgraded.
        self.assertIn("HUB_IDENTITY_MISMATCH", self.section)
        # The doc negates each escape hatch; assert on the contiguous noun
        # phrases so bold markers around 不 do not break the check.
        for forbidden_shape in ("覆盖旧 pin", "只信 TLS", "自动重新 TOFU"):
            self.assertIn(forbidden_shape, self.section)

    def test_non_goals_keep_wire_schema_and_deps_untouched(self):
        self.assertIn("不改 wire 版本", self.section)
        self.assertIn("不动 Hub store schema", self.section)
        self.assertIn("不引入新依赖", self.section)

    def test_doc_comment_anchors_point_at_the_spec(self):
        for path in HUB_IDENTITY_ANCHOR_FILES:
            self.assertIn("§7.7", path.read_text(encoding="utf-8"), f"{path} not anchored")

    def test_spec_marks_itself_unimplemented(self):
        self.assertIn("尚未实现", self.section)


class NodeCosignDocTests(unittest.TestCase):
    """c-nodecosign: the co-sign spec exists, is complete, and its anchors hold."""

    @classmethod
    def setUpClass(cls):
        cls.spec = extract_section(PASSKEY.read_text(encoding="utf-8"), "## 6.")
        cls.evidence = COSIGN_EVIDENCE.read_text(encoding="utf-8")
        cls.decisions = DECISIONS.read_text(encoding="utf-8")

    def assertContains(self, haystack, needle, where):
        self.assertIn(needle, haystack, f"{needle!r} missing from {where}")

    def test_node_cosign_spec_section_and_evidence_exist(self):
        self.assertContains(self.spec, "Node 侧 device passkey 联署", "passkey-login §6")
        self.assertContains(self.spec, "未实现", "passkey-login §6 status")
        self.assertContains(self.evidence, "全 fleet", "evidence attack chain")
        self.assertContains(self.evidence, "远程代码执行", "evidence attack chain")

    def test_node_cosign_request_list_is_exhaustive(self):
        # Every privileged surface the task named must appear, and each of the
        # eight rows carries a reason column.
        for token in [
            "instance.create", "instance.resume", "gate.run", "gate.then",
            "gate.land", "gate.cancel", "gate.unpin",
            "bypassPermissions", "bypass-permissions",
            "--dangerously-skip-permissions",
            "--dangerously-bypass-approvals-and-sandbox",
            "danger-full-access", "capabilities", "computer-use",
            "binaryPath", "`args`",
        ]:
            self.assertContains(self.spec, token, "co-sign request list")
        # The eight required rows and the reviewed-and-excluded table.
        for row in ["C1", "C2", "C3", "C4", "C5", "C6", "C7", "C8"]:
            self.assertContains(self.spec, f"| {row} |", f"co-sign row {row}")
        self.assertContains(self.spec, "审阅过但**不**要求联署", "exclusion table")
        for excluded in ["worker.provision", "tty.write", "instance.purge"]:
            self.assertContains(self.spec, excluded, "exclusion table")
        # The list is closed: no "etc." hedge inside the spec.
        self.assertNotIn("等等", self.spec)

    def test_node_cosign_canonicalization_is_exact(self):
        for token in [
            "RFC 8785", "JCS", "UTF-16", "SHA-256", "base64url",
            "nonce", '"aud"', '"kid"', '"params"', '"v": 1',
            "重复键", "浮点数", "成员", "排序",
            "agentCredential",  # the single excluded key is named
            "bundle", "members",  # fleet one-tap shape
            "clientDataJSON", "UV=1", "ES256",
        ]:
            self.assertContains(self.spec, token, "canonicalization section")
        # 120 s interactive window and the audience binding must be explicit.
        self.assertContains(self.spec, "120", "challenge TTL")
        self.assertContains(self.spec, "hostId", "audience")

    def test_node_cosign_verification_refuses_before_dispatch_and_cites_d035(self):
        for token in [
            "D-035", "refuse-never-reroute", "不降级", "拒绝",
            "持久接受", "nonce", "重放", "fail closed",
        ]:
            self.assertContains(self.spec, token, "verification/failure section")
        # All three carrier entry points are named as one shared verifier.
        for anchor in [
            "server.rs:838-846",
            "runtime_link.rs:29-38",
            "runtime_wss.rs",
        ]:
            self.assertContains(self.spec, anchor, "carrier entry points")

    def test_node_cosign_unattended_conflicts_are_recorded_honestly(self):
        for token in [
            "联署券", "coupon", "无人值守", "nMax", "audiences",
            "headless", "等待设备", "移除",
        ]:
            self.assertContains(self.spec, token, "usability section")
        # The hard bans that make the trade-off real must be stated.
        self.assertContains(self.spec, "不可兼得", "land conflict")
        self.assertContains(self.spec, "只接受逐次交互式联署", "CUA coupon ban")
        self.assertContains(self.decisions,
                            "不接受预先签名的无人值守联署券", "D-045 addendum")
        # Round-1 review: unattended is NOT an existing product concept; the
        # section must say so and cite protocol.md:222 (launchedBy provenance).
        flattened = re.sub(r"\s+", "", self.spec)
        self.assertContains(flattened, "没有「无人值守」这个概念",
                            "unattended status annotation")
        self.assertContains(flattened, "第222行", "protocol.md:222 citation")
        for token in ["launchedBy", "尚未拍板", "不表示能力等级"]:
            self.assertContains(self.spec, token, "unattended status annotation")

    def test_node_cosign_explicit_non_goals(self):
        for token in [
            "明确不做", "不实现", "不改 wire", "docs-only",
            "不做前门", "c-hubidentity",
        ]:
            self.assertContains(self.spec, token, "non-goals section")

    def test_node_cosign_decisions_d045_addendum_is_appended_not_edited(self):
        marker = "**附记（2026-09-22，c-nodecosign"
        self.assertContains(self.decisions, marker, "decisions.md D-045 addendum")
        addendum = self.decisions.index(marker)
        d046 = self.decisions.index("## D-046")
        self.assertLess(addendum, d046, "addendum must stay inside the D-045 section")
        self.assertRegex(
            self.decisions[addendum:d046],
            re.compile(r"passkey-login\.md.*§6", re.S),
        )

    # ── file:line anchors cited by the spec and evidence ──────────────────

    def assert_line(self, relative, number, fragment):
        text = line(relative, number)
        self.assertIn(fragment, text, f"{relative}:{number} = {text!r}")

    def test_node_cosign_origin_anchor_is_still_wire_origin(self):
        # The attack-chain anchor: Node copies origin straight from params.
        # Post-c-hubidentity merge, the §7.7 doc comment occupies lines 25-38
        # and wire_origin moved to 39; keep both sides of the argument pinned.
        self.assert_line("crates/remuda-node/src/origin.rs", 39, "fn wire_origin")
        self.assert_line("crates/remuda-node/src/origin.rs", 42,
                         'params.get("origin")')
        self.assert_line("crates/remuda-node/src/origin.rs", 85,
                         "Never serialized into a request digest")

    def test_node_cosign_hub_trust_anchor_is_still_line_284(self):
        self.assert_line("crates/remuda-hub/src/agent_scope.rs", 284,
                         "InputOrigin::Human")
        self.assert_line("crates/remuda-hub/src/agent_scope.rs", 51,
                         "fn stamp")
        self.assert_line("crates/remuda-hub/src/agent_scope.rs", 302,
                         "fn restrict_launch_overrides")

    def test_node_cosign_carrier_entry_points(self):
        self.assert_line("crates/remuda-node/src/server.rs", 846,
                         "async fn dispatch_rpc")
        self.assert_line("crates/remuda-node/src/server.rs", 932,
                         '"instance.create"')
        self.assert_line("crates/remuda-node/src/server.rs", 955,
                         '"instance.resume"')
        self.assert_line("crates/remuda-node/src/server.rs", 888,
                         "is_gate_method")
        self.assert_line("crates/remuda-node/src/runtime_link.rs", 38,
                         '"instance.create"')
        self.assert_line("crates/remuda-node/src/transport/wss/runtime_wss.rs",
                         194, "create_from_params")
        self.assert_line("crates/remuda-node/src/transport/wss/runtime_wss.rs",
                         210, "create_from_params")

    def test_node_cosign_privilege_field_anchors(self):
        self.assert_line("crates/remuda-node/src/model.rs", 97,
                         "permission_mode: String")
        self.assert_line("crates/remuda-node/src/model.rs", 152,
                         "capabilities explicitly granted")
        self.assert_line("crates/remuda-node/src/native.rs", 712,
                         "fn request_is_bypass")
        self.assert_line("crates/remuda-node/src/native.rs", 715,
                         '"bypassPermissions"')
        self.assert_line("crates/remuda-node/src/native.rs", 1456,
                         "danger-full-access")
        self.assert_line("crates/remuda-driver/src/presets.rs", 151,
                         "fn merge_yolo_argv")
        self.assert_line("crates/remuda-driver/src/presets.rs", 57,
                         "--dangerously-skip-permissions")
        self.assert_line("crates/remuda-driver/src/launch/skills.rs", 139,
                         "LaunchOrigin::Agent")
        self.assert_line("crates/remuda-node/src/computer_use.rs", 36,
                         "fn host_preflight")

    def test_node_cosign_gate_anchors(self):
        self.assert_line("crates/remuda-protocol/src/gate.rs", 16,
                         'METHOD_GATE_RUN: &str = "gate.run"')
        self.assert_line("crates/remuda-protocol/src/gate.rs", 412,
                         "`gate.run` params")
        self.assert_line("crates/remuda-protocol/src/gate.rs", 531,
                         "bash -lc")
        self.assert_line("crates/remuda-node/src/gate.rs", 552,
                         "METHOD_GATE_RUN => self.run_gate")
        self.assert_line("crates/remuda-node/src/gate.rs", 564, "fn run_gate")
        self.assert_line("crates/remuda-node/src/gate.rs", 590,
                         "fn run_gate_then")
        self.assert_line("crates/remuda-node/src/gate.rs", 600,
                         "fn run_gate_land")

    def test_node_cosign_wire_and_channel_anchors(self):
        self.assert_line("crates/remuda-protocol/src/hubnode.rs", 43,
                         'METHOD_INSTANCE_CREATE: &str = "instance.create"')
        self.assert_line("crates/remuda-hub/src/fleet.rs", 18,
                         "/v1/fleet/instances")
        self.assert_line("crates/remuda-node/src/transport/wss.rs", 55,
                         "Bootstrap token")
        self.assert_line("crates/remuda-node/src/enroll.rs", 44,
                         "Host token from Hub hello")

    def test_node_cosign_doc_comments_point_at_spec(self):
        # The only .rs edits c-nodecosign made: doc comments must carry the
        # spec pointer (reviewed via git diff --stat for executable lines).
        origin_head = "\n".join(read("crates/remuda-node/src/origin.rs")
                                .splitlines()[:3])
        scope_head = "\n".join(read("crates/remuda-hub/src/agent_scope.rs")
                               .splitlines()[:3])
        for head in (origin_head, scope_head):
            self.assertIn("passkey-login.md", head)
            self.assertIn("§6", head)
            self.assertIn("SPEC-ONLY", head)

    def test_node_cosign_new_docs_leak_no_host_or_user_artifacts(self):
        # Rule for committed files: no hostnames/domains, IP literals, home
        # directories, or token-shaped secrets in the prose we added.
        spec_start = PASSKEY.read_text(encoding="utf-8").index("## 6.")
        docs = [
            PASSKEY.read_text(encoding="utf-8")[spec_start:],
            self.evidence,
            self.decisions[self.decisions.index("**附记（2026-09-22"):
                           self.decisions.index("## D-046")],
        ]
        home_marker = "/" + "home/"
        for text in docs:
            self.assertNotIn(home_marker, text)
        ip_literal = re.compile(r"(?<!\d)(?:\d{1,3}\.){3}\d{1,3}(?!\d)")
        for text in docs + [Path(__file__).read_text(encoding="utf-8")]:
            for secret_prefix in ("agk" + "_", "sk" + "-",
                                  "ghp" + "_", "xai" + "-"):
                self.assertNotIn(secret_prefix, text)
            match = ip_literal.search(text)
            self.assertIsNone(
                match,
                "IP literal in added prose: %s" % (match.group(0) if match else ""),
            )


if __name__ == "__main__":
    unittest.main()
