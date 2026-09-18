"""Synthetic skills fixtures; no network, no shellcheck, no Rust build.

The lint reads a workspace-shaped tree from --root, so every case here is an
inert temporary directory: a good skill passes, and each way a committed skill
could go wrong fails with a finding naming it.
"""
import os
from pathlib import Path
import subprocess
import tempfile
import unittest

LINT = Path(__file__).resolve().parents[1] / "ci" / "skills-lint.sh"

GOOD = {
    "SKILL.md": "---\nname: sample\ndescription: inert fixture\n---\n\n# Sample\n",
    "references/notes.md": "Reads stay inside the instance directory.\n",
    "scripts/run.sh": "#!/bin/sh\nset -eu\necho ok\n",
}
GOOD_MODES = {"scripts/run.sh": 0o755}


class SkillsLint(unittest.TestCase):
    def lint(self, files, modes=None):
        with tempfile.TemporaryDirectory(prefix="skills-lint-test-") as directory:
            root = Path(directory) / "skills"
            for relative, content in files.items():
                path = root / "sample" / relative
                path.parent.mkdir(parents=True, exist_ok=True)
                path.write_text(content, encoding="utf-8")
            for relative, mode in (modes or GOOD_MODES).items():
                path = root / "sample" / relative
                if path.is_file():
                    os.chmod(path, mode)
            result = subprocess.run([str(LINT), "--root", str(root)],
                                    capture_output=True, text=True)
            return result.returncode, result.stdout + result.stderr

    def test_a_well_formed_skill_passes(self):
        code, output = self.lint(GOOD)
        self.assertEqual(code, 0, output)
        self.assertIn("skills-lint: passed", output)

    def test_frontmatter_name_must_match_the_directory(self):
        code, output = self.lint({**GOOD, "SKILL.md": "---\nname: renamed\n---\n\n# Sample\n"})
        self.assertNotEqual(code, 0)
        self.assertIn("!= directory", output)

    def test_frontmatter_must_be_present(self):
        code, output = self.lint({"references/notes.md": "no frontmatter here\n"},
                                 {})
        self.assertNotEqual(code, 0)
        self.assertIn("no SKILL.md", output)

    def test_user_scoped_mcp_registration_is_rejected(self):
        code, output = self.lint(
            {**GOOD, "scripts/install.sh":
             "#!/bin/sh\nclaude mcp add --transport stdio --scope user x -- /bin/sh y.sh\n"},
            {"scripts/run.sh": 0o755, "scripts/install.sh": 0o755})
        self.assertNotEqual(code, 0)
        self.assertIn("user-scoped MCP registration", output)

    def test_copy_into_a_user_level_skills_directory_is_rejected(self):
        code, output = self.lint(
            {**GOOD, "references/notes.md":
             'cp -R -n skills/sample "$HOME/.claude/skills/"\n'})
        self.assertNotEqual(code, 0)
        self.assertIn("user-level agent config directory", output)

    def test_tilde_spelling_is_rejected_too(self):
        code, output = self.lint(
            {**GOOD, "references/notes.md": "cp -R -n skills/sample ~/.claude/\n"})
        self.assertNotEqual(code, 0)
        self.assertIn("user-level agent config directory", output)

    def test_a_vendor_home_expression_is_not_a_skills_finding(self):
        # `${CODEX_HOME:-$HOME/.codex}` is a default the agent must know, not an
        # instruction to write anything; the reject list must not fire on it.
        code, output = self.lint(
            {**GOOD, "references/notes.md":
             "- `${CODEX_HOME:-$HOME/.codex}/computer-use/Thing.app`\n"})
        self.assertEqual(code, 0, output)

    def test_shell_scripts_must_be_executable(self):
        code, output = self.lint(GOOD, {"scripts/run.sh": 0o644})
        self.assertNotEqual(code, 0)
        self.assertIn("want an executable mode", output)

    def test_non_shell_files_must_not_be_executable(self):
        code, output = self.lint(GOOD, {"scripts/run.sh": 0o755, "references/notes.md": 0o755})
        self.assertNotEqual(code, 0)
        self.assertIn("want a non-executable mode", output)

    def test_umask_only_modes_are_accepted(self):
        # Git records one executable bit; a 0664 checkout file is not a finding.
        code, output = self.lint(GOOD, {"scripts/run.sh": 0o775, "references/notes.md": 0o664})
        self.assertEqual(code, 0, output)

    def test_self_test_mode_passes_without_a_workspace(self):
        result = subprocess.run([str(LINT), "--self-test"], capture_output=True, text=True)
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        self.assertIn("skills-lint: self-test passed", result.stdout)

    def test_the_committed_skills_lint_clean(self):
        root = Path(__file__).resolve().parents[2] / "skills"
        result = subprocess.run([str(LINT), "--root", str(root)],
                                capture_output=True, text=True)
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)


if __name__ == "__main__":
    unittest.main()
