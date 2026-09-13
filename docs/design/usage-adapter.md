# UsageAdapter — 三家 harness 的用量与估价（D-028 条件 1）

状态：**模块已落地、已测，尚未接入任何 driver 的 tail**。接入（claude transcript tail / codex rollout tail / grok session tail 产生 `ObservationPayload::Usage`）是 print 退役闸门的后续工时；本文档先固定数据口径、价目表维护流程与已知偏差。

- 代码：`crates/remuda-driver/src/usage/`（`mod.rs` / `prices.rs` / `claude.rs` / `codex.rs` / `grok.rs`）
- 测试：模块内单测 + `crates/remuda-driver/tests/usage.rs`，夹具见 `tests/fixtures/usage/README.md`
- 关联：[D-028 决策](./decisions.md)（D-028/D-028a）、[native-pty-first.md](./native-pty-first.md) §「print 退役条件」、协议 §5.5（[protocol.md](./protocol.md)）

## 1. 背景与目标

今天全仓唯一的 `UsagePayload` 发射点是 `claude_print`，从 stream-json `result` 帧读 `total_cost_usd` 与 `usage`（`claude_print.rs::usage_from_result`）。print 退役后用量必须改走三家各自的结构化文件：

| Harness | 权威来源 | 粒度 |
| --- | --- | --- |
| claude（native TUI） | transcript `assistant` 记录的 `message.usage` | 每条 model message（内容块级重复，见 §3.1） |
| codex | rollout `token_usage_record`（每次模型响应） | per-response，另发累计快照 |
| grok | `usage.json` / `updates.jsonl` usage 字段 / headless `usage`+`end` | per-turn 或 per-session |

token 数来自原生文件（结构化证据）；**金额一律由本地价目表估算，UI 必须标注「估算」**。未知模型只出 token、不出金额。

## 2. 数据流

```text
 transcript.jsonl ──┐
 rollout.jsonl ─────┼─▶ usage::claude/codex/grok 解析函数
 updates.jsonl ─────┤      │（只做字段归一，无状态；codex 需记住 turn→model）
 usage.json ────────┘      ▼
                        UsageEvent { turn_id, request_id, model,
                          tokens: TokenCounters{ uncached_input, output,
                            cache_read, cache_write_5m/1h },
                          reasoning_tokens, cost_usd: Option,
                          reported_cost_usd: Option, estimated, source }
                              │
                              ▼
                        UsageAggregator（按各源原生键去重/拒累计）
                              │  per-turn totals / session totals
                              ▼
                        to_usage_payload() ──▶ UsagePayload
                          accounting: "estimated", mode: "snapshot"
```

接入时（后续 PR）每个 tail 轮询到新行后调用对应 extractor，把 `UsageEvent` 喂给一个 per-instance `UsageAggregator`，在 turn 边界与 session 结束时各发一个 snapshot 负载（`scope: turn|session`，`metricRevision` 递增；§5.5：snapshot 覆盖不累加）。

## 3. 各源口径

### 3.1 Claude transcript

- 一条原生 transcript 记录只装一个 content block；同一条 model message（`message.id` 相同）跨 2–7 条记录，**`message.usage` 逐条重复且完全一致**（已用主机上真实 transcript 抽样确认，usage 跨块无变化）。extractor 每条记录都产出事件，`UsageAggregator` 以 `message.id` 为键只计第一次。
- 字段：`input_tokens`（不含缓存）、`output_tokens`、`cache_read_input_tokens`、`cache_creation_input_tokens`；缓存写 TTL 拆分为 `usage.cache_creation.ephemeral_5m_input_tokens` / `ephemeral_1h_input_tokens`。缺拆分时整笔写入 5m 桶（低估优于高估）。
- `output_tokens_details.thinking_tokens` → `reasoning_tokens`（仍计入 output，不重复加）。
- turn 关联：新版记录可能带顶层 `requestId`；缺失则 `turn_id=None`，事件仍计入 session 总量，不凭邻接猜 turn。
- `isSidechain:true`（子 agent）记录丢弃。
- stream-json `result` 帧由 `claude::usage_from_result_frame` 解析，**仅用于 parity 对照**（print 仍在时的成本对账），native 路径不产生该事件。

### 3.2 Codex rollout

- `token_usage_record.payload.usage` = 单次响应，**可加**；response 去重键 `response_id`，turn 模型来自同文件 `turn_context.payload.model`（`CodexUsage` 按文件顺序维护 turn→model 映射，另支持 fallback model）。
- `event_msg/token_count.info.total_token_usage` = **线程累计快照**，每次响应后重发、resume 后原样再发（协议 §5.5）。extractor 仍解析（标 `CodexTokenCount`），aggregator `push()` 直接拒绝并计数（`ignored_cumulative()`），调用方即使无脑全喂也不会双算。
- 口径：Codex 的 `input_tokens` **含缓存**（`total = input + output`），extractor 减去 `cached_input_tokens` 与 `cache_write_input_tokens` 得到 uncached 桶（saturating，不允许负数）。`reasoning_output_tokens` 单独上报。
- OpenAI 不单独收 cache creation：价目表把写桶定价为普通输入价。

### 3.3 Grok

三种形态，全部按协议 §5.5 已验证的口径区分：

1. **`usage.json`**：接受单对象（自带 `modelId`）与 `{modelId: counters}` 映射两种形态，camelCase 键。**注意：P6 spike 只抓到文件名，没抓到文件体**（[evidence/grok-signals-1.md](./evidence/grok-signals-1.md) A 节文件清单），字段结构是从已抓到的 headless `end.modelUsage` 反推的——属已知偏差 §6，待真实文件确认。
2. **`updates.jsonl`**：`session/update` / `_x.ai/session/update` 帧，usage 可在 `update.usage`、`update._meta.usage` 或 `params._meta.usage`；turn 关联取 `update.prompt_id` / `_meta.promptId`，退化到 `sessionId`。按 §5.5，**ACP 的 `inputTokens` 含缓存**，减回 cache read/write 得 uncached。
3. **headless `usage`/`end` 帧**：snake_case，`input_tokens` **不含缓存**（§5.5 明确与 ACP 口径不等价，不可直接跨表加减）；`end.modelUsage` 按模型展开为额外事件（camelCase 键但语义 uncached），原生 `total_cost_usd` 保留为 `reported_cost_usd`。

去重：有 prompt/request 原生 id 才按键去重；无稳定 id 的记录逐条保留（§5.5：不凭邻接去重）。已提交的 TUI 夹具里没有 usage 对象（只有 `_meta.totalTokens` 提示），grok 形态由 `fixtures/usage/` 合成夹具覆盖。

## 4. 价目表（`prices.rs`）

- 纯数据 `const` 表：每行一个模型族（input / output / cache-read / cache-write 5m / cache-write 1h，**USD/MTok**）+ 模型 id 前缀列表。
- 元数据：`PRICE_TABLE_REVISION = 1`、`PRICE_TABLE_UPDATED = "2026-09-14"`。每次调价 **revision +1**，使日志中的估算可归因到具体表版本。
- 匹配：去日期快照后缀（`-20251001`）、大小写/空白不敏感、**最长前缀优先**（`grok-4-fast` 优先于 `grok-4`，`gpt-5-nano` 优先于 `gpt-5`）。`""` / `"unknown"` / 未收录 id（如 spike 构建 `"spike"`）→ 无价格：token 照常出，`cost_usd=None`，负载里 cost 为 `Knowledge::Unknown`（不是 0）。
- 状态：`Published`（revision 1 只有 claude-opus-4 / sonnet-4 / haiku-4-5 三张 Anthropic 卡，被 §5 的真实 `total_cost_usd` 对账间接佐证）；其余（claude-opus-5 / sonnet-5 沿用、GPT-5 全系、Grok 全系）为 `Provisional`，估价照出但在 print 翻默认前必须由 owner 按官方价格页核一遍。

### 4.1 维护流程

1. 从各家官方 pricing 页取卡，更新行数值与 `PRICE_TABLE_UPDATED`，`PRICE_TABLE_REVISION + 1`。
2. 新模型族加一行 + 前缀；新小版本通常只需确认前缀命中（跑 `prices` 单测）。
3. 把 `PriceStatus` 改回 `Published`；顺手把本文 §6 偏差表对应条目标记解决。
4. 跑 parity 测试（`cargo test -p remuda-driver --test usage -- parity`）；如价格变动使锚点断言失败，按 §5 重新记录对账数。
5. 一笔 conventional commit（例：`chore(driver): refresh usage price table to revision N`），commit message 里写来源。

## 5. Parity 对账（print 退役闸门证据）

`tests/usage.rs::claude_print_cost_parity_within_10_percent` 用真实 print `result` 帧（源自仓库内 `claude-askuser.jsonl`，模型 `claude-haiku-4-5-20251001`）：

| 指标 | 值 |
| --- | --- |
| 原生 `total_cost_usd` | **0.0141267** |
| 本地表估算 | **0.0131337** |
| 相对差 | **−7.0 %**（闸门：±10 %，通过） |

另一测试 `claude_transcript_and_result_paths_price_identically_*` 证明同一份计数器走 transcript 路径与 result 路径估价分毫不差（合成 transcript 的计数器与真实 result 帧逐项对齐）。

print 退役条件引用的是「连续 3 次全绿」的 parity gate；本次先固定**单次锚点 + 公差**作为基线。

## 6. 已知偏差（不假装精确）

1. **Haiku 估算稳定低于原生账单约 7 %**（多份夹具：6.8 %–7.4 %）。表价（1/5、cache read 0.1、1h write 2.0）与 Anthropic 公开卡一致；差额疑似原生账单含表外项目（如 web fetch/search 调用、版本侧计费差或服务端舍入）。print 存活时以原生值为准；print 退役后该 7 % 是「估算」标签的一部分，需在 ≥1 周覆盖期内继续对账。
2. **Opus 5 / Sonnet 5 沿用 4 代价目（Provisional）**：至表日期无独立公开卡可核。仓库 `workflow-control-plane-s4.jsonl` 的 opus-5 result 帧显示估算/账单偏差很大：帧 1 计数器（cache write 39,597 全记 5m、cache read 77,156、output 1,295）按 4 代卡估出 **0.9553**，原生仅收 **0.5332**（≈1.79×，说明 5 代卡或缓存计价明显不同）；同文件另有一帧计数器全 0 却收 0.0337（表外项目）。这正是 print 必须等三家用量覆盖 ≥1 周才退役的原因。
3. **缓存写 TTL 是估价最大的敏感点**：无 `cache_creation` 拆分时整笔记 5m 价（较便宜档）；若供应商实际按 1h 计费会系统性低估。上述 opus-5 帧即全 5m 记录。
4. **Grok `usage.json` 文件体未实测**：结构按 headless `end.modelUsage` 反推；首次读到真实文件时需复核字段名与 input 是否含缓存。
5. **Codex 输入含缓存 / Grok ACP 含缓存 vs headless 不含**：归一已按各源处理，但不同源之间不可直接相减假定等价（§5.5）。
6. **输出 token 的最终值在流中会变**：content-block 记录上的 `output_tokens` 在 block 停止时理论上可能只是中间值。当前实现按 `message.id` 去重保留**第一条**；已抽样确认真实 transcript 同 id 各记录 usage 完全一致，故无实际差异。若未来观察到同 id usage 真有变化，需改为「同 id 取最后一条」。
7. **协议可见性**：负载已有 `accounting:"estimated"|"reported"` 枚举，本适配器全部发 `estimated`，这是「估算」标签的协议通道。**给协议 owner 的请求**：若 UI 后续要展示「本地表 revision」「估算值 vs 原生 reported 值并列」或 provisional 置信度，现有字段不够，需要在 §5.5 增字段（如 `costBasis:{tableRevision, reported?}`）；本模块不私改协议 crate，只在事件层保留 `reported_cost_usd` 与表 revision 常量备用。
8. **无 usage 即 unknown**：所有未上报桶映射为 `Knowledge::Unknown`（reason `not-emitted`/`unpriced-model-or-no-usage`），不写 0；空 totals 的 session 负载 cost 也是 Unknown（有测试固定）。

## 7. 后续接入清单（非本 PR 范围）

- [ ] claude transcript tail 接入：`TranscriptTail` 新行 → `usage_from_transcript_line` → per-instance aggregator；turn 边界由现有 lifecycle 信号确定。
- [ ] codex：`RolloutTail` 全量喂 `CodexUsage`（可无脑喂，累计快照会被拒）。
- [ ] grok：tail `usage.json`（整文件 snapshot）+ `updates.jsonl`；首次真实 `usage.json` 复核偏差 4。
- [ ] 每次发负载带 `PRICE_TABLE_REVISION` 可追溯性（待协议字段，偏差 7）。
- [ ] parity gate 接 CI 连续 3 次记录，作为 print 翻默认的前置证据。
