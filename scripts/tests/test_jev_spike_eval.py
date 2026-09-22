"""Offline evaluation for the c-jevspike gate-failure triage spike.

Three concerns, all stdlib-only and all network-free:

(a) sanitization -- the synthetic corpus must never carry /home/, /Users/,
    dotted IPv4 addresses, or email forms;
(b) rule baseline -- the deterministic plain-rule triager
    (fmt/clippy/compile error -> regression, known flaky roster -> flake,
    everything else -> unknown) scored per gate category;
(c) offline go/no-go judge -- given a fixture of Jev-shaped choice answers,
    evaluate the four thresholds from briefs/c-jevspike.md:

    1. regression precision >= 0.95 at the skip-retry operating point;
    2. regression coverage at that point exceeds the rule baseline by >= 15pp;
    3. p95 end-to-end latency <= 900ms;
    4. first/last calibration buckets deviate from their midpoints by <= 0.15.

CLI:

    python3 scripts/tests/test_jev_spike_eval.py --baseline
    python3 scripts/tests/test_jev_spike_eval.py --judge \
        scripts/tests/fixtures/jev-spike/responses.fixture.json

With no arguments the unittest suite runs.
"""

from __future__ import annotations

import argparse
import importlib.util
import json
import math
import re
import sys
import tempfile
import unittest
from pathlib import Path

FIX_DIR = Path(__file__).resolve().parent / "fixtures" / "jev-spike"
CORPUS_PATH = FIX_DIR / "corpus.jsonl"
MANIFEST_PATH = FIX_DIR / "manifest.json"
PASS_PATH = FIX_DIR / "responses.fixture.json"
FAIL_PATH = FIX_DIR / "responses.fail.fixture.json"
GENERATOR_PATH = FIX_DIR / "generate_corpus.py"

QUESTION_KEY = "triage"

PRECISION_MIN = 0.95
COVERAGE_GAIN_MIN = 0.15
P95_MAX_MS = 900
CAL_MAX_DEVIATION = 0.15
# Half-open probability buckets; the last edge runs past 1.0 so p == 1.0
# lands in the top bucket.
CAL_BUCKETS = ((0.0, 0.2, 0.1), (0.8, 1.0001, 0.9))

EMAIL_RE = re.compile(r"[A-Za-z0-9._%+-]+@[A-Za-z0-9.-]+\.[A-Za-z]{2,}")
IPV4_RE = re.compile(r"(?<!\d)(?:\d{1,3}\.){3}\d{1,3}(?!\d)")
HOME_RE = re.compile(r"/home/|/Users/")
ERRORCODE_RE = re.compile(r"error\[e\d{4}\]")

CATEGORIES = ("cargo-test", "clippy", "fmt", "vitest", "playwright", "infra")
GOLD_LABELS = ("regression", "flake")


# ---------------------------------------------------------------------------
# Loading
# ---------------------------------------------------------------------------

def load_corpus(path: Path = CORPUS_PATH) -> list[dict]:
    records = []
    with path.open(encoding="utf-8") as handle:
        for line in handle:
            line = line.strip()
            if line:
                records.append(json.loads(line))
    return records


def load_manifest(path: Path = MANIFEST_PATH) -> dict:
    return json.loads(path.read_text(encoding="utf-8"))


def load_response_envelope(path: Path) -> dict:
    envelope = json.loads(Path(path).read_text(encoding="utf-8"))
    if "responses" not in envelope:
        raise ValueError(f"{path}: missing 'responses' array")
    return envelope


# ---------------------------------------------------------------------------
# (b) Deterministic rule baseline
# ---------------------------------------------------------------------------

def rule_baseline(record: dict, flaky_roster: list[str]) -> str:
    """Plain-rule triager. Returns 'regression', 'flake', or 'unknown'.

    The rule abstains ('unknown') whenever it lacks a deterministic signal;
    abstention is what keeps precision at 1.0 and is exactly the behavior a
    model triager would have to beat on coverage.
    """
    log = record["log"]
    if any(name in log for name in flaky_roster):
        return "flake"
    text = log.lower()
    if (
        "contain formatting differences" in text
        or "could not compile" in text
        or ERRORCODE_RE.search(text)
    ):
        return "regression"
    return "unknown"


def baseline_table(records: list[dict], flaky_roster: list[str]) -> dict:
    """Confusion counts + rates, overall and per reporting category."""
    table = {}
    for category in CATEGORIES:
        table[category] = {
            "n": 0, "gold_regression": 0, "gold_flake": 0,
            "pred_regression": 0, "pred_flake": 0, "pred_unknown": 0,
            "regression_tp": 0, "regression_fp": 0,
            "flake_tp": 0, "flake_fp": 0,
        }
    for record in records:
        row = table[record["category"]]
        row["n"] += 1
        row[f"gold_{record['gold']}"] += 1
        pred = rule_baseline(record, flaky_roster)
        row[f"pred_{pred}"] += 1
        if pred == "regression":
            if record["gold"] == "regression":
                row["regression_tp"] += 1
            else:
                row["regression_fp"] += 1
        elif pred == "flake":
            if record["gold"] == "flake":
                row["flake_tp"] += 1
            else:
                row["flake_fp"] += 1

    def rates(row, kind, gold_total) -> dict:
        tp, fp = row[f"{kind}_tp"], row[f"{kind}_fp"]
        return {
            "predicted": tp + fp,
            "precision": tp / (tp + fp) if tp + fp else None,
            "coverage": tp / gold_total if gold_total else None,
        }

    for category, row in table.items():
        row["regression"] = rates(row, "regression", row["gold_regression"])
        row["flake"] = rates(row, "flake", row["gold_flake"])

    total = {
        "n": sum(r["n"] for r in table.values()),
        "gold_regression": sum(r["gold_regression"] for r in table.values()),
        "gold_flake": sum(r["gold_flake"] for r in table.values()),
        "pred_regression": sum(r["pred_regression"] for r in table.values()),
        "pred_flake": sum(r["pred_flake"] for r in table.values()),
        "pred_unknown": sum(r["pred_unknown"] for r in table.values()),
        "regression_tp": sum(r["regression_tp"] for r in table.values()),
        "regression_fp": sum(r["regression_fp"] for r in table.values()),
        "flake_tp": sum(r["flake_tp"] for r in table.values()),
        "flake_fp": sum(r["flake_fp"] for r in table.values()),
    }
    tp, fp = total["regression_tp"], total["regression_fp"]
    total["regression_precision"] = tp / (tp + fp) if tp + fp else None
    total["regression_coverage"] = tp / total["gold_regression"]
    tp, fp = total["flake_tp"], total["flake_fp"]
    total["flake_precision"] = tp / (tp + fp) if tp + fp else None
    total["flake_coverage"] = tp / total["gold_flake"]
    table["__all__"] = total
    return table


# ---------------------------------------------------------------------------
# (c) Offline go/no-go judge
# ---------------------------------------------------------------------------

def _answer(response: dict) -> dict:
    try:
        answer = response["answers"][QUESTION_KEY]
    except (KeyError, TypeError) as exc:
        raise ValueError(
            f"response {response.get('id')!r}: missing answers[{QUESTION_KEY!r}]"
        ) from exc
    for key in ("choice", "probabilities", "confidence"):
        if key not in answer:
            raise ValueError(
                f"response {response.get('id')!r}: answer missing {key!r}")
    probs = answer["probabilities"]
    if "regression" not in probs or "flake" not in probs:
        raise ValueError(
            f"response {response.get('id')!r}: probabilities must name "
            "'regression' and 'flake'")
    return answer


def _pairs(envelope: dict, records: list[dict]) -> list[tuple]:
    """Join response probabilities to gold labels. Returns (gold, p, ms)."""
    gold_by_id = {record["id"]: record["gold"] for record in records}
    seen = set()
    pairs = []
    for response in envelope["responses"]:
        rid = response.get("id")
        if rid not in gold_by_id:
            raise ValueError(f"response id {rid!r} not present in corpus")
        if rid in seen:
            raise ValueError(f"duplicate response id {rid!r}")
        seen.add(rid)
        answer = _answer(response)
        p_reg = float(answer["probabilities"]["regression"])
        if not 0.0 <= p_reg <= 1.0:
            raise ValueError(f"response {rid!r}: probability out of range")
        latency = response.get("latency_ms")
        if not isinstance(latency, (int, float)) or latency < 0:
            raise ValueError(f"response {rid!r}: bad latency_ms")
        pairs.append((gold_by_id[rid], p_reg, float(latency)))
    if seen != gold_by_id.keys():
        missing = sorted(gold_by_id.keys() - seen)
        raise ValueError(f"fixture is missing responses for {len(missing)} ids, "
                         f"first: {missing[:3]}")
    return pairs


def _operating_point(pairs: list[tuple]) -> dict | None:
    """Highest-coverage threshold whose regression precision is >= 0.95.

    Candidates are a 0.001 grid; ties prefer higher precision, then a lower
    threshold (more conservative about the boundary).
    """
    n_reg = sum(1 for gold, _, _ in pairs if gold == "regression")
    best = None
    for k in range(500, 1000):
        threshold = k / 1000
        predicted = [(gold, p) for gold, p, _ in pairs if p >= threshold]
        if not predicted:
            continue
        tp = sum(1 for gold, _ in predicted if gold == "regression")
        precision = tp / len(predicted)
        if precision + 1e-12 < PRECISION_MIN:
            continue
        coverage = tp / n_reg
        candidate = (coverage, precision, -threshold, threshold,
                     len(predicted), tp)
        if best is None or candidate[:3] > best[:3]:
            best = candidate
    if best is None:
        return None
    coverage, precision, _, threshold, n_pred, tp = best
    return {"threshold": threshold, "precision": precision,
            "coverage": coverage, "predicted": n_pred, "tp": tp}


def _percentile(values: list[float], q: float) -> float:
    """Nearest-rank percentile (q in [0,1]) over an already-sorted list."""
    return values[math.ceil(q * len(values)) - 1]


def _calibration(pairs: list[tuple]) -> list[dict]:
    rows = []
    for lo, hi, midpoint in CAL_BUCKETS:
        bucket = [(gold, p) for gold, p, _ in pairs if lo <= p < hi]
        if not bucket:
            rows.append({"bucket": [lo, hi], "n": 0, "empirical": None,
                         "midpoint": midpoint, "deviation": None,
                         "supported": False})
            continue
        empirical = sum(1 for gold, _ in bucket if gold == "regression") / len(bucket)
        rows.append({"bucket": [lo, hi], "n": len(bucket),
                     "empirical": empirical, "midpoint": midpoint,
                     "deviation": abs(empirical - midpoint),
                     "supported": True})
    return rows


def judge(envelope: dict, records: list[dict],
          rule_regression_coverage: float) -> dict:
    pairs = _pairs(envelope, records)
    n_reg = sum(1 for gold, _, _ in pairs if gold == "regression")
    point = _operating_point(pairs)

    criteria = {}
    if point is None:
        criteria["1_regression_precision"] = {
            "pass": False,
            "detail": "no threshold predicts any regression at precision "
                      f">= {PRECISION_MIN:.2f}",
        }
        criteria["2_coverage_gain"] = {
            "pass": False,
            "detail": "no precision-compliant operating point exists, so a "
                      "coverage gain cannot be claimed",
        }
    else:
        gain = point["coverage"] - rule_regression_coverage
        criteria["1_regression_precision"] = {
            "pass": point["precision"] + 1e-12 >= PRECISION_MIN,
            "detail": (f"threshold {point['threshold']:.3f}: precision "
                       f"{point['precision']:.4f} on {point['predicted']} "
                       f"predicted regressions (>= {PRECISION_MIN:.2f})"),
        }
        criteria["2_coverage_gain"] = {
            "pass": gain + 1e-12 >= COVERAGE_GAIN_MIN,
            "detail": (f"model coverage {point['coverage']:.4f} vs rule "
                       f"baseline {rule_regression_coverage:.4f}: gain "
                       f"{gain:+.4f} (>= +{COVERAGE_GAIN_MIN:.2f} required)"),
        }

    latencies = sorted(latency for _, _, latency in pairs)
    p95 = _percentile(latencies, 0.95)
    criteria["3_p95_latency"] = {
        "pass": p95 <= P95_MAX_MS,
        "detail": f"p95 latency {p95:.0f}ms (<= {P95_MAX_MS}ms)",
    }

    calibration = _calibration(pairs)
    cal_pass = all(
        row["supported"] and row["deviation"] <= CAL_MAX_DEVIATION + 1e-12
        for row in calibration
    )
    detail_parts = []
    for row in calibration:
        if not row["supported"]:
            detail_parts.append(f"{row['bucket'][0]:g}-{row['bucket'][1]:g}: empty")
        else:
            detail_parts.append(
                f"{row['bucket'][0]:g}-{row['bucket'][1]:g}: "
                f"empirical {row['empirical']:.3f} vs midpoint "
                f"{row['midpoint']:g}, |dev| {row['deviation']:.3f}")
    criteria["4_calibration_tails"] = {
        "pass": cal_pass,
        "detail": "; ".join(detail_parts)
        + f" (both tails <= {CAL_MAX_DEVIATION:.2f}, empty bucket fails)",
    }

    verdict = "go" if all(item["pass"] for item in criteria.values()) else "no-go"
    return {
        "n": len(pairs),
        "n_regression": n_reg,
        "n_flake": len(pairs) - n_reg,
        "operating_point": point,
        "rule_regression_coverage": rule_regression_coverage,
        "p95_latency_ms": p95,
        "calibration": calibration,
        "criteria": criteria,
        "verdict": verdict,
    }


# ---------------------------------------------------------------------------
# Reporting
# ---------------------------------------------------------------------------

def _fmt_rate(value) -> str:
    return "  -  " if value is None else f"{value:.3f}"


def format_baseline(table: dict) -> str:
    lines = [
        "Deterministic rule baseline per category "
        "(R=regression, F=flake, U=unknown)",
        f"{'category':<12} {'n':>4} {'gR':>3} {'gF':>3} {'pR':>3} "
        f"{'pF':>3} {'pU':>3} {'R prec':>7} {'R cov':>7} {'F prec':>7}",
    ]
    for category in CATEGORIES:
        row = table[category]
        lines.append(
            f"{category:<12} {row['n']:>4} {row['gold_regression']:>3} "
            f"{row['gold_flake']:>3} {row['pred_regression']:>3} "
            f"{row['pred_flake']:>3} {row['pred_unknown']:>3} "
            f"{_fmt_rate(row['regression']['precision']):>7} "
            f"{_fmt_rate(row['regression']['coverage']):>7} "
            f"{_fmt_rate(row['flake']['precision']):>7}")
    total = table["__all__"]
    lines.append(
        f"{'ALL':<12} {total['n']:>4} {total['gold_regression']:>3} "
        f"{total['gold_flake']:>3} {total['pred_regression']:>3} "
        f"{total['pred_flake']:>3} {total['pred_unknown']:>3} "
        f"{total['regression_precision']:>7.3f} "
        f"{total['regression_coverage']:>7.3f} "
        f"{total['flake_precision']:>7.3f}")
    return "\n".join(lines)


def format_judgment(report: dict, source: Path) -> str:
    lines = [f"Offline judgment of {source}",
             f"records: {report['n']} "
             f"({report['n_regression']} regression / {report['n_flake']} flake)"]
    point = report["operating_point"]
    if point is None:
        lines.append("operating point: none reaches the precision floor")
    else:
        lines.append(
            f"operating point: p(regression) >= {point['threshold']:.3f} -> "
            f"{point['predicted']} predictions, precision "
            f"{point['precision']:.4f}, coverage {point['coverage']:.4f}")
    lines.append(f"p95 latency: {report['p95_latency_ms']:.0f}ms")
    for row in report["calibration"]:
        if row["supported"]:
            lines.append(
                f"calibration {row['bucket'][0]:g}-{row['bucket'][1]:g}: "
                f"n={row['n']} empirical={row['empirical']:.3f} "
                f"dev={row['deviation']:.3f}")
        else:
            lines.append(
                f"calibration {row['bucket'][0]:g}-{row['bucket'][1]:g}: empty")
    for name, criterion in report["criteria"].items():
        mark = "PASS" if criterion["pass"] else "FAIL"
        lines.append(f"[{mark}] {name}: {criterion['detail']}")
    lines.append(f"VERDICT: {report['verdict'].upper()}")
    return "\n".join(lines)


# ---------------------------------------------------------------------------
# Tests
# ---------------------------------------------------------------------------

class CorpusTests(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        cls.records = load_corpus()
        cls.manifest = load_manifest()

    def test_counts_and_schema(self):
        self.assertGreaterEqual(len(self.records), 200)
        self.assertEqual(len(self.records), self.manifest["total"])
        ids = [record["id"] for record in self.records]
        self.assertEqual(len(ids), len(set(ids)))
        self.assertEqual(ids, [f"c{i:04d}" for i in range(1, len(ids) + 1)])
        for record in self.records:
            self.assertIn(record["category"], CATEGORIES)
            self.assertIn(record["gold"], GOLD_LABELS)
            self.assertTrue(record["log"].strip())
            self.assertTrue(record["signature"])

    def test_manifest_counts_and_family_gold(self):
        counts = self.manifest["counts_by_category"]
        for category in CATEGORIES:
            gold_r = sum(1 for r in self.records
                         if r["category"] == category and r["gold"] == "regression")
            gold_f = sum(1 for r in self.records
                         if r["category"] == category and r["gold"] == "flake")
            self.assertEqual(counts[category]["regression"], gold_r)
            self.assertEqual(counts[category]["flake"], gold_f)
            self.assertEqual(counts[category]["total"], gold_r + gold_f)
        family_gold = {family["signature"]: family["gold"]
                       for family in self.manifest["families"]}
        for record in self.records:
            self.assertEqual(family_gold[record["signature"]], record["gold"],
                             msg=record["id"])

    def test_no_home_paths_ipv4_or_email(self):
        offenders = []
        for record in self.records:
            blob = json.dumps(record, ensure_ascii=True)
            if HOME_RE.search(blob):
                offenders.append((record["id"], "home path"))
            if IPV4_RE.search(blob):
                offenders.append((record["id"], "ipv4"))
            if EMAIL_RE.search(blob):
                offenders.append((record["id"], "email"))
        self.assertEqual(offenders, [])


class BaselineTests(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        cls.records = load_corpus()
        cls.roster = load_manifest()["known_flaky_roster"]
        cls.table = baseline_table(cls.records, cls.roster)

    def test_locked_overall_numbers(self):
        total = self.table["__all__"]
        self.assertEqual(total["n"], 204)
        self.assertEqual((total["gold_regression"], total["gold_flake"]),
                         (126, 78))
        self.assertEqual(total["regression_tp"], 48)
        self.assertEqual(total["regression_fp"], 0)
        self.assertEqual(total["flake_tp"], 25)
        self.assertEqual(total["flake_fp"], 0)
        self.assertEqual(total["pred_unknown"], 131)
        self.assertEqual(total["regression_precision"], 1.0)
        self.assertAlmostEqual(total["regression_coverage"], 48 / 126, places=12)
        self.assertEqual(total["flake_precision"], 1.0)
        self.assertAlmostEqual(total["flake_coverage"], 25 / 78, places=12)

    def test_locked_per_category_confusions(self):
        expected = {
            # category: (pred_regression, pred_flake, pred_unknown)
            "cargo-test": (6, 12, 30),
            "clippy": (24, 0, 4),
            "fmt": (18, 0, 2),
            "vitest": (0, 4, 32),
            "playwright": (0, 9, 35),
            "infra": (0, 0, 28),
        }
        for category, (pr, pf, pu) in expected.items():
            row = self.table[category]
            self.assertEqual((row["pred_regression"], row["pred_flake"],
                              row["pred_unknown"]), (pr, pf, pu),
                             msg=category)


class JudgeTests(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        cls.records = load_corpus()
        cls.roster = load_manifest()["known_flaky_roster"]
        cls.rule_coverage = baseline_table(
            cls.records, cls.roster)["__all__"]["regression_coverage"]

    def _judge(self, path):
        return judge(load_response_envelope(path), self.records,
                     self.rule_coverage)

    def test_pass_fixture_is_go(self):
        report = self._judge(PASS_PATH)
        self.assertEqual(report["verdict"], "go")
        for name, criterion in report["criteria"].items():
            self.assertTrue(criterion["pass"], msg=f"{name}: {criterion}")
        point = report["operating_point"]
        self.assertEqual(point["threshold"], 0.678)
        self.assertAlmostEqual(point["precision"], 116 / 122, places=12)
        self.assertAlmostEqual(point["coverage"], 116 / 126, places=12)
        self.assertEqual(report["p95_latency_ms"], 675)
        tails = report["calibration"]
        self.assertEqual(tails[0]["n"], 42)
        self.assertEqual(tails[0]["empirical"], 0.0)
        self.assertEqual(tails[1]["n"], 104)
        self.assertAlmostEqual(tails[1]["empirical"], 103 / 104, places=12)

    def test_fail_fixture_is_no_go(self):
        report = self._judge(FAIL_PATH)
        self.assertEqual(report["verdict"], "no-go")
        criteria = report["criteria"]
        # The negative control is constructed to break precision, coverage
        # gain, and p95 latency; assert at least those three are red.
        self.assertFalse(criteria["1_regression_precision"]["pass"])
        self.assertFalse(criteria["2_coverage_gain"]["pass"])
        self.assertFalse(criteria["3_p95_latency"]["pass"])
        self.assertIsNone(report["operating_point"])
        self.assertGreater(report["p95_latency_ms"], P95_MAX_MS)

    def test_missing_id_is_rejected(self):
        envelope = load_response_envelope(PASS_PATH)
        envelope["responses"] = envelope["responses"][:-1]
        with self.assertRaises(ValueError):
            judge(envelope, self.records, self.rule_coverage)


class RegenerationTests(unittest.TestCase):
    """A second person must regenerate byte-identical offline artifacts."""

    @classmethod
    def setUpClass(cls):
        sys.dont_write_bytecode = True
        spec = importlib.util.spec_from_file_location("jev_corpus_generator",
                                                      GENERATOR_PATH)
        cls.generator = importlib.util.module_from_spec(spec)
        spec.loader.exec_module(cls.generator)

    def _sha256(self, text: str) -> str:
        import hashlib
        return hashlib.sha256(text.encode("utf-8")).hexdigest()

    def test_artifacts_regenerate_byte_identical(self):
        with tempfile.TemporaryDirectory() as tmp:
            out = Path(tmp)
            self.generator.write_all(out)
            for name in ("corpus.jsonl", "responses.fixture.json",
                         "responses.fail.fixture.json", "manifest.json"):
                self.assertEqual(
                    (out / name).read_text(encoding="utf-8"),
                    (FIX_DIR / name).read_text(encoding="utf-8"),
                    msg=f"{name} is not reproducible from the generator")

    def test_manifest_hashes_match_artifacts(self):
        manifest = load_manifest()
        corpus_text = CORPUS_PATH.read_text(encoding="utf-8")
        pass_text = PASS_PATH.read_text(encoding="utf-8")
        fail_text = FAIL_PATH.read_text(encoding="utf-8")
        self.assertEqual(manifest["corpus_sha256"], self._sha256(corpus_text))
        self.assertEqual(manifest["responses_fixture_sha256"],
                         self._sha256(pass_text))
        self.assertEqual(manifest["responses_fail_fixture_sha256"],
                         self._sha256(fail_text))


# ---------------------------------------------------------------------------
# CLI
# ---------------------------------------------------------------------------

def _print_baseline() -> int:
    records = load_corpus()
    roster = load_manifest()["known_flaky_roster"]
    print(format_baseline(baseline_table(records, roster)))
    return 0


def _print_judgment(path: Path) -> int:
    records = load_corpus()
    roster = load_manifest()["known_flaky_roster"]
    rule_coverage = baseline_table(records, roster)["__all__"][
        "regression_coverage"]
    envelope = load_response_envelope(path)
    report = judge(envelope, records, rule_coverage)
    print(format_judgment(report, path))
    return 0 if report["verdict"] == "go" else 1


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    parser.add_argument("--baseline", action="store_true",
                        help="print the rule baseline metrics table")
    parser.add_argument("--judge", metavar="RESPONSES_JSON", type=Path,
                        help="evaluate a Jev response fixture (go/no-go)")
    args = parser.parse_args(argv)
    if args.baseline:
        return _print_baseline()
    if args.judge is not None:
        return _print_judgment(args.judge)
    unittest.main(argv=[sys.argv[0]], verbosity=2)
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
