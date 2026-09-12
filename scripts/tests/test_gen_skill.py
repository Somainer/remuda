"""Synthetic skill assembly fixture; no network or Rust build required."""
from pathlib import Path
import shutil
import subprocess
import tempfile
import unittest

SOURCE = Path(__file__).resolve().parents[1] / "gen-skill.sh"


class SkillAssembly(unittest.TestCase):
    def test_sections_are_ordered_and_check_detects_a_stale_copy(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            script = root / "scripts/gen-skill.sh"
            script.parent.mkdir()
            shutil.copy2(SOURCE, script)
            sections = root / "skills/remuda/sections"
            sections.mkdir(parents=True)
            (sections / "00-intro.md").write_text("---\nname: remuda\n---\n\n# Remuda\n")
            (sections / "20-second.md").write_text("## Second\n")
            (sections / "10-first.md").write_text("## First\n")
            run = lambda *args: subprocess.run([str(script), *args], cwd=temporary,
                                               capture_output=True, text=True)
            self.assertNotEqual(run("--check").returncode, 0)
            self.assertEqual(run().returncode, 0)
            target = root / "skills/remuda/SKILL.md"
            self.assertEqual(target.read_text(), "---\nname: remuda\n---\n\n# Remuda\n\n## First\n\n## Second\n")
            self.assertEqual(run("--check").returncode, 0)
            before = target.read_bytes()
            (sections / "15-new.md").write_text("## New tool\n")
            self.assertNotEqual(run("--check").returncode, 0)
            self.assertEqual(target.read_bytes(), before, "--check must not write")
            self.assertEqual(run().returncode, 0)
            self.assertLess(target.read_text().index("## New tool"), target.read_text().index("## Second"))
            self.assertEqual(run("--check").returncode, 0)

    def test_missing_sections_fail_clearly(self):
        with tempfile.TemporaryDirectory() as temporary:
            script = Path(temporary) / "scripts/gen-skill.sh"
            script.parent.mkdir()
            shutil.copy2(SOURCE, script)
            output = subprocess.run([str(script), "--check"], capture_output=True, text=True)
            self.assertNotEqual(output.returncode, 0)
            self.assertIn("00-intro.md", output.stderr)


if __name__ == "__main__":
    unittest.main()
