"""secret-scan.py and the Rust secret lint read one set of shapes.

scripts/ci/secret-shapes.json is the only definition; secret-scan.py loads it,
and crates/remuda-hub/src/secret_lint.rs embeds it. Both test the same corpus,
scripts/ci/secret-shapes-corpus.txt, so a shape that one side flags and the
other passes fails one of the two suites.
"""
import importlib.util
import json
from pathlib import Path
import re
import unittest

CI = Path(__file__).resolve().parents[1] / "ci"


def load_scanner():
    spec = importlib.util.spec_from_file_location("secret_scan", CI / "secret-scan.py")
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


def corpus(names):
    """(expected flagged, text) for every corpus line, needles substituted."""
    cases = []
    for raw in (CI / "secret-shapes-corpus.txt").read_text(encoding="utf-8").splitlines():
        if not raw.strip() or raw.startswith("#"):
            continue
        sign, text = raw[0], raw[2:]
        assert sign in "+-", raw

        def needle(match):
            name = match.group(1)
            assert name in names, f"unknown rule {name!r} in corpus"
            return name

        cases.append((sign == "+", re.sub(r"<<(.+?)>>", needle, text)))
    return cases


class SecretShapes(unittest.TestCase):
    def setUp(self):
        self.scan = load_scanner()
        self.shapes = json.loads((CI / "secret-shapes.json").read_text(encoding="utf-8"))
        self.names = {rule["name"] for rule in self.shapes["rules"]}

    def test_scanner_rules_are_the_shared_file(self):
        self.assertEqual(
            [(name, rx.pattern, minimum) for name, rx, minimum in self.scan.RULES],
            [(r["name"], r["pattern"], r["minPayload"]) for r in self.shapes["rules"]],
        )

    def test_corpus_verdicts(self):
        cases = corpus(self.names)
        self.assertGreater(len(cases), 20)
        for flagged, text in cases:
            with self.subTest(text=text):
                self.assertEqual(bool(self.scan.secret_hits(text)), flagged)

    def test_every_rule_has_a_flagged_line(self):
        hit = set()
        for flagged, text in corpus(self.names):
            if flagged:
                hit |= {name for name, _ in self.scan.secret_hits(text)}
        self.assertEqual(hit, self.names)

    def test_shared_files_pass_their_own_scan(self):
        for name in ("secret-shapes.json", "secret-shapes-corpus.txt"):
            findings = []
            text = (CI / name).read_text(encoding="utf-8")
            self.scan.scan_text(name, text, findings)
            self.assertEqual(findings, [], name)


if __name__ == "__main__":
    unittest.main()
