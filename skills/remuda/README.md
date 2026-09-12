# Editing the Remuda skill

Edit the relevant file in `sections/`, then run `./scripts/gen-skill.sh`.
Commit the section and the regenerated `SKILL.md` together. CI runs
`./scripts/gen-skill.sh --check` and rejects a stale assembled copy.

Sections are concatenated in filename order with one blank line between them.
`00-intro.md` owns the YAML frontmatter. A new section needs only a new `.md`
file with a sortable filename; no registry or script edit is required.

If generated `SKILL.md` conflicts during a rebase, resolve the source sections
and regenerate it instead of merging the assembled text by hand. The section
files are authoritative; keep future feature edits within their own section.
