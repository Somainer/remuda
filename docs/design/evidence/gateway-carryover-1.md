# gateway-carryover-1 实跑证据：网关/直连选择透传到原生 shell-pty 启动，宿主 settings 不得覆盖

工作树 `wt/c-gwcarry`（分支 `wt/c-gwcarry/gateway-overlay-precedence`，from `origin/main`）。

对应 owner findings（2026-09-17 14:32，远端 Node，`ssh-stdio` + driver `shell-pty`）：
在 New Session 选 `模型来源 = 网关`（profile Doubao AI）并输入
`passthrough/ark/seed-evolving`，Hub instance 记录写着 `delegation gateway` +
providerProfileId + 该 model，但 Node 的 `launch/settings.json` 落的是**宿主用户自己的
settings**，会话跑在宿主网关上，等同于选了 `跟随主机`。

脱敏规则（全文遵守）：env 只列 **KEY NAME**，base URL 只留 **host** 一段，
不出现任何 token 值、真实主机名、用户名或家目录路径。网关一律记作
`gateway.example`，宿主原生网关一律记作 `host-native.example`。

---

## 1 根因：两个缺陷叠加

**(a) 原生载体从未 materialise overlay。**
`remuda-node/src/native.rs::resolve_claude_overlay` 对
`DriverKind::ShellPty` 无条件 `return Ok(None)`，因此 Hub 下发的
`providerOverlay` + `providerAuthToken` 在原生载体上根本没有落盘，
driver 收到的 `settings_overlay = None`。
`claude-print` / `claude-pty` 不在这个早退分支里 —— 这正好解释为什么同一台机器
12:55 的 claude-print 启动带着三个 env key 和 Hub profile 的 base URL，而 14:32 的
shell-pty 启动没有：**回归只针对原生载体**。

**(b) 3638572a 之后宿主 settings 成了唯一的 provider 配置。**
该 commit 让 shell-pty 用 `user_settings_home` 把宿主 `~/.claude` 的
`settings.json` / `settings.local.json` 播种进 `launch/settings.json`。叠加 (a) 之后
宿主层是文档里**唯一**描述 provider 的东西：宿主的 `ANTHROPIC_BASE_URL`、
`ANTHROPIC_AUTH_TOKEN`、`ANTHROPIC_MODEL`、`ANTHROPIC_DEFAULT_*_MODEL`、
`CLAUDE_CODE_SUBAGENT_MODEL`、`CLAUDE_CODE_MAX_CONTEXT_TOKENS`，以及顶层 `model`
全部生效。

**为什么"宿主层放最低"不足以修复。**
两层并不是用同一组 key 描述 provider 的：宿主的 `env.ANTHROPIC_MODEL` 在 Claude
内部**压过** overlay 的 `model` key；overlay 若只带 `model`，宿主的
`env.ANTHROPIC_BASE_URL` 依然存活。而且 `materialize_shell_pty_agent` **不发
`--model` argv**，所以对 shell-pty 而言 overlay 的 `model` key 是请求 model 的唯一
通道 —— 宿主顶层 `model` 也必须剥掉，不能只处理 env。

---

## 2 修复：宿主为 BASE，Hub overlay 在 gateway/direct 上为 AUTHORITATIVE

优先级（低 → 高），文档同步写在 `launch/user.rs` 模块注释：

```text
user settings.json
⊕ user settings.local.json          （Claude 的 user 层 precedence）
⊕ Hub provider overlay              （gateway/direct：authoritative，先剥宿主 provider 键）
⊕ Remuda hooks（按 event 合并，用户 hooks 保留）
⊕ tui / showStatusInTerminalTab / terminalProgressBarEnabled 强制钉住
```

- `merge_provider_overlay_over_user` 在合并前剥掉宿主的 endpoint / 凭据 / model 变量
  （`is_overridden_provider_env`，大小写不敏感）与顶层 `model`；hooks、`permissions`、
  `theme`、`statusLine`、effort、自定义键、无关 `env` 全部保留 —— 这正是"宿主层留作
  base 而非丢弃"的意义。
- `delegation = none`（跟随主机 / 原生登录态）**行为不变**：宿主 settings 原样透传。
- delegation 要求 gateway/direct 但 overlay 缺失时，仍然剥掉宿主 provider 键（空
  overlay 合并为无），让会话退回 harness 自身登录态，而不是静默用宿主网关 ——
  否则这个失败模式与本 bug 无法区分。
- Node 侧 `ShellPty` 的 provider overlay 写进 `<launch>/provider/settings.json`，
  与 hook session 拥有的 `<launch>/settings.json`（合并**输出**）分离，避免互相覆盖；
  仍是 0600。

## 3 修复前后 launch settings 对照（同机真实代码路径，env 只列 KEY NAME）

`env.ANTHROPIC_BASE_URL` 一栏只显示 host 段。

### 修复前 (a) —— shell-pty：overlay 完全没落盘，只有宿主层

| 字段 | 值 |
|---|---|
| `model` | `ark/seed-evolving[1m]`（宿主的） |
| `env.ANTHROPIC_BASE_URL` host | `host-native.example` ❌ |
| `env` KEY NAMES（10） | `ANTHROPIC_AUTH_TOKEN`, `ANTHROPIC_BASE_URL`, `ANTHROPIC_DEFAULT_HAIKU_MODEL`, `ANTHROPIC_DEFAULT_OPUS_MODEL`, `ANTHROPIC_DEFAULT_SONNET_MODEL`, `ANTHROPIC_MODEL`, `CLAUDE_CODE_ENABLE_GATEWAY_MODEL_DISCOVERY`, `CLAUDE_CODE_MAX_CONTEXT_TOKENS`, `CLAUDE_CODE_SUBAGENT_MODEL`, `HOST_ONLY` |

### 修复前 (b) —— 即便 overlay 在场，朴素 low→high 合并仍然不够

| 字段 | 值 |
|---|---|
| `model` | `passthrough/ark/seed-evolving` ✅ |
| `env.ANTHROPIC_BASE_URL` host | `gateway.example` ✅ |
| `env` KEY NAMES（10） | 与上表同 —— **宿主的 `ANTHROPIC_MODEL` / `ANTHROPIC_DEFAULT_*_MODEL` / `CLAUDE_CODE_SUBAGENT_MODEL` / `CLAUDE_CODE_MAX_CONTEXT_TOKENS` 全部存活** ❌ |

`ANTHROPIC_MODEL` 压过 `model` key，所以 (b) 里真正回答的仍是宿主 model。

### 修复后 —— gateway delegation

| 字段 | 值 |
|---|---|
| `model` | `passthrough/ark/seed-evolving` ✅ |
| `env.ANTHROPIC_BASE_URL` host | `gateway.example` ✅ |
| `env` KEY NAMES（4） | `ANTHROPIC_AUTH_TOKEN`, `ANTHROPIC_BASE_URL`, `CLAUDE_CODE_ENABLE_GATEWAY_MODEL_DISCOVERY`, `HOST_ONLY` |
| 宿主 model / endpoint 变量 | 6 个全部剥除 ✅ |
| `hooks` / `permissions` / `theme` / `statusLine` / `enabledPlugins` | present ✅ |

`HOST_ONLY` 保留，证明剥的只是 provider 路由变量，不是用户的 `env`。

### 修复后 —— delegation none（跟随主机）

| 字段 | 值 |
|---|---|
| `model` | `ark/seed-evolving[1m]`（宿主的）✅ |
| `env.ANTHROPIC_BASE_URL` host | `host-native.example` ✅ |
| `env` KEY NAMES（10） | 与修复前 (a) 完全一致 —— 原样透传 ✅ |

## 4 Journal：一行 lifecycle 说明到底哪个 provider 源生效

回归在 journal 里是**看不见**的：Hub 记录说 gateway + profile id，会话跑在宿主网关上，
中间没有任何东西能反驳其中任何一方。新增 `provider_source_resolved`
（`LifecycleTopic::Configuration`，`SourceChannel::Runtime`）：

| 字段 | gateway 情形 | 跟随主机情形 |
|---|---|---|
| `status` | `overlay` | `host-native` |
| `related_ids.delegation` | `gateway` | `none` |
| `related_ids.providerProfileId` | recipe 的 profile id | 同 |
| `related_ids.effectiveModel` | `passthrough/ark/seed-evolving` | 宿主 model |
| `related_ids.hostSettingsSeeded` | `true` / `false` | 同 |

只有名字和 id：单测断言 `related_ids` 里既不含 `token` 也不含 `gateway.example`
—— journal 不是那个 0600 instance 文件。

## 5 Web：模型来源摘要行反映真正要启动的 model

`web/src/pages/NewSessionPage.tsx` 的摘要行原本插值
`defaultGateway.defaultModel`（profile 默认），于是截图里 model 输入框是
`passthrough/ark/seed-evolving`、摘要行却是 `Doubao AI · claude-opus-4-8`，页面与它
即将发起的 run 自相矛盾。改为插值 `launchModel` —— 也就是 create 请求真正携带的那个值
（`gatewayModel ?? model`，同一变量、同一渲染）。页面其余部分未改动。

## 6 测试

驱动三向合并（`crates/remuda-driver/src/launch/user.rs`）：

- `host_settings_alone_carry_over_untouched_for_native_delegation` —— delegation none 不改写宿主
- `the_hub_overlay_wins_the_endpoint_token_and_model_over_the_host` —— endpoint / token / model 归 Hub
- `the_hosts_model_and_endpoint_variables_are_stripped_when_the_overlay_applies` —— 6 个宿主变量剥净，且文档里不残留宿主 host 段与宿主 model
- `an_overlay_less_gateway_launch_still_strips_the_host_provider_config`
- `the_users_hooks_permissions_and_custom_settings_survive_the_overlay` —— 用户 hooks / permissions 存活
- `a_lowercase_or_mixed_case_host_variable_is_still_stripped`

全链路（`shell_pty.rs`，走真实 materializer + hook overlay）：

- `a_gateway_delegation_overrides_the_hosts_provider_and_journals_the_source`
  —— 合并结果 + journal 行 + 用户 settings 只读不写
- 既有 `a_scoped_native_home_merges_the_users_effective_settings`（delegation none）仍绿，证明 native-config-1 的透传没被破坏

Node overlay materialisation（`crates/remuda-node/src/native.rs`）：

- `a_shell_pty_gateway_launch_materialises_the_hub_overlay_not_the_hosts`
  —— 原生载体拿到 overlay、内容是 Hub 的 endpoint、不含宿主 base URL、不与 hook 合并输出同路径、0600
- `a_shell_pty_native_delegation_launch_gets_no_overlay`

Web：

- `summarises the model chosen for launch, not the profile default`
- `still points at the Provider page when no gateway is configured`

**反向验证（sabotage）**：逐个回退三处修复，确认测试真的会红 ——
去掉 `strip_overridden` → driver 3 red；把 `ShellPty` 放回 Node 早退分支 →
Node 1 red；摘要行改回 `defaultModel` → web 1 red。恢复后全绿。

## 7 验证命令与结果

```text
cargo test -p remuda-driver -p remuda-node --lib   →  361 + 198 passed, 0 failed
cargo test -p remuda-node（全 target）              →  10 suites ok, 0 failed
cargo clippy --workspace --all-targets -- -D warnings → clean
cargo fmt --all                                     → clean
pnpm --dir web exec vitest run src/pages/NewSessionPage.models.test.tsx → 7 passed
pnpm --dir web test（全量）                          → 114 files / 994 tests passed
```

环境既有失败（与本改动无关，已在未修改的 `origin/main` 上复现同样失败）：
`remuda-driver --test adapters_parity` 与 `--test effort_transcript` 需要
`remuda-testing` 的 `remuda` bin / `test-stub` feature，本机不具备。
