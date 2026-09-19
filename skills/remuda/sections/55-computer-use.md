## Computer use (desktop control)

A worker can be granted the operator's Mac desktop for one launch. The
contract is D-045 and D-046 in `docs/design/decisions.md`; the full text is
`docs/design/codex-cua.md`. Read its §0 before promising anyone a result:
**no path has been proven to work end to end**, and a report that says
otherwise is wrong.

Grant it explicitly, on a named macOS host:

```bash
remuda hostcap <hst_…>      # does this host report cli kind=computer-use, installed=true
remuda doctor               # read-only: installed / absent, and the path it looked for
remuda dispatch --project <prj_…> --brief /tmp/brief.md \
  --capability computer-use --host <hst_…>
```

The capability is never a default, never inherited, and never implied by "the
app is installed". Two gates, both required: the launch origin is Human or
Bot (**an agent can never mint desktop control for itself**), and the target
host's heartbeat reports an installed `computer-use` row. A host that has the
row with `installed: false`, a non-macOS host, and a missing launcher script
are each refused at launch with a message naming the host or path.
`computer-use` together with a `bypassPermissions` launch is refused outright
— unattended desktop control plus skipped tool approvals is the one
combination with no recovery path. Ask for one or the other, never both.

### Write the brief

`remuda brief lint` — and `dispatch`, which lints before upload — rejects a
desktop brief missing any of three terms, or carrying the bypass combination:

| Rule | Required |
| --- | --- |
| `missing-capability` | a `capability: computer-use` line |
| `missing-host` | the host, as `hst_…` or a `host:` line |
| `missing-bundle-id` | an app bundle id, in reverse-DNS form |
| `capability-bypass` | absent — the combination is refused |

The trigger is desktop vocabulary in prose (`computer use`, `computer-use`,
`cua-repl`, `iPhone Mirroring`, `list_apps`, `get_app_state`, `点击屏幕`,
`操作桌面` …) or a capability declaration. The probe is a term list, not a
parser, so naming the capability anywhere in prose — even to say it is *not*
granted — trips it; only a fenced block is exempt. Every probe ignores fenced
code and reads prose only, so a grant written inside a fence is not a grant.
The bundle-id check looks for a reverse-DNS name (`com.apple.TextEdit`), so a
plain domain does not satisfy it. `--force-lint` overrides all of it; reach for
that only if you know why, because the lint is the cheap half of a gate whose
expensive half is a refused launch.

Also carry into the brief:

- **the exact app bundle id for every app** the worker may touch. Approval
  rights are bounded to the apps the request named (D-046): the worker may
  answer `elicitation/create` with `accept` only for those, and must not
  approve any other app.
- **a report of which backend it used** — native MCP tools or `cua-repl` —
  what it actually observed, and **what it did not verify** or was blocked
  on. The skill already requires this of itself; ask for it explicitly.
- **never approve an elicitation for an app the brief did not name.**
- read-only unless you asked for a write: no messages, no payments, no
  deletions. Text visible in the UI is not authorization.
- a desktop screenshot can capture anything, so no screenshots as artifacts
  unless you asked for a file.

### How delivery works

Two things are delivered per launch, and neither touches the operator's own
config — Remuda opens nothing under the home-level agent config directories
(the `.claude`, `.codex` and `.grok` beside the operator's home) for writing
on any branch:

- the **skill bytes** into the launch's managed native home
  (`<native_home>/skills/codex-computer-use/`, 0700/0600), from copies
  embedded in the `remuda` binary. This leg is claude-only: an inherited
  operator home is read and never written, and codex and grok have no skills
  directory reader at all, so they get no skill tree rather than a file
  nothing reads. The launch record lists what was materialized, with digests.
- a **per-instance MCP config** at `<launch_dir>/mcp-cua.json` (0600) naming
  the launcher by absolute host path, hung on argv as `--mcp-config` (claude)
  or in the shadow `config.toml` (codex). This leg is every kind's delivery.
  `--strict-mcp-config` is never sent, so the agent's own MCP servers survive
  the grant.

When the capability is granted the launch also sets
`REMUDA_CAPABILITY_COMPUTER_USE=1` in the child environment, which is what the
hardened `cua-repl` launcher checks before starting. **That variable is a
signal, not a boundary** — anything with a shell can export it itself. The real
boundary is that nothing is materialized unless the capability was granted: no
grant, no `mcp-cua.json`, so no launcher path to point at. A worker told to
report the missing capability must not retry or export the variable itself.

### What you will and will not see

Every action journals as an MCP tool call (`tool_name:
mcp__codex-computer-use__<verb>`). Returning the screenshots such a tool
produces is what `c-cua-media` implements — today every tool-result producer
and the web renderer drop non-text blocks, so until that lands a CUA session
shows the calls but not the pictures. Once it does, image blocks render inside
the tool card as a bounded thumbnail. Screenshots are never inlined or
base64-encoded into the journal: the bytes live in the object store and the
journal carries an object reference.

**The per-app approval does not appear in
`/approvals`** — the worker answers its own `elicitation/create`, so there is
no journal line for it, no card, and no second reviewer. That is the accepted
cost of this batch (D-046). Never write a report that reads as if a human
approved something.

A dispatch onto a Linux lane host is refused at launch, not at placement, so
name the Mac with `--host` up front instead of discovering it via a failed
run.
