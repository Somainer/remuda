# Codex Computer Use skill: origin and verification 1

Date: 2026-09-19.
Scope: the `skills/codex-computer-use/` skill that Remuda ships to a
launched agent, and the machine-state observations that shaped it.
Source: the skill's own `references/setup.md` verification log, moved here
verbatim and kept out of the shipped skill, which must stay instruction-only.

Design references: [decisions](../decisions.md) D-037 (per-launch
`computer-use` capability), D-038 (CUA screenshots in the journal);
plan (B) task `c-cua-skill` and its §3 integration notes.

**The load-bearing consequence first.** A Claude Code host consuming
Computer Use output — screenshots read back, UI actually operated — is
**unverified**. What *is* verified is that the stdio metadata handshake
succeeds and that a Codex-authenticated host can drive the JS surface. Any
claim in the skill, the plan or a brief that treats "the tools listed" as
"the UI works" is wrong; see the last two tables below for what was measured.

## What this skill is

Remuda redistributes nothing here. The skill contains no OpenAI binaries; the
capability comes from an already-installed Codex / ChatGPT Computer Use on a
macOS host, reached over stdio by one of two launchers:

- `scripts/launch-mcp.sh` — starts the installed `SkyComputerUseClient mcp`
  helper as a stdio MCP server. Works only when the host process is
  authenticated to the vendor service.
- `scripts/launch-cua-repl.sh` — starts the ChatGPT `cua_node` runtime with
  `@oai/cua-repl`, exposing a JS surface (`cua.listApps`, `app.click`,
  `app.typeText`, `app.pressKey`, …) over stdio JSON-RPC. This is the fallback
  when the native helper rejects the host as unauthenticated.

Two identifiers in the skill are third-party constants, not local state, and
are kept deliberately: the Mach service boundary `2DC432GLL2.com.openai.sky.CUAService`
(documented so an agent does not try to connect to it directly) and the
vendor path `/Applications/ChatGPT.app/Contents/Resources/cua_node` (overridable
with `CUA_NODE`). `CODEX_HOME` likewise moves the `Codex Computer Use.app`
lookup off its default.

## Verification log, 2026-09-17 (moved verbatim)

| 检查 | 结果 |
| --- | --- |
| 当前 Codex `cua.getState()` | 成功返回应用和浏览器清单；未进行 UI 输入 |
| 本机 `SkyComputerUseClient --help` | 提供 `mcp` 子命令 |
| 独立子进程 MCP `initialize`、`tools/list` | 成功，返回 10 个原生应用工具 |
| Claude Code 本机版本 | 2.1.274 |
| Claude Code 实际消费 MCP 截图及操作 UI | 未验证 |
| 用户级 skill 安装、MCP 注册及权限修改 | 本次开发未执行 |

## Verification log, 2026-09-19 (moved verbatim)

| 检查 | 结果 |
| --- | --- |
| `probe_mcp.py --schema` | `metadata_ok`，10 个原生工具 |
| 未认证宿主调用原生 `list_apps` | `Sender process is not authenticated` |
| Python 直连 `computeruse.sock` ping | 连接后被断开 |
| `launch-cua-repl.sh` 等价启动（ChatGPT `cua_node` + `CUA_REPL_ENABLED_SURFACES=computer`） | `cua.listApps` 成功 |
| `cua.getApp("com.apple.ScreenContinuity")` | 成功；需回答 `elicitation/create` |
| iPhone Mirroring 截图与坐标点击 | 成功；AX 树只有窗口铬 |
| 改 `clientInfo.name` 为 `codex` | 不能通过原生 MCP 鉴权 |
| Grok 会话内 Orca computer-use | 用户要求不要用 |

## Reading the tables

- **`metadata_ok` is not `ui_ok`.** `initialize` + `tools/list` succeed even
  for a host the vendor service will later reject; the next `list_apps` fails
  with `Sender process is not authenticated`. The probe in `scripts/probe_mcp.py`
  is bounded on purpose — 4 MiB cap, 1–60 s deadline, stderr drained and never
  printed, tools listed but never called — so it can never be evidence that a
  screenshot or a click works.
- **Do not connect to the Mach socket directly.** An unsigned process is
  disconnected by the service; that is the reason the launcher path exists at
  all.
- **Do not impersonate the vendor client.** Renaming `clientInfo.name` to
  `codex` does not authenticate a native MCP host.
- **The authenticated-caller row is the only end-to-end success recorded.** It
  was the Codex runtime driving the JS surface, not a Remuda-launched agent.
  Per the plan's Q1, `c-cua-launch`'s merge is gated on a live probe of a
  Remuda-launched agent driving `cua-repl`; until that exists, the launch work
  ships refusals and per-instance materialization only.
- **No user-level installation happened.** The `用户级 skill 安装…未执行` row is
  the state the repo must keep: a committed skill never writes the operator's
  own config, and no launch path may either.

## Hardening applied when vendoring (2026-09-19)

- The `$HOME/.claude/skills` copy and the user-scoped MCP registration were
  removed from `references/setup.md`; a launched agent is told to report a
  missing registration, never to create one.
- The two tables above were lifted out of `references/setup.md` into this file.
- `scripts/launch-cua-repl.sh` — desktop control — now refuses to run without
  an explicit capability acknowledgement flag.
- `scripts/ci/skills-lint.sh` enforces all three going forward: shellcheck on
  every committed skill script, frontmatter `name` == directory name, file
  modes, and a reject list for host-mutating instructions.

## Redaction rule for CUA evidence

A CUA screenshot can show anything that was on the desktop. Evidence in this
directory carries **redacted or synthetic** screens only; a real desktop
screenshot is never committed, and screenshots are never journaled or logged
inline. See plan (C) Q6 and D-038.

## References

- [Claude Code skills](https://code.claude.com/docs/en/skills)
- [Claude Code MCP](https://code.claude.com/docs/en/mcp)
- [Claude Code computer use](https://code.claude.com/docs/en/computer-use)
- [OpenAI Computer Use](https://learn.chatgpt.com/docs/computer-use)
