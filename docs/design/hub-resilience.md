# Hub 韧性：笔记本工况下的连接韧性与「诚实陈旧态」

**状态**：规格已定，**全部未实现**（docs-only，2026-09-22，c-hubresil）。本文合并
原计划的 c-node-buffer / c-reconnect-storm / c-live-stale / c-egress-pin 四项——
同一个问题的四个面：**Hub 短暂消失时，系统表现如何、用户看到什么**。

**裁定边界（2026-09-22，所有者原话约束）**：本轮**不做公网前门**；暴露姿态
（D-031）暂不决。本文不写任何前门 / 会合点 / 公网可达内容，不据本文任何一条
论证要不要做前门。既定前提是：**手机能控制的时候，跑 Hub 的笔记本必须开机且
在线，但手机与笔记本不必在同一局域网**。所以「优雅扛住 Hub 短暂消失」不是锦上
添花，是这个拓扑能否成立的前提。

**相关文档**：[api-routing.md](./api-routing.md) §11（egress 载荷不变量）、
[decisions.md](./decisions.md) D-019（Node 生命周期独立于控制连接）、D-021
（secret 分域）、D-035（拒绝而非静默改道）、D-049（badge 语义）、
[hub-topology-plan 简报](../../)（所有者裁定记录，仓库外，不入库）。

**阅读约定**：第 2 节是现状调查，每条断言带 file:line，并且明确区分
**「代码里有」**与**「只是计划里写过」**。第 3–6 节才是规格。规格条目标
【规格】，现状条目标【现状】。

---

## 1. 失效模型：Hub 会怎样「短暂消失」

Hub 跑在一台会合盖、会休眠、会换网的笔记本上。本文覆盖的空窗都是**分钟级、
可恢复**的：

1. 合盖/休眠后唤醒（TCP 半开，对端可能长时间不发 FIN/RST）；
2. WiFi 切换、VPN 开关、漫游换 IP（写侧 ECONNRESET 或卡写）；
3. Hub 进程重启（笔记本未休眠，进程退出再拉起）；
4. 手机自身的网络切换与后台 tab 挂起。

本文**不**覆盖：Hub 数据目录损坏/丢失、Hub 换机迁移、多 Hub 双活（单人系统
永远只有一个活 Hub）、captive portal（需要物理触达笔记本，所有者让步不覆盖）。

---

## 2. 现状调查：Hub 不可达时今天实际会发生什么

> 本节回答任务最关键的问题。结论先行：**Node 本地持久 journal + watermark 重放
> 是真的，且做得比「队列」扎实；但「过夜缓冲」作为容量承诺不存在，重连退避对
> 同时唤醒偏窄，手机端没有任何陈旧态信号，Hub 侧没有退让帧。** 下面逐条给证据。

### 2.1 出站 WSS：谁断开连接、怎么重连、怎么退避

【现状】Node 出站 WSS 的活动期由一个 session 任务持有
（`crates/remuda-node/src/transport/wss.rs:640` 起的 `session_task`）。断连检测
有三条路径，全是**被动**的：

* 读侧收到关闭/错误：`wss.rs:874-884`（`recv_ws` 返回 `None` 或 `Err` 即重连）；
* 15 s 一次的应用层心跳写失败：`wss.rs:791-811`，心跳间隔在 `WssConfig` 默认
  15 s（`wss.rs:91`），Hub 租约 TTL 60 s；
* 任意其它帧写失败：`wss.rs:807-811`、`847-851` 等多个 select 臂，写失败即
  进入同一个 `reconnect()`。

【现状】**没有主动 WSS Ping**。Node 只**回** Ping 不发 Ping
（`wss.rs:529-531` 的 `Message::Ping(payload) => Pong`）；Node 与 Hub 两侧都
搜不到主动 `Message::Ping` 发送，也搜不到 `SO_KEEPALIVE` 设置。后果：合盖后
TCP 半开时，**在下次心跳写失败之前，读侧可能长时间没有任何信号**；空闲连接
（没有 journal 流量时只有 15 s 心跳）最坏要等一个写周期才发现链路已死。

【现状】重连是**无限循环**：`reconnect()` 的退避循环是 `loop {}`
（`wss.rs:1288-1343`），dial 失败或 hello 失败都只累加 attempt 后再来，没有
放弃出口，也没有「请稍后再试」的服务端信号——Hub hello 处理器对成功认证的
Node 一律接受（`crates/remuda-hub/src/ws.rs:437-523`）。

【现状】退避参数（`crates/remuda-node/src/transport/mod.rs:54-68`）：
`initial = 1s`、`max = 30s`、jitter = ±25%（`mod.rs:66-68`），指数倍增
（`mod.rs:76-83` 的 `delay`），确定性 jitter（`mod.rs:86-105`）。两个调用点的
种子都**不是每主机随机**：session 任务用帧 id 计数器
（`wss.rs:1291` 的 `ids.load(...)`），daemon 外层监督用进程 pid
（`crates/remuda/src/cmd/node/daemon.rs:166`）。同一时刻被唤醒的 N 个 Node
首次重连全部落在 0.75–1.25 s 的窄窗里——这是重连风暴的结构性来源（见 §4）。

【现状】**命令永不重放**：重连前的 warn 明写
`"hub wss reconnecting; commands are not replayed"`（`wss.rs:1292-1296`）；
重连时在途 journal append 全部以 `Disconnected` 失败
（`wss.rs:1275` 调 `fail_pending`，定义在 `wss.rs:1244-1256`），在途对象拉取
与 api.* 流同样被失败（`wss.rs:1277-1286`），api 流的监听侧据约回 503。

补充：daemon 外层监督在**首次** dial/hello 就失败（比如开机时 Hub 还没上线）
时走另一条退避——记录日志、放开 lease、睡一个 `jittered_delay` 再整轮重来
（`crates/remuda/src/cmd/node/daemon.rs:162-170`）。daemon 进程本身不退。

### 2.2 agent 进程与 daemon 是否继续跑

【现状】**继续跑**。两层证据：

* 出站连接只是 daemon 持有的一个可撤销 lease；daemon 本体是本地 Unix socket
  监听器 + 组合根，`run_daemon_runtime_listener` 的生命周期不绑定任何 Hub 连接
  （`crates/remuda-node/src/daemon.rs:441-463`）。连接断开只 drop transport，
  注释明言组合根继续负责实例进程（`daemon.rs:344-349`）。
* daemon 外层监督在 outbound worker 退出时只记一条
  `"outbound worker stopped; local daemon controller remains available"`
  （`crates/remuda/src/cmd/node/daemon.rs:49-52`），随后下一轮重连。

【现状】Hub 侧在 socket 结束时把 host 置 offline
（`crates/remuda-hub/src/ws.rs:385-392` 调 `mark_host_offline`，
`crates/remuda-hub/src/store.rs:2010-2024`）；host 离线超过 **600 s** 宽限后，
reaper 把该 host 上仍非终态的实例**在 Hub 侧**置为 `exited / host-lost`
（`crates/remuda-hub/src/store.rs:2027-2041`，宽限默认
`crates/remuda-hub/src/config.rs:142-144` 的 `600_000` ms，每 1 s 扫一次，
`crates/remuda-hub/src/lib.rs:676-691`）。注意这是 **Hub 投影**，不停 Node 上
的进程；Node 在 600 s 内重连并重新 hello 后，journal 续传与实例对账照常
（hello 路径里的 epoch 对账 `crates/remuda-hub/src/ws.rs:944-1007`）。超过
600 s 才重连的边界行为（Hub 已写 exited、进程其实还活着）今天依赖后续 journal
生命周期事件把投影修回来——本文不改变它，但记入 §3 的容量目标：**缓冲目标必须
显著短于 600 s host-lost 宽限**，让正常空窗永远不碰这条边。

### 2.3 空窗期的 observation：缓冲在哪、重放还是丢失

【现状——这条是真的】**observation 在转发前就已经落 Node 本地持久 journal，
空窗期产生的 observation 在磁盘上，不在内存队列里。** 介质是每 Node
`<data_dir>` 下的 SQLite（WAL 模式）+ JSONL：打开时
`crates/remuda-journal/src/store.rs:460-467`（`journal_mode=WAL`、
`synchronous=NORMAL`，fsync 策略 `FsyncPolicy::Data`，Node 侧入口
`crates/remuda-node/src/store.rs:450-455`）；每条事件先写 JSONL 再提交 SQLite
索引（`crates/remuda-journal/src/store.rs:618-620`），seq 是该实例本地单调
watermark +1（`remuda-journal/src/store.rs:596`），事件表
`PRIMARY KEY (instance_id, seq)`（`remuda-journal/src/store.rs:476-484`），
写完广播给本地订阅者（`remuda-journal/src/store.rs:623`）。

【现状】重放是 **tail-only 游标 + 有界分页**，不是「从头再发」：

* WSS 路径每个实例一个 pump（`crates/remuda-node/src/transport/wss/runtime_wss.rs:791-815` 的 `ensure_pump`），
  按 `FlushCursor` 从 Hub 缺的 seq 读页，页大小 256
  （`crates/remuda-node/src/transport/wss/runtime_wss.rs:824` 的
  `REPLAY_PAGE`），在途窗口 16（`runtime_wss.rs:821` 的 `UPLINK_WINDOW`）；
  flush 计划只信「游标 vs journal durable seq」，不信生命周期
  （`crates/remuda-node/src/journal_flush.rs:137-148`）。
* 重连 hello 时 Hub 回它已 durable 的 watermark，Node 据此把游标对齐/回退
  （daemon 桥路径 `crates/remuda-node/src/daemon.rs:794-815` 的
  `resume_watermarks`；WSS 路径 hello 后 `apply_resume_watermarks` +
  `resume_runtime_journals`，调用点 `wss.rs:1325-1328`）。**Hub 报低了，
  游标就回退重放**——游标只是优化，journal 才是权威
  （`journal_flush.rs:11-23` 的模块说明）。
* 在途帧断连后不靠内存重发：pump 的转发失败会置 `needs_replay`，由
  `FlushBackoff` 梯子（250 ms 起、5 s 封顶，`journal_flush.rs:34-37`）重试，
  重试的仍是「从游标读 journal」（`runtime_wss.rs:879-960` 的 `pump_live`）。

【现状】已有的**内存**背压只有两处，都只管「转发快慢」、不管「磁盘多少」：

* session 的 journal mpsc 容量 32（`wss.rs:46` 的 `DEFAULT_JOURNAL_QUEUE`，
  装配点 `crates/remuda/src/cmd/node.rs:202` 与
  `crates/remuda/src/cmd/node/daemon.rs:130`）；`JournalSender::push` 在队列
  满时 `send().await` 等（`wss.rs:185-203`），有单测断言第二个 append 会等
  （`wss.rs:1396-1427` 的 `journal_queue_applies_backpressure`）。
* NDJSON daemon 桥的在途 pending 上限 16
  （`crates/remuda-node/src/daemon.rs:852`），读页大小随之收窄
  （`daemon.rs:865` 的 `16 - pending.len()`）。

【现状——必须照实写】**「过夜缓冲」（60 分钟目标）在代码里不存在为承诺。**
具体说：

* journal **没有任何大小/时长水位、没有回收/轮转/配额**——全 crate 搜不到
  prune/retention/max-size 一类机制；它会一直长到磁盘写满。写满时
  JSONL/索引写返回错误，沿 `append` 上抛（本地写入口
  `crates/remuda-node/src/store.rs:369-381` 把 journal writer 的错误直接变成
  `NodeError::Driver`）。**今天没有「写满后怎么办」的产品语义**：没有软告警、
  没有 driver 侧背压策略、没有「拒绝新会话」的码。
* 空窗期 session 任务正在 `reconnect()` 里 sleep 时不收 journal_rx，32 个
  mpsc 槽填满后，**转发侧**的 pump 会挂住（上面说的内存背压）；但本地
  observation 写 journal 走的是 journal writer 的独立通道，不经过这 32 槽——
  所以「记录继续落盘、转发排队等 Hub」成立，「磁盘有界」不成立。
* 唯一的硬失败契约在 **NDJSON daemon 桥控制器**：有未确认 append 且
  30 s 没等到 ACK，控制器直接报错退出
  （`crates/remuda-node/src/daemon.rs:670-672` 的
  `journal acknowledgement timed out; reconnect required`），ACK 对不上所发
  seq 同样是硬错误（`daemon.rs:710-714`）。WSS session 路径没有这把 30 s
  闸——它靠无限重连 + 游标重放收敛。**§3 不削弱这条硬契约。**

### 2.4 seq 会不会断层或重复

【现状】Hub 侧 append 对每条事件按显式 seq 落 `PRIMARY KEY (instance_id, seq)`
（`crates/remuda-hub/src/store.rs:4863-4870`），共享的单行落库逻辑
`append_loaded_event`（`store.rs:8116-8180`）区分三种情况：

* 该 seq 已存在 → 视为 replay，回旧行且 `replayed=true`，durable 游标不动
  （`store.rs:8124-8135`）；重放行不再发布到 follow bus、不发推送
  （ws append 批处理只对 `!appended.replayed` 的行 `publish_journal` +
  `alerts::observe`，`crates/remuda-hub/src/ws.rs:643-648`）；
* 新行 seq 不等于期望 seq → **gap 硬错误**
  `journal gap: expected {k}, got {seq}`（`store.rs:8136-8141`）；
* 正常连续 → 插入并推进游标（`store.rs:8155-8168`）。

【现状】Node 侧认识两类幂等响应：`journal duplicate seq {seq}` 与
`journal gap: expected …, got …`（`wss.rs:1367-1388` 的 `already_durable`）。
照实记一笔：**当前 Hub 代码不会发出 `journal duplicate seq` 这个字符串**——
全仓搜索它只出现在 Node 的兼容分支与 Node 自己的测试里（`wss.rs:1370`、
`wss.rs:1479-1482`），现存 Hub 的等价路径是上面的「旧 seq 命中 →
replayed=true」。Node 这个分支是与旧 Hub 对线留下的兼容代码。

【现状】所以空窗语义是：**不丢（磁盘权威 + watermark 重放）、不重影（旧 seq
重放不发布/不推送）、不容忍洞（gap 报错）。** 断层只可能来自「Node 本地
journal 自身缺失」（数据目录损坏/换机），那不在本文范围。副作用幂等
（interaction wake / push / egress 重装在重放窗被抑制）今天只做了一半：
journal 重放不重复发布（上条），但 egress 重装是 hello 后**无条件重发**的
（`crates/remuda-hub/src/ws.rs:341-345` →
`crates/remuda-hub/src/api_relay.rs:426-453`），它靠 install 的幂等性而不是
「按 watermark 抑制」。

### 2.5 空窗期抛出的 interaction 有没有人收得到

【现状】Node 侧 interaction 从 broker 出来后先**落本地 journal**
（`crates/remuda-node/src/interactions.rs:153-168` 的 pump 调
`commit_broker_observation` → store append），与 observation 同一条持久路径，
所以空窗期的 interaction.requested 同样在磁盘上、重连后随 journal 重放到达
Hub（§2.3/§2.4）。

【现状】但「用户及时看到」有两个真实缺口：

1. **Node 本地等待有 15 分钟 TTL**：broker 默认 TTL
   `crates/remuda-driver/src/interaction.rs:20`（`15 * 60` s），sweeper 周期
   30 s（`crates/remuda-node/src/interactions.rs:23`），到点由本地 sweeper 置
   过期并 journal 一条 interaction.expired（driver 侧 deadline 判定
   `remuda-driver/src/interaction.rs:332`、`384`）。Hub 空窗超过 TTL 时，
   agent 不会真的等下去——它在 Node 侧就被放行了（过期语义由 driver 定义）；
   重连后 Hub 收到的是「请求 + 已过期」两条。
2. **Hub 的 interactions 表没有 deadline/expiresAt 列**
   （`crates/remuda-hub/src/store.rs:4912-4924`）：`apply_interaction_event`
   收到 requested 就置 pending、收到 expired 才置 expired
   （`store.rs:8300-8339`）。所以空窗后迟到的 requested **不会在到达时被判
   过期**，会先进 pending；同一重放批里稍后到达的 expired 再把它改过去。期间
   推送/角标按逐条 journal 事件触发（requested 触发
   `crates/remuda-hub/src/alerts.rs:203-215` 的 `Need your input` 推送）。
   「到达即过期」的诚实子态（计划里写过 `Expired.OnArrival`）**代码里没有**。

【现状】手机 badge 是 Hub 上 pending interaction 的**持久计数**，不是连接状态
（`crates/remuda-hub/src/alerts.rs:172-181`，badge 随每条推送下发）。空窗期
badge 不增长（Hub 根本不知道），重放追平后才可能变化——这是对的，本文不改
（D-049 (5)，见 §5.5）。

### 2.6 手机端：Hub 缺席时看到什么

【现状】`/v1/follow` 只发四类东西：订阅快照、`type:"event"`、
`type:"gap"`、tty 快照/二进制；**没有空闲心跳帧**
（`crates/remuda-hub/src/ws.rs:1288-1406` 的 `follow_session`，事件帧
`ws.rs:1381-1388`，gap 仅在背压/广播 lag 时由 `resync_after_gap` 发出，
`ws.rs:1390-1399` 与 `ws.rs:1809-1824`）。Hub 不主动 Ping（同 §2.1）。
所以手机**无法区分「连接活着但没事件」与「连接已半开死掉」**：socket 没关、
又没新事件时，页面看上去一切正常，最后一屏旧数据被当成当前状态显示。

【现状】follow 总线是有界广播，默认 256 槽
（`crates/remuda-hub/src/ws.rs:181-197`，默认
`crates/remuda-hub/src/config.rs:175-177`），慢消费者 lag 后收到 gap → 客户端
重拉快照（Hub 侧 `ws.rs:1399` / `ws.rs:1809-1824`；工作区 follow 客户端把
gap 直接映射成 refresh，`web/src/features/workspaces/follow.ts:16`）。这是
**背压降级**的现成形状，§3 沿用。

【现状】两个 web 客户端对关闭的处理不一致，都没有陈旧态：

* 工作区 follow：关闭后固定 1 s 重连
  （`web/src/features/workspaces/follow.ts:25-27`）；
* 会话 follow（`eventsSubscribe`）：建 socket
  （`web/src/lib/api.ts:1586-1592`），只听 `message`/`error`，**没有 close
  重连**（`web/src/lib/api.ts:1590` 起的订阅段内无 close 处理），也没有任何
  live/stale 指示。

【现状】需要区分两种「不可达」：(a) **手机↔Hub 不可达**（笔记本休眠/手机换
网）——REST 与 follow 同时失败；(b) **Hub 在、目标 Node 离线**——REST 照常，
host 行为 `offline`、实例行有 connectivity 投影（`store.rs:2016-2020`）。
今天 UI 没有把 (a) 显式画成连接态，(b) 只散落在各行。第 5 节规格的四态针对
(a)，并要求 (b) 用独立的「Node 离线」语义表达，二者不得混用。

### 2.7 Hub 恢复后的收敛顺序（代码里有）

1. TCP 重拨 + `node.hello`，携带本地 watermark（WSS
   `wss.rs:1300-1329`；daemon 桥 `daemon.rs:596-599`）；
2. Hub 认证（argon2id，8 MiB/1 pass，`crates/remuda-hub/src/auth.rs:26-35`），
   回 `nodeToken`、租约（15 s 心跳 / 60 s TTL，
   `crates/remuda-hub/src/ws.rs:1246-1247`）与该 host 全部实例的 durable
   watermark（`ws.rs:523-527` 的 `list_instance_watermarks`）；
3. Node 对齐游标、恢复各实例 pump（§2.3），缺口页追平；
4. Hub 对新行发布事件/推送，重放行静默（§2.4）；
5. hello 同批做实例 inventory 对账（`ws.rs:500-509` 与
   `ws.rs:944-1007`）并重装 egress（`ws.rs:341-345`）。

### 2.8 现状小结：有什么、缺什么

| 能力 | 代码里有 | 只是计划里写过 / 没有 |
| --- | --- | --- |
| 空窗 observation 落盘不丢 | 有：本地 SQLite WAL+JSONL，`remuda-journal/src/store.rs:460-467,596,618-620` | — |
| watermark 幂等追平 | 有：Hub 旧 seq 不重复发布，`store.rs:8124-8135`、`ws.rs:643-648` | — |
| 断连后进程继续跑 | 有：daemon/组合根不随连接退出，`daemon.rs:344-349,431-453` | — |
| 无限重连、命令不重放 | 有：`wss.rs:1288-1343,1292-1296` | — |
| 30 s ACK 硬失败契约（NDJSON 桥） | 有：`daemon.rs:670-672` | — |
| 主动 Ping / keepalive | — | 无：只回 Pong，`wss.rs:529-531` |
| 服务端重连退让 | — | 无：hello 无 retry_after，`ws.rs:437-523` |
| 宽 jitter / 每主机随机种子 | — | 无：±25%，种子是计数器/pid，`mod.rs:66-68`、`wss.rs:1291` |
| journal 容量水位 / 「过夜」承诺 | — | 无：无配额无回收，写满错误上抛，`node/src/store.rs:369-381` |
| interaction「到达即过期」 | — | 无：Hub 表无 deadline，`store.rs:4912-4924,8300-8339` |
| 手机 live/stale/断开/恢复四态 | — | 无：follow 无心跳帧，`ws.rs:1288-1406` |
| 断开时拒建新会话 | — | 无：会话 follow 甚至不重连，`web/src/lib/api.ts:1586-1592` |
| egress 载荷 Node 公钥加密 | — | 无：明文经 Hub→Node，`api_relay.rs:328-351`（今天无第三方） |

---

## 3. 【规格】缓冲：介质、水位、超限、背压、follow 降级

### 3.1 不变量（任何实现必须先满足）

1. **observation 先本地持久、后转发。** 现状架构（§2.3）保持：journal
   SQLite+JSONL 是权威，转发游标只是优化。任何重构不得把「先入内存队列、
   连上再落盘」引入热路径。
2. **不静默丢弃。** 水位触顶只能触发背压、降级或显式拒绝，绝不丢事件
   （D-035）。gap 仍然是硬错误（§2.4）。
3. **硬失败契约不削弱。** NDJSON 桥的 `journal acknowledgement timed out;
   reconnect required`（`daemon.rs:670-672`）保留；水位治理是它**之外**的
   软层，不替代它。
4. **缓冲目标显著短于 host-lost 宽限。** 容量默认按「≥60 分钟空窗」设计，
   而 host-lost 是 600 s（§2.2）——二者不是一回事：60 分钟目标保证**数据**
   可追平；超过 600 s 的失联 Hub 投影会先 exited，追平后由生命周期事件修复，
   本文不承诺该边角的 UI 连续性。

### 3.2 介质与计量

* 介质沿用 `<data_dir>` 的 SQLite WAL + JSONL（不新增队列系统）。
* Node 新增一个只读计量器：周期性统计 journal 目录字节数与最老未确认 seq 的
  年龄（= 本地 durable seq − 已被任一连接 ACK 的 watermark 之间事件的时间
  跨度）。计量只读数既有文件与 watermark，不扫描事件内容。
* 容量是**可配置**的，两维度同时生效：`journal_buffer_bytes`（默认值由目标
  反算，见 3.3）与 `journal_buffer_target_seconds = 3600`。

### 3.3 水位与行为

| 水位 | 判据 | 行为 |
| --- | --- | --- |
| 正常 | 字节 < 60% 且最老未确认事件年龄 < 目标 60% | 无变化 |
| 软满（60%） | 任一维度过线 | 发一次 `journal.softfull` 结构化日志/指标（含两维度读数）；不改变转发与受理 |
| 高水位（90%） | 任一维度过线 | (a) follow 降级（3.5）；(b) 转发排队时 interaction/lifecycle 小帧优先于 blob 大帧；(c) 新会话创建在 Node 侧收到非致命 `node-buffer-high` 提示（可继续） |
| 触顶（100%） | 本地 journal 写返回 ENOSPC/配额错 | **运行中的实例**：该实例标 failed 并 journal 一条 `journal-full` 诊断后停写（写不进去就不能假装在跑）；**新建会话**：`instance.create` 在 Node 侧以 reason `node-buffer-full` **拒绝**；daemon 与控制面不退 |

* 「≥60 分钟」不是拍脑袋的数字承诺：默认 `journal_buffer_bytes` 必须在 Node
  指标里按该机型观测到的 p99 字节/分钟反算并展示（「当前容量可缓冲 N 分钟」），
  操作员可见、可调；文档与 UI 一律呈现这个**实测分钟数**，不呈现无条件的
  「可过夜」。
* 不做自动删除/压缩旧 journal（回收会破坏 gap 自由与重放权威）；释放空间只
  能靠 Hub 追平后由操作员显式 purge 已终态实例（沿用既有实例删除路径）。

### 3.4 driver 侧背压

* 现状的两层内存背压保留语义：转发队列满时等待（`wss.rs:185-203` 的 mpsc
  32、`runtime_wss.rs:821` 的窗口 16、daemon 桥的 16/页
  `daemon.rs:852,865`）。它们阻塞的是**转发泵**，本地 journal 写入与 agent
  进程不被这两层阻塞。
* 新增的背压只允许作用在 §3.3 表里列出的动作上；**不允许**因为 Hub 不可达而
  暂停 agent 进程或卡住本地 journal 写（那会把「Hub 缺席」升级成「任务失败」，
  违背 D-019）。
* 转发恢复后仍走游标重放 + `FlushBackoff` 梯子（`journal_flush.rs:34-37`、
  `runtime_wss.rs:879-960`），不为背压另造重放路径。

### 3.5 follow socket 降级

* 沿用既有「慢消费者 → gap → 重拉快照」形状（`ws.rs:1390-1399`、
  `ws.rs:1809-1824`）。新增一个 Hub→手机的 `follow.degraded` 通知帧，当该
  Node 处于 §3.3 高水位、或 follow 队列持续背压时发出，携带
  `{reason, lastSeq}`；客户端收到后：停止渲染流式增补、展示「缓冲中/降级」
  提示，保留最后一屏但标注陈旧（措辞随 §5 的状态机），需要时按 lastSeq 拉
  快照恢复。
* 降级期间**不得**省略 interaction/lifecycle 事件——它们是审批与会话状态的
  权威信号；可降级的是大体量增补（屏幕、长转录、图片占位）。
* 与第 5 节的关系：`follow.degraded` 表达「Hub→手机这一屏数据不完整」，
  `stale` 表达「手机↔Hub 连接不新鲜」；两者可同时出现，UI 上取更强的一档。

---

## 4. 【规格】重连风暴：服务端退让 + 拉宽退避

### 4.1 场景

N 个 Node 在同一事件后同时唤醒（笔记本恢复、上班时间、Hub 重启完成）。一次
hello 的真实成本不低：argon2id（8 MiB/1 pass，`auth.rs:26-35`）跑在 Hub
**单 writer 线程**的 job 队列里（`crates/remuda-hub/src/store.rs:1541-1558`，
所有写 job 串行，慢于 1 s 才 warn，`store.rs:73`），其后还有 inventory
落库、watermark 查询（`ws.rs:482-522`）。现状首窗 0.75–1.25 s（§2.1），N 个
hello 在单 writer 前排成一串，尾延迟 ≈ N × 单 hello 串行时间。

### 4.2 Hub 侧：hello 退让帧

* hello 处理器新增一个**软负载**判断（滑动窗口统计进行中的 hello 数与最近
  hello 平均耗时，计数器只在内存）。超阈值时，hello **仍完成认证**（不重复
  收 argon2），结果帧里带 `retryAfterMs`，语义为「你已认证，但请 T 毫秒后再
  完成注册/上报，期间不要发 journal」；Node 读到后不建立活动链路、按 T 退避
  重开连接。
* 复用既有 `retryAfterMs` 约定（Hub 已有 NodeBusy 退让先例，
  `crates/remuda-hub/src/error.rs:92-103`、`crates/remuda-hub/src/transport.rs:195-209`），
  不新增 HTTP 状态码、不在认证成功前拒绝（避免给未认证洪泛放大成本）。
* T 由 Hub 按当前在途 hello 数计算（建议 `T = clamp(在途数 × 均值/目标并发,
  2s, 30s)`，实现时定为常量公式并加单测）。
* 这是**退让不是拒绝**：不影响租约 TTL、不把 host 置 offline。

### 4.3 Node 侧：退避与 jitter

* 首窗改为 **2–8 s 均匀随机**（不再是 1 s ±25%），倍增到上限 **60 s**：
  替换 `crates/remuda-node/src/transport/mod.rs:54-68` 的默认值。
* jitter 种子改为**每主机持久随机**：用 host_id 与每进程 CSPRNG 的混合，一次
  进程生命周期内稳定；不许再用帧计数器（`wss.rs:1291`）或裸 pid
  （`crates/remuda/src/cmd/node/daemon.rs:166`）——它们让同批唤醒者对齐。
* 收到 §4.2 的 `retryAfterMs` 时：下一次 dial 延迟取
  `max(指数窗, retryAfterMs)`，并把 attempt 视作仍在递增（不重置到首窗）。
* 保持无限重连与「命令不重放」两条现状语义不变（`wss.rs:1292-1296`）。

### 4.4 多 Node 同时唤醒的尾延迟估算方法

实现验收必须给出可复算的估算，而不是「感觉够宽」：

1. **解析上界**：第 i 轮尝试窗为 `W_i = min(8·2^i, 60)` 秒，N 个唤醒时刻
   t≈0 的 Node 在每轮的 dial 时刻是该窗内 i.i.d. 均匀分布；单 Hub 的 hello
   有效服务率 μ（个/秒，由 argon2 + 写 job 实测均值倒数给出），容量 c 个
   并发认证。
2. **确定性模拟**：用每主机固定种子生成 N 个调度序列，模拟「到达 → 容量 c
   队列 → 服务 μ → 满则收 retryAfterMs 重新入下一轮」，报告 p50/p95/p99
   全部 hello 完成时间。模拟器是纯函数、可单测（给定种子表断言 p99 区间）。
3. **验收口径**：以「目标 fleet 规模 N（实现时在测试里取 N=64）、单 writer
   实测 μ」为输入，p99 完成时间必须小于 3 个 60 s 窗；不满足就调首窗/容量，
   不靠把上限顶到无穷大。
4. 现场没有 N 个 Node 时，μ 用 hello 路径的微基准（argon2 验证 +
   inventory 写 + watermark 读的串行耗时）代入，数字与方法一起进文档。

---

## 5. 【规格】诚实陈旧态：live / stale / disconnected / recovering

### 5.1 信号源（先补心跳，状态才可实现）

* Hub 对 `/v1/follow` 新增 **`follow.tick`** 帧：无内容变化也发，周期
  **5 s**，载 `{hubTime, lastSeq?}`。这是手机端计时锚。现状没有这种帧
  （§2.6，`ws.rs:1288-1406`）。
* 手机端同时使用三个信号：最近一帧（tick 或 event）距今时间、socket
  open/close/error、REST 探测（仅在 socket 关闭后做，避免常态轮询）。
* tick 5 s 的理由：它必须明显小于 stale 阈值，留出 2 次漏拍余量；又不能太
  密——移动无线与后台 tab 下定时器会被节流，5 s 是「15 s 判 stale 仍只需
  3 拍」与「电量/流量可接受」的折中。Hub 不主动 Ping 的现状缺口（§2.1）由
  Node 侧连接治理另行处理，本文只要求手机不依赖 WSS Ping 做状态（后台
  Service Worker 里 Ping 不可靠）。

### 5.2 四态判据

| 状态 | 判据（全部满足） | 屏幕含义 |
| --- | --- | --- |
| `live` | socket open 且最近一帧 ≤ **15 s** | 数据即当前 |
| `stale` | socket open 但最近一帧 > 15 s 且 ≤ **45 s** | **数据是旧的**：顶部常驻琥珀条「连接陈旧，以下内容可能不是最新（已落后 N 秒）」，禁止把旧帧当新状态渲染（如禁用「正在输入/运行中」动态指示，保留静态最后一屏并打上时间戳） |
| `disconnected` | 最近一帧 > 45 s；或 socket close/error 后一次重连失败；或连续 3 次重连失败 | 顶部红色条「已与 Hub 断开」；所有只读内容明确标为离线缓存；所有写操作入口禁用并解释 |
| `recovering` | 曾 disconnected，socket 重新 open 但快照/gap resync 未完成 | 顶部蓝色条「正在恢复…」；resync 完成（收到快照且 seq 连续）前**保持旧数据标注**，完成后转 live；resync 失败回 disconnected |

* **15 s 的理由**：Node 心跳/租约广告本来就是 15 s
  （`ws.rs:1246` 的 15_000 ms、`wss.rs:91`），同一数量级下手机 stale 与
  Node 租约语义对齐；tick 5 s 时 15 s = 连续 3 拍无消息，正常抖动（一拍延迟
  + 一次追帧）不会误报，而合盖/断网在约 15 s 处被诚实指出。
* **45 s 的理由**：3 倍 stale 窗，覆盖一次漫游/DHCP/无线重关联的典型耗时；
  超过 45 s 仍没恢复，用户应当假定「此刻发出的任何指令都到不了」，而不是
  继续盯着一个像是活着的界面。两个阈值都必须做成客户端常量并允许后续按
  实测调整，不允许服务端悄悄改而客户端无感知（tick 可携带建议值，但默认
  判定在客户端常量里）。
* 后台 tab：定时器被节流时以 socket close/error 与重新可见时的首帧时间为准；
  从后台回前台立即按最近帧年龄判一次态，不等下一拍。

### 5.3 最高优先级规则：绝不把旧数据当新数据

* 任何「运行中/等待审批/最后更新于现在」的动态呈现都必须绑定**新鲜连接**；
  stale/disconnected/recovering 下只允许呈现带时间戳的静态最后一屏。
* 不允许用乐观动画、spinner 或「重发中」提示把旧内容伪装成实时内容。
* 追平恢复时如果发现最后一屏与快照有冲突（例如会话已在别处结束），以快照
  为准并明确提示刷新差异，不做静默合并。

### 5.4 disconnected 下拒绝新建会话（D-035 的自然延伸）

* `disconnected`（以及 `recovering` 未完成）状态下，手机端**不提供**新建会话
  的提交入口：Compose 发送键置灰，点击给出说明，不写 outbox、不静默入队。
* 必需文案（语义，不是自由发挥）：**「当前无法连接 Hub，笔记本可能已休眠或
  离线。会话不会在离线时排队——恢复连接后再发送。」** 文案必须同时传达三件
  事：现在不可达、可能原因（笔记本侧）、不会假装已发出。
* REST 写请求遇到网络层失败（不是 Hub 返回的业务拒绝）时，同样按此口径呈现，
  不得本地乐观创建一行「发送中」会话。Hub 在线但目标 Node 离线属于另一种
  拒绝（既有 host/placement 语义），文案分开，不混成断线。
* 依据：所有者裁定的让步是「手机控制时笔记本必须在线」；静默入队会制造
  「以为已发出」的错觉（D-035 refuse-never-reroute 的同族原则；D4 已记录
  所有者选择直接拒绝）。

### 5.5 badge 语义不变

* app badge 继续只表示 Hub 上 pending interaction 的持久计数
  （`alerts.rs:172-181`，D-049 (5)）：live/stale/disconnected 都**不**改
  badge，不用通知条数或连接状态冒充审批数，Badging API 不可用就什么都不做。
* 连接状态只存在于 §5.2 的界面状态条，不进入推送载荷。

---

## 6. 【规格】egress 载荷保护：一条不变量

**不变量**：`EgressSnapshot` 这类携带 provider 上游密钥的载荷
（`crates/remuda-hub/src/api_relay.rs:130-135` 的结构体，装载点
`api_relay.rs:305-360`——从密钥库取明文 `api_relay.rs:328` /
`api_relay.rs:2011-2014`，作为 `authToken` 发出 `api_relay.rs:351`），
**不得以明文经过任何既不是 Hub、也不是目标 Node 的第三方**。

* **今天为什么成立**：Node 出站直连 Hub，`api.egress` 只在 Hub→目标 Node
  方向发一跳（`api_relay.rs:426-453` 的重连重装也是同一跳），链路上没有
  第三方；密钥不到 worker Node、不进 `api.open`、不进 journal
  （[api-routing.md](./api-routing.md) §6/§7 已定）。
* **在册的做法（规格，未实现）**：Node 在 enroll 时登记一把封装公钥
  （与未来 Hub 身份体系共用登记渠道，本文不展开身份设计）；Hub 发
  `api.egress` 前用目标 Node 的公钥把敏感字段（`authToken` 及任何带密钥的
  header）**封成密文**，帧里携带密钥 id 与封装配版；目标 Node 用本地私钥在
  内存中解密后按现有流程装载，落盘形态不变（仍不落盘）。revoke 帧不携带
  秘密，维持明文。这样任何未来的中转形态看到的都只是密文字节。
* **本条的边界**：这是一条不变量与为未来中转预留的密封做法，**不是**对当前
  风险的修复（当前没有第三方），**也不**构成「应当/不应当做公网前门」的任何
  论证——暴露姿态由所有者另行裁定，本文不碰（见文首裁定边界）。
* 验收形态：api-routing §11 记录该不变量；`api_relay.rs` 模块 doc comment
  锚定本节；任何后续改动 `api.egress` 明文跳数的 PR 必须同时改这条不变量并
  经过显式批准。

---

## 7. 明确不做

* 不做公网前门、会合点、任何公网可达路径设计（所有者裁定，见文首）。
* 不做主动 WSS Ping / TCP keepalive / 接口切换 watcher 的手机侧替代——
  half-open 快速检出的连接层治理属于另一条线（Hub 出站形态未定），本文只保证
  手机端不依赖它做诚实状态（§5.1）。
* 不做 journal 自动回收/压缩（§3.3）；不做离线 outbox（§5.4）；不做 HA。
* 不改 wire 主版本：`follow.tick` / `follow.degraded` / hello
  `retryAfterMs` 都是既有帧族的新字段/新通知，旧客户端忽略未知帧即维持
  现状行为。
* 不削弱 30 s ACK 硬失败契约（§3.1）。

## 8. 验收索引

* 现状断言（第 2 节）逐条带 file:line，且 §2.8 表显式区分「代码里有」与
  「只是计划里写过」。
* 规格可实现点：缓冲水位表（§3.3）、退让帧公式（§4.2）、退避参数（§4.3）、
  尾延迟模拟（§4.4）、四态判据与阈值理由（§5.2）、拒建文案（§5.4）、
  egress 密封（§6）。
* 静态检查：`scripts/tests/test_hub_resilience_doc.py` 校验本文 file:line
  真实存在、关键词覆盖（缓冲/退避/jitter/live/stale/disconnected/recovering/
  egress）。
