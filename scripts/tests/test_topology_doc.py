"""Static checks for the Hub identity spec (docs-only batch c-hubidentity).

No network, no subprocess: this test only reads files. It asserts that

(a) every ``path.rs:line`` / ``path.md:line-range`` reference made by the new
    protocol §7.7 section and by evidence/hub-identity-1.md points at a file and
    line range that exist in this working tree (shorthand ``name.rs:line``
    references resolve against the most recently seen full path with that base
    name, exactly as a reader resolves them), and
(b) the spec covers the required security vocabulary: nonce / seq / fail-loud /
    TOFU / 轮转 / 备份 (plus D-035 and ed25519 so the ruling and the algorithm
    cannot be silently dropped).

It deliberately does NOT run a node, a Hub, or any program: the batch is
docs-only and the proof is that the prose's claims stay anchored to real code.
"""
import re
import unittest
from pathlib import Path

ROOT = Path(__file__).resolve().parents[2]
PROTOCOL = ROOT / "docs/design/protocol.md"
EVIDENCE = ROOT / "docs/design/evidence/hub-identity-1.md"
ANCHOR_FILES = [
    ROOT / "crates/remuda-hub/src/store.rs",
    ROOT / "crates/remuda-node/src/transport/hubnode_codec.rs",
    ROOT / "crates/remuda-node/src/origin.rs",
]

# path/name with a tracked source/doc suffix, followed by :line or :line-line
REFERENCE = re.compile(
    r"(?P<ref>[A-Za-z0-9_./-]+\.(?:rs|md|py|toml)):(?P<start>\d+)(?:-(?P<end>\d+))?"
)

REQUIRED_KEYWORDS = ["nonce", "seq", "fail-loud", "TOFU", "轮转", "备份", "D-035", "ed25519"]


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
        cls.evidence = EVIDENCE.read_text(encoding="utf-8")
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
        missing = [word for word in REQUIRED_KEYWORDS if word not in self.section]
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
        for path in ANCHOR_FILES:
            self.assertIn("§7.7", path.read_text(encoding="utf-8"), f"{path} not anchored")

    def test_spec_marks_itself_unimplemented(self):
        self.assertIn("尚未实现", self.section)


if __name__ == "__main__":
    unittest.main()
