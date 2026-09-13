# 飞书 rich card 原地更新同步 Agent Session 调研

## 1 结论

**可行，建议做，但必须先拆两个硬阻塞。** 推荐路线：用 **CardKit 卡片实体**（`POST /open-apis/cardkit/v1/cards` 取 `card_id`，再以 `msg_type:"interactive"` + `content={"type":"card","data":{"card_id":"…"}}` 发消息），在 `remuda-feishu` 进程内新增 per-instance 单写者 actor，把 `/v1/follow` 的 `ObservationKind` 事件流折叠成 **500 ms 一 tick 的 `POST /open-apis/cardkit/v1/cards/{card_id}/batch_update`**；MVP 关闭 streaming、不依赖回调同步返回值、交互反馈由下一 tick 的卡片更新给出。两个阻塞：**(a)** Hub 的 `answer_interaction` 要求 `InputOrigin::Human`，dispatcher 持 bot-kind token 实测返回 403（已跑真实探针）；**(b)** 当前入站是 `lark-cli event consume` NDJSON 单向管道，**没有 3 秒同步响应通道**，所以 toast 与「回调内返回整卡」这两条常规反馈路径在今天的架构下都不可用。MVP ≈ 10 人日。

## 2 飞书卡片能力矩阵

出处约定：官方文档均可在 URL 末尾追加 `.md` 取纯 markdown 版（HTML 页是 SPA 壳，抓取会拿到空内容），下表 `doc:` 指 `https://open.feishu.cn/document/<path>`。`probe:` 指本次针对真实 OpenAPI 跑的探针（临时卡片实体，未发送到任何会话）。

| # | 能力 | 接口 / 字段 | 关键限制（数字均来自出处） | 标记 | 出处 |
|---|---|---|---|---|---|
| 1 | 消息级整卡替换 | `PATCH /open-apis/im/v1/messages/{message_id}`，body `{"content":"<卡片 JSON>"}` | 调用身份须等于发送者；**更新前后两版卡片的 `config` 都必须有 `update_multi:true`**；批量发送/独享卡不支持；14 天内（230031）；单消息 5 QPS，端点 1000/min + 50/s；≤30 KB（230025） | V | doc:uAjLw4CM/ukTMukTMukTM/reference/im-v1/message/patch |
| 2 | 卡片实体整卡替换 | `PUT /open-apis/cardkit/v1/cards/{card_id}`，body `{"card":{"type":"card_json","data":"<转义 JSON, 1–1,000,000 字符>"},"sequence":int,"uuid":opt}` | 仅创建该卡的应用身份可调（300311）；**覆盖全部内容**，见 §5.4 | V | doc:cardkit-v1/card/update |
| 3 | 组件级局部更新 | `PATCH /cardkit/v1/cards/{card_id}/elements/{element_id}`（`partial_element`）/ `PUT …/elements/{element_id}`（`element`）/ `POST …/elements`（`insert_before\|insert_after\|append` + `target_element_id`）/ `DELETE …/elements/{element_id}` | `partial_element` **不能改 `tag`**（300312）；`element` 全量替换可改 tag；权限 `cardkit:card:write`，tenant_access_token | V | doc:cardkit-v1/card-element/{patch,update,create,delete} |
| 4 | 一次多操作原子更新 | `POST /cardkit/v1/cards/{card_id}/batch_update`，`actions` = `partial_update_setting \| add_elements \| delete_elements \| partial_update_element \| update_element` | 多操作共用**一个 `sequence`**；仅 JSON 2.0；不支持 `update_multi:false`；身份须与创建者一致 | V | doc:cardkit-v1/card/batch_update |
| 5 | 卡片配置热更 | `PATCH /cardkit/v1/cards/{card_id}/settings`，`settings` 转义 JSON 1–100,000 | 只能改 `config` 与 `card_link` | V | doc:cardkit-v1/card/settings |
| 6 | 文本流式（打字机） | `PUT /cardkit/v1/cards/{card_id}/elements/{element_id}/content`，`{"content":"<全量文本 1–100,000>","sequence":int}` | 仅 `plain_text` / `markdown`（300310）；需 `config.streaming_mode:true`（关闭 300309，超时 200850）；**仅当旧文本是新文本前缀才有动画**，否则整块重渲染；`streaming_config.print_frequency_ms`（默认 70 ms）/ `print_step`（默认 1）/ `print_strategy` `fast\|delay`，均支持 `{default,android,ios,pc}` 分端；开启后 **10 分钟自动关闭** | V | doc:cardkit-v1/streaming-updates-openapi-overview, cardkit-v1/card-element/content |
| 7 | 流式期间仍可用其它写接口 | — | 官方原文「在流式更新模式下，开发者可调用卡片和组件接口对卡片持续进行全量更新、局部更新、文本流式更新，且不会触发接口的频率限制（QPS）」；但 **10 次/秒单卡上限不豁免** | V | 同上 |
| 8 | 独享/按人不同视图 | JSON 2.0 **不支持**；`update_multi` 默认 `true` 且置 `false` 被所有 CardKit 写接口拒绝（300302）。按人差异化只存在于 1.0 的延时更新 `card.open_ids[]`（且要求 `update_multi:false`） | 群里一张卡 = 所有人同一视图，**权限只能在回调里判，不能靠隐藏 UI** | V | doc:feishu-cards/card-json-v2-structure；doc:…/uMDO1YjLzgTN24yM4UjN |
| 9 | 交互回调 | 事件 `card.action.trigger`（webhook 或 SDK WebSocket 长连接）；**3 秒内返回 HTTP 200**（超时 200341，且不补推；处理异常或超时反而触发重推）；响应体可含 `{"toast":{...}}` 和/或 `{"card":{"type":"raw","data":{…2.0 卡片}}}`；2.0 卡不能被 1.0 卡替换（200830） | payload: `event.action.{value,tag,name,form_value,input_value,option,options,checked}`、`event.operator.open_id`、`event.context.{open_message_id,open_chat_id}` | V | doc:feishu-cards/card-callback-communication；doc:…/feishu-cards/handle-card-callbacks |
| 10 | 回调进行中禁止更新 | mid-callback 窗口内调 `batch_update` / `partial_update_element` / `content` 直接 **HTTP 400 + 200810**「The card is in an ongoing interaction and cannot be updated」 | 这是**对 OpenAPI 调用本身**的禁令，不只是「响应体里带卡片」那条路 | V | doc:cardkit-v1/card/batch_update 错误表 |
| 11 | 延时更新 token | `event.token`（`c-xxxx`），**有效 30 分钟、最多用 2 次**（300030 / 300040）；`POST /open-apis/interactive/v1/card/update` body `{token, card}` | 必须先响应回调再用；≤30 KB（100 KB 硬失败 100000）；1.0 时代接口 | V | doc:…/uMDO1YjLzgTN24yM4UjN |
| 12 | 频率上限 | **单卡片实体 10 次/秒**（卡片+组件级 OpenAPI 合计）；每个 cardkit 端点、`im` patch、`interactive/v1/card/update` 各 1000/min + 50/s；`im` patch 另加单消息 5 QPS | 流式模式豁免 QPS 限流，**不豁免 10 Hz** | V | doc:…/streaming-updates-openapi-overview 注意事项 |
| 13 | 体积上限 | 渲染后卡片 ≤30 KB（230025 / 200860）；元素+组件 **≤200**（300305）；create `data` 1–3,000,000；update `data`/`element`/`partial_element`/`actions` 1–1,000,000；`settings` 1–100,000；`content` 1–100,000；`card_id` 1–20；`element_id` 1–20；`uuid` 1–64 | style 标签会使实际体积大于请求体（膨胀系数 **UNVERIFIED**） | V（膨胀系数 U） | doc:cardkit-v1/* 字段表 |
| 14 | Schema / 生命周期 | `"schema":"2.0"` 必填（300303）；`element_id` 卡内唯一、字母开头、字母数字下划线、≤20（重复 300301）；**标题组件没有 element_id**；卡片实体只能发送一次、寿命 14 天（200750）；交互 30 天 | 第 14–30 天点击仍触发回调，但更新静默 no-op | V | doc:feishu-cards/card-json-v2-structure；doc:cardkit-v1/card/create |
| 15 | 客户端版本 | JSON 2.0 需客户端 **≥7.20**（更低版本仅显示标题+升级提示）；`collapsible_panel` 本体 **≥7.9**；其 `header.width`（`fill\|auto\|auto_when_fold`）需 **≥7.32**；streaming 的 `print_*` 参数需 **≥7.23**（7.20–7.22 用内置默认）；客户端交互错误码 ≥7.28 才可见 | 降级用 `config.min_version` + `fallback_card` | V | doc:feishu-cards/card-json-v2-structure；doc:feishu-cards/card-components/containers/collapsible-panel |
| 16 | form 一次提交 | 多选用原生 `form` + `checker` 子组件（勾选是纯客户端状态、零回调、不可能丢）；`form_value` 形如 `{组件 name: 该组件类型的值}`（input→string、checker→bool、多选→array） | form 内每个交互组件需非空唯一 `name`；提交按钮**只带** `name` + `form_action_type:"submit"`，多加 `action_type:"form_submit"`/`form_name`/`behaviors` 触发 **200530**（客户端本地拒绝，bot 收不到任何请求） | V（form_value 形状 V；200530 的混模式细节 U，来自本机实测记忆） | doc:feishu-cards/card-callback-communication；本机实测记忆 |

## 3 Remuda 侧现状与集成点

**进程与收发。** `remuda dispatcher`（`crates/remuda/src/cmd/dispatcher.rs:26`）或 `remuda hub --with-dispatcher`（`crates/remuda/src/cmd/hub.rs:97-113`），是通道适配器而非 agent loop（`crates/remuda-feishu/src/lib.rs:1-13`）。入站 = 两个受监督的 `lark-cli event consume` 子进程，分别订阅 `im.message.receive_v1` 与 `card.action.trigger`（`lib.rs:55-57`；`consume.rs:43-76`）。出站 = 子进程 `lark-cli im +messages-send/+messages-reply`（`outbound.rs:269-305`），默认 DryRun（`outbound.rs:94-103`）。

**关键架构事实（影响设计）：** NDJSON consume **没有同步返回通道**，所以 §2 第 9 行的「响应体带 toast / 带整卡」在今天用不了。

**卡片现状。** 只会**发新消息**：`LarkCli::send_card` / `reply(OutboundBody::Interactive)`（`outbound.rs:168-184, 206-225`）；渲染器 approval / question / progress / completion / recorded / expired 在 `cards.rs:122-397`。**没有任何原地更新**：`CardKitStream` / `CardKitOp`（`cards.rs:40-119`）只是占位类型（注释写明「Live HTTP is M2-06」），`CARDKIT_MAX_HZ = 10`（`cards.rs:14`）已编码了单卡 10 Hz 上限，`cards.rs:302,332` 静态写死 `"streaming_mode": false`。

**事件流。** `ObservationKind`（`crates/remuda-protocol/src/enums.rs:708-724`）：`message`/`thought`/`tool_call`/`tool_result`/`interaction.{requested,answered,expired}`/`workflow.*`/`lifecycle`/`usage`/`artifact`/`raw_tty`/`opaque`；信封含 `seq`/`eventId`/`observedAt`/`completeness`/`source.driverKind`（`observation.rs:852-892`）。PTY 屏幕快照 = `lifecycle→native{nativeName:"screen"}`，**250 ms 轮询、变化才发、≤80 行**（`generic_pty.rs:42,823-883`）；herdr 状态 = `nativeName:"agent_status"` working/idle/blocked/done（`claude_pty.rs:882-890`）。`raw_tty` 在 follow socket 上是**二进制帧**（`ws.rs:862-866`）。

**订阅。** `GET /v1/follow`（`crates/remuda-hub/src/ws.rs:166-182`）：握手需 Origin + 已认证设备，**Agent origin 被拒**（`ws.rs:176-178`），Bot/Human 通过；attach 时推 `{type:"snapshot",instanceId,asOfSeq,events:[从 seq 0]}`（`ws.rs:904-920`），之后每次 append 推 `{type:"event",…}`。**没有 afterSeq 续传**：背压时发 `{type:"gap",reason:"backpressure"}` + 全量快照（`ws.rs:1047-1062`），bus 容量默认 256（`ws.rs:111-119`）。HTTP 侧 `GET /v1/instances/{id}`（`http.rs:273-287`）与 `GET /v1/instances/{id}/journal?afterSeq=N`（`http.rs:567-593`）是今天 dispatcher 每 `follow_interval_ms`（默认 1000，`config.rs:155`）轮询的路径（`dispatcher.rs:630-638, 1029-1103`）。服务端已有折叠器 `remuda_journal::projection::{TranscriptProjection,InteractionProjection,StatusProjection}`（`crates/remuda-journal/src/projection.rs:84-152`）。客户端订阅入口 `HubClient::follow_ws`（`crates/remuda-hub-client/src/lib.rs:274-290`）。

**控制面。** `POST /v1/instances/{id}/commands`：`instance.send`（`payload.input={type:"prompt",mode:"new-turn",blocks:[{type:"text",text}]}`，`completionScope:"native-turn"`，带 `idempotencyKey`；`hub_api.rs:75-95`、`http.rs:403-462`）、`instance.cancel`（`hub_api.rs:97-108`）、`instance.configure`（`http.rs:449-455`）、`tty.write`/`instance.keys`（`agent_scope.rs:165-169`）；回答交互 `POST /v1/interactions/{id}/answer`（`interactions.rs:20-24,146-224`），Hub 不本地提交，Node first-answer-wins 后镜像。

**已知缺陷。** `map_journal_event`（`hub_api.rs:177-231`）匹配的是 `"tool"|"tool_use"|"tool-boundary"` 和 `"result"|"completion"` 这些**不存在的字符串**，真实 kind 是 `tool_call`/`tool_result`/`lifecycle`，所以进度卡与完成卡对真实 journal 基本不触发。`TicketStore` 纯内存（`tickets.rs:120-126`），重启丢失全部开放卡片绑定；`follow_live` 不过期空闲 ticket（TODO 在 `cmd/dispatcher.rs:313`）。

**落点建议。** 放在 `remuda-feishu` 进程内，新增 `cardkit.rs` + `session_card.rs`，与 `dispatcher.rs` 并列：会话↔会话映射、ticket、owner 门禁、lark-cli 身份都已在此（`dispatcher.rs:34-63`、`tickets.rs:36-59`、`inbound.rs:218-229`），Hub 保持通道无关。代价：单进程持 N 条 follow 连接并独占 CardKit 频率预算。

## 4 推荐设计：Session Card

### 4.1 卡片结构

`config`：`{"schema":"2.0"}`（顶层）、`update_multi:true`（2.0 强制）、`streaming_mode:false`（MVP 恒假）、`width_mode:"fill"`、`enable_forward:false`、`min_version:{"version":"7.20"}` + `fallback_card`、`summary.content:"[remuda] {task} · {state}"`（手机推送预览文案）。`header`（**无 element_id，只能整卡 PUT 改**）：title `⛺ {agent} · {inst_short}`、subtitle `{driver} · {model}/{effort}`、`template` blue/orange/green/red/grey、`text_tag_list`；整会话内整卡 PUT 应 ≤6 次（终态、rollover、gap 纠偏）。

`body.elements`（约 9 个，`element_id` 字母开头、≤20 字符）：

| element_id | tag | 内容 | 截断 |
|---|---|---|---|
| `st_line` | markdown | `🟢 working · turn 7 · 3m12s · seq 214` | — |
| `act_line` | markdown | `🔧 Edit(…/ws.rs) · applied` | ≤120 B |
| `stream_tx` | markdown（预埋 `enable_streaming`） | 助手正文，**严格 append-only** | ≤6000 B |
| `scr_panel` → `scr_txt` | collapsible_panel（PTY driver 默认展开） | 屏幕尾窗代码块 | 末 16 行 × 120 列 ≤1600 B |
| `hist_panel` → `hist_md` | collapsible_panel `expanded:false` | 最近 3 轮摘要 | 3 × 200 字 |
| `act_zone` | markdown / column_set / form（三形态） | 交互区 | title ≤40 字 |
| `ctl_row` | column_set | `btn_stop`(danger+confirm) / `btn_input` / `btn_web`（`action_type:"link"`；带 `type:"callback"` 的 interactive_container 有按钮消失的客户端 bug，**U**） | 每行 ≤2 按钮，`width:"fill"` |
| `in_form` | form `name:"f_prompt"` | `input name:"prompt"` + 提交按钮只带 `name` + `form_action_type:"submit"` | — |
| `foot_note` | markdown | `卡片 2/2 · 10:31:02 · owner @x · 降级:无 · 12.3k tok` | — |

超过 24 KB 估算值时先把 `stream_tx` 压到 4000 B，再不行走 rollover（§4.5）。

### 4.2 状态映射表

| 来源 ObservationKind | 落点 | 规则 |
|---|---|---|
| `lifecycle` entity + `InstanceRecord.lifecycle/activity` | `st_line`；终态另触发整卡 PUT 改 header | hash 去重 |
| `lifecycle→native{agent_status}` | `st_line` | 边沿触发，立即 flush |
| `lifecycle→native{screen}`（250 ms、80 行） | `scr_txt` | 同 tick 只留最后一帧；**先过脱敏正则**（token/key/Bearer/sk-） |
| `tool_call` / `tool_result`（`toolName`、`changes[].application`、`state`） | `act_line` | 只留最新一条 |
| `message(role=assistant, phase=final\|commentary)` | `stream_tx` | 按 `messageId`+`revision` 折叠；超限则旧段压入 `hist_md` |
| `message(phase=input)` | `foot_note` 一行 `↩ prompt by @X` | 不进正文 |
| `interaction.requested` | `act_zone` + header 转 orange + 改写 `summary.content`（触发手机推送） | 立即 flush |
| `interaction.answered` / `.expired` | `act_zone` → 只读「已允许 · @X · 14:22」 | 先 `disabled:true` 再替换 |
| `usage` / `artifact` | `foot_note`（artifact 最多 3 个 open_url 按钮） | 低频 |
| `thought` / `raw_tty` / `workflow.*` / `opaque` | 丢弃 | **但推进 watermark** |

### 4.3 更新管线

订阅 `follow_ws`（`remuda-hub-client/src/lib.rs:274-290`）替换 1 秒轮询；自持 `last_applied_seq` 水位，snapshot 与 gap 重放一律丢弃 `seq <= watermark`；收到 `gap` 不做增量，直接一次**整卡 PUT**并把 watermark 置为快照 `asOfSeq`。HTTP `journal?afterSeq` 保留为 WS 不可用时的降级。

每 instance 一个 tokio **单写者 task** 独占 `card_id`（sequence 天然单调，免锁），**tick = 500 ms**（> screen pump 的 250 ms，远低于 10 ops/s），一个 tick 折叠成**一次 `batch_update`**（多个 `partial_update_element` + `act_zone` 的 `update_element`，共用一个 sequence）≈ 2 ops/s。立即 flush 的事件：`interaction.requested`、lifecycle 终态、`interaction.expired`、回调入队后 +300 ms（避 200810）。

接口选择：多元素 → `batch_update`；`act_zone` 换 tag → `update_element`（`partial_element` 改不了 tag，300312）；header / 大改 / gap 纠偏 → `PUT /cards/{card_id}`；config → `PATCH …/settings`；`im/v1/messages/patch` 仅作 CardKit 全线不可用时的降级（届时前后两版都必须带 `update_multi:true`）。

`sequence`：per-card i64 落 SQLite，**先写意图行 `(seq, uuid, op_digest)` 再发 HTTP**；`uuid = hash(card_id, seq, op_digest, attempt)` 做幂等；重启后从 `persisted + 1000` 起；收 300317 则 `local + 1` 重发一次。**注意整卡 PUT 会把基线抬到它自己的 sequence**（§5.4）。

### 4.4 交互与权限

2.0 只有共享卡，群内一张卡一个视图，**权限必须在回调里判**。按钮 `value`：`{v:1, act, cid, iid, rv, opt, n}`。

- approve/deny → `POST /v1/interactions/{iid}/answer`，`idempotencyKey = uuid5(iid, rv, answer_digest)`
- form/prompt → `instance.send`，`idempotencyKey = sha256(card_id|element_id|rv|operator|form_digest)`
- stop → `instance.cancel`（带 confirm）；model/effort → `instance.configure`
- PTY 选项（D-022）→ `tty.write` / `instance.keys`，按钮文案必须写明「将发送 ↵/1」，反馈措辞「已记录，等待原生确认」（`cards.rs:352-373` 已承认原生可能未确认）

**回调纪律（NDJSON 版）：** consume 无同步返回通道 ⇒ 无 toast、无回调内换卡。流程为：ACL 校验 → 校验 `rv` 与 ticket → `card_answers(iid, rv)` UNIQUE 插 pending → 入队 → **+300 ms 后由单写者以 `batch_update` 给出可见反馈**，遇 200810 退避重试 ≤3 次。`form_value` 在 Remuda 总线上可能是 **JSON 字符串**而非对象，必须走 `tickets.rs:468` 的 `parse_form_value`。

**身份阻塞：** `answer_interaction` 要求 `InputOrigin::Human`（`interactions.rs:153-156`），bot-kind token 实测 403（§5.6）。MVP 方案：syncer 依 `event.operator.open_id` 映射到已登记的飞书用户，持一枚 **human-kind device token** 调用 answer，保留归属；v2 再评估 Hub 侧「Bot 代人 + actor 审计」通道（会改动 D-018 语义，需新增 decision 条目）。

### 4.5 失败处理

| 错误 | 处置 |
|---|---|
| 300317 sequence 未递增 | `seq = local+1` 重发一次；再失败整卡 PUT |
| 200810 交互进行中 | 300 ms 退避重排队，≤3 次 |
| 300313 element 不存在 / 300309 流式已关 | 说明上一次整卡 PUT 漏声明 → 立即整卡 PUT 纠偏（§5.4） |
| 230025 / 200860 / 300305 / 200750 / 230031 | rollover 换新卡 |
| 300311 身份不符 | 停用 CardKit，走降级阶梯 |
| 429 / 频控 | 令牌桶 **8 ops/s per card**、全局 40/s 且 800/min；队列做**状态合流**（丢中间态只留最新），绝不堆积 |

**降级阶梯：** 完整卡 → 精简卡（`st_line`+`act_line`+`ctl_row`）→ 纯文本 outbound → 仅日志告警；当前档位写进 `foot_note`。

**rollover 触发**（估算 >24 KB / 元素 >150 / 卡龄 >12 天 / 上表硬错误）：旧卡最后一次 `batch_update` 置按钮 `disabled:true` 且 `st_line` 写「↓ 已续接新卡片」→ 建新实体 + 发新消息 → **确认新 `message_id` 落库后**才切换写入目标。

**飞书 WS（若 v2 改用 SDK 长连接）：** `handshakeTimeoutMs:15000` + `wsConfig.pingTimeout:60` + 30 s 看门狗读 `getConnectionStatus().state`（不能用「最近没事件」判活）；重连后对所有 live 卡片各做一次整卡 PUT 纠偏。渲染器写成 total 函数，单元素 panic 捕获替换为「(渲染失败)」。

## 5 已验证 / 已推翻的关键假设

**5.1 流式模式下 CardKit 局部更新仍有效 —— SUPPORTED（措辞需修正）。** 官方原文：「在流式更新模式下，开发者可调用卡片和组件接口对卡片持续进行全量更新、局部更新、文本流式更新」；`batch_update` 的「使用限制」全文只有三条（仅 2.0、不支持 `update_multi:false`、身份一致），**无 streaming 例外**。修正：被禁的不是「流式期间」而是**卡片回传交互回调进行中的整个时间窗**，且禁令作用在 OpenAPI 调用本身 —— mid-callback 调 `batch_update` 直接 400 + 200810。官方步骤四要求先 `streaming_mode:false` 再处理回调。另：10 Hz 单卡上限不豁免。

**5.2 `act_zone` 换 tag 用 `update_element` 可行、`partial_element` 报 300312 —— SUPPORTED。** `partial_element` 字段说明原文「不支持修改 `tag` 参数」；300312 = 「Unable to update element tag」。`PUT …/elements/{id}` 的错误码列表中**不含 300312**，佐证该限制只作用于 PATCH 路径。注意 markdown→form 是容器化改造，仍可能因结构问题失败（300121），且换成 form 后下一个要防的错是 200530。

**5.3 toast-only 回调不 bump 版本、5 次连点全部到达 —— UNVERIFIABLE（且立论依据被推翻）。** 「5 下只到 2 下」来自**相反实验**（每次点击都返回整卡）的**失败结果**，且两份记录对点击速度的描述互相矛盾（「快速连点」vs「慢点」）。官方回调响应体只有 `toast` 与 `card{type,data}`，`card.action.trigger` payload 无任何 version/sequence 字段，「卡片版本」不是 API 概念。官方只支持弱结论：`{}` 或纯 toast = 不更新卡片（渲染语义），**不等于投递可靠**；3 秒同步窗无补推（200341）与超时重推并存，「恰好 5 次各一次」无路径保证。对 Remuda 更是空谈：NDJSON consume 没有响应通道。两位作者最终都改用原生 form + 一次 `form_submit`。

**5.4 整卡 PUT 不重置 sequence、不影响 element_id 绑定 —— REFUTED（后半句错）。** 真实探针：① sequence 基线**不重置**，但整卡 PUT 与流式/局部更新**共用同一个 per-entity 计数器**——PUT 自身 sequence 低于基线被 300317 拒，成功后把基线抬到自己的值（实测 element seq=1000 ok → 整卡 PUT seq=500 拒 → 1500 ok → element seq=1200 拒、1501 ok）；`streaming_mode` true→false→true 也不重置（Go SDK 里「一次流式状态期间」的注释已过时）。② **element_id 绑定被摧毁**：整卡 PUT 是「覆盖更新卡片实体的所有内容」，新 JSON 未重新声明的 element 立即失效，再调其组件接口报 **300313 `not find elementID`**；新 JSON 省略 `config.streaming_mode` 会导致后续流式写入报 **300309**。**设计含义：任何整卡 PUT 必须原样重新声明全部 element_id 与 config。**

**5.5 `form_value` 形状为 `{name: value}` —— SUPPORTED（两点精确化）。** 官方 payload 示例的键即组件 `name`；checker 在 form 内 `name` 必填且卡内唯一，与 input 混用合法（本机 create-lark-card skill 的 `BaseFormElem` 联合类型同时含二者）。精确化：(a)「混用」是废话——该形状对所有 form 子组件无条件成立；(b) value 按类型分化：input→string、checker→bool（真机 `{"summary":true,"qa":false}`）、多选→array、picker→带时区字符串；提交按钮自身的 `name` 走 `action.name` 不在 `form_value` 里，`action.value` 为 `{}`。**Remuda 专属坑**：经 lark-cli 总线时 `form_value` 可能是 JSON 字符串（`tickets.rs:468`、fixture `tests/fixtures/card-action-form.jsonl`）。另发现本机 create-lark-card skill 的 form-container 文档教的提交按钮写法（同时给 `action_type:"form_submit"` + `form_action_type` + `form_name`）正是触发 200530 的组合，**以真机记忆为准**。

**5.6 bot-kind token 调 answer 返回 403 —— SUPPORTED（已跑真实探针）。** 临时集成测试（跑完已删除）：`login-bot` 403 `{"code":"FORBIDDEN"}`、`minted-bot`（即 `RunningHub::mint_bot_device_token`，dispatcher 自身路径）403、`human-control` 对同一不存在的 interaction 返回 **404**、匿名 401。human 的 404 是决定性判别器：403 来自 `interactions.rs:153-156` 的 origin 门禁而非鉴权或对象缺失。无 header 能把 bot 提权（`agent_scope::caller` 只会降级）。现有测试未覆盖（`crates/remuda-node/tests/wss.rs:1184-1188` 只测 agent token）。**结论：combined 模式下今天的 `/yes`、`/no` 与所有审批卡按钮对 Hub 必然 403。**

**5.7 `collapsible_panel` 在 7.20 可用 —— SUPPORTED（归因需修正）。** 组件自身门槛是 **7.9+**（低于此显示「请升级至最新版本客户端」占位），7.20 是 **JSON 2.0 整体**的门槛而非该组件的。唯一的绝对化错误：`header.width`（`fill`/`auto`/`auto_when_fold`）文档标注**需 7.32+**，用了就需要更高版本；本机 create-lark-card skill 的 schema 暴露了该字段却无版本警告。

**5.8 手机端整卡 PUT 闪烁/滚动跳位、batch_update 不会 —— UNVERIFIABLE，且后半句过宽。** 12 篇官方 .md 中「闪烁/抖动/滚动/重绘/移动端」零命中；唯一负责二者取舍的《更新卡片》按**内容差异大小**而非渲染表现给建议。`batch_update` 含 `add_elements`/`delete_elements`/`update_element`，改变高度必然重排，不能声称「不会位移」。官方唯一为移动端差异化开放、且以上屏平滑为目的的能力是**流式文本更新**的 `print_frequency_ms`/`print_step` 分端参数，不是 batch_update。少用整卡更新的正当理由是版本窗内旧版点击被静默丢弃与 10 Hz 上限，不是「手机闪烁」。

## 6 限制与风险

1. **共享卡唯一视图**：2.0 无独享卡，群内任何人都能点；ACL 只能在回调里判，拒绝也需可见反馈（MVP 延迟 300 ms+）。
2. **NDJSON 无同步通道**：点击后至少 300 ms 才有视觉反馈；且「不返回任何东西是否触发客户端 200671」取决于 lark-cli 是否自动 ACK —— **未验证，MVP 首要验证项**。
3. **身份**：human-kind token 意味着 syncer 持有可代人回答审批的凭证，须最小权限 + 可吊销 + 全量审计，是安全面的实质扩张。
4. **PTY driver 无 assistant message**：claude-pty / generic-pty 只能渲染 `lifecycle→native{screen}`（屏幕派生、80 行、**含泄密风险**，必须脱敏）或 `raw_tty`，而非转写文本。
5. **频率**：250 ms 的 screen pump 与 10 Hz 单卡上限之间只有 500 ms tick 一层保护；N 路并发是否先撞 1000/min、50/s 未验证。
6. **双时钟**：卡片实体 / 消息更新 14 天，交互 30 天；14–30 天点击有回调但更新静默 no-op，须在到期前主动滚动并置灰。
7. **重启**：今天 ticket 纯内存，卡片绑定全丢；须落库并在启动时逐卡整卡 PUT 纠偏（遵守 §5.4 的重新声明规则）。
8. **无 seq 续传**：任何背压都会重放整段 journal，去重逻辑有洞就会重复渲染或重复建卡。

## 7 分阶段实施

### MVP ≈ 10 人日 —— 验收标准：一条真实会话从创建到终态，卡片原地更新 ≥50 次无 300317/230025，审批按钮点击后 ≤1.5 s 卡片显示「已记录」，重启 dispatcher 后同一张卡继续更新

1. **(2 d)** `crates/remuda-feishu/src/cardkit.rs`：create / 整卡 PUT / batch_update / settings，sequence 分配器 + 意图行 + uuid + 令牌桶 + typed error；替换 `cards.rs:40-119` 的占位 `CardKitOp`。
2. **(1.5 d)** 按 `ObservationKind`（`enums.rs:708-724`）重写 `hub_api.rs:177-231` 的错误字符串匹配；CardModel 折叠复用 `remuda-journal/src/projection.rs:84-152`。
3. **(2 d)** `session_card.rs`：per-instance actor、500 ms 合流、diff→ops、element_id 常量表、rollover（含「新 message_id 落库后才切目标」）。
4. **(1.5 d)** `follow_ws` 替换 `dispatcher.rs:630-638 / 1029-1103` 的轮询 + watermark/gap 处理；HTTP journal 保留降级。
5. **(1 d)** 存储：`session_map`（`dispatcher.rs:110-129`）扩为 card_state（`message_id`、`card_id`、`last_synced_seq`、`sequence`、render hash、降级档位）+ `card_answers(iid, rv)` UNIQUE；tickets 落库。
6. **(2 d)** 回调改造（`inbound.rs` 接 `card.action.trigger`）：ACL、200530-safe form、`parse_form_value` 字符串兜底、human-kind device token、补 `tests/hub_dispatcher.rs` 的 answer 用例（当前零覆盖）。

**MVP 明确砍掉**：streaming、`hist_panel`、artifact 按钮、i18n、聚合卡、`type:"callback"` 的 interactive_container。

### v2 ≈ 6 人日 —— 验收标准：注入式故障测试全绿；打字机在 print 类 driver 上连续输出 3 分钟不闪烁且终态自动退出流式

1. **(2.5 d)** 假 CardKit server 注入 300317 / 230025 / 200810 / 300313 / 429，三个 e2e：rollover、重启恢复、WS limbo。
2. **(2 d)** `stream_tx` 打字机：`PUT …/elements/stream_tx/content`，`print_frequency_ms:{default:70,ios:50}`、`print_step:2`、`print_strategy:"fast"`；**仅在前缀增长时使用**，仅 print 类 driver，终态必须 `streaming_mode:false` 并重写 `summary.content`（关闭流式不会自动复位 summary），且每 8 分钟 re-arm 以避开 10 分钟自动关闭。
3. **(1.5 d)** `hist_panel` + artifact 按钮 + 群内聚合索引卡。

## 8 未决问题

1. `lark-cli event consume` 收到 `card.action.trigger` 后是否自动回 HTTP 200 `{}`？若否，用户每次点击都会看到 200671。**MVP 阻塞级，需真机验证。**
2. 若要 toast/即时反馈，是否值得在 dispatcher 内另起一条 SDK WebSocket（单 app 单连接约束、与现有 consume 子进程的事件订阅冲突如何切分）？
3. 卡片实体 14 天寿命从 `POST /cards` 创建起算还是从 `im` 发送起算？（影响 rollover 阈值）
4. `lark-cli` 是否已有 cardkit v1 子命令？若无，syncer 需直连 OpenAPI 并自持 tenant_access_token —— 这会在 Remuda 内新增一处凭证管理面。
5. `config.enable_forward` 与 `config.width_mode:"fill"` 在 JSON 2.0 中是被接受还是被忽略？
6. 30 KB 上限按渲染后大小计的具体膨胀系数（设计里暂用 ×1.35，**纯估算**）。
7. 单卡 10 ops/s 与端点 1000/min、50/s 是否独立计数？N 路并发会话的真实天花板是多少？
8. Hub 是否应新增「Bot 代人回答 + actor 审计」通道（改 D-018 语义），以免 syncer 长期持有 human-kind token？
9. 群内多会话时，是每会话一张卡还是一张索引卡 + 线程内子卡？（`AnswerScope::Chat` 表明回调不带 thread id，`tickets.rs:61-89`）
