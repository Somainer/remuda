# Hub 韧性（c-hubresil）：现状调查证据与规格落点

Date: 2026-09-22. Batch: c-hubresil. Scope: docs-only（不改 Rust 实现、不改 wire
版本、不改 store schema；四个 Rust 文件只加 doc comment）。

Design references: [hub-resilience.md](../hub-resilience.md)（本批新增的规格正文，
其第 2 节现状调查与本文互为引用）、[api-routing.md §11](../api-routing.md)
（egress 载荷不变量）、[decisions.md](../decisions.md) D-019 / D-021 / D-035 /
D-049。

本文不截图。所有行号对应当前工作树（doc comment 加入后的行号），可逐条复核；
复核方式是静态阅读与 `grep`，没有启动任何 Node/Hub/手机进程，不联网。

## 1. 调查方法与边界

- 工作树：本批次的 worktree，基线 `origin/main`（最近合并
  `2b9203ff`，c-nodecosign 已并入）。
- 问题清单（任务书第 1 点）逐条回答：(a) 出站 WSS 断开后的重连与退避；
  (b) agent 进程是否继续跑；(c) 空窗 observation 被缓冲重放还是丢失；
  (d) journal seq 是否断层/重复；(e) 空窗 interaction 有没有人收得到；
  外加手机端 follow 形态与 egress 明文跳数。
- 裁定边界：本轮不做公网前门。调查中凡涉及「将来的中转形态」只作不变量记录，
  不展开方案（见规格 §6/§7）。
- 静态校验：`scripts/tests/test_hub_resilience_doc.py`，纯读文件，无网络、无子
  进程执行被测程序（其中一条用 `git diff` 确认四个 .rs 文件只加注释行）。

## 2. 关键发现：代码里有的

1. **持久 journal 是缓冲本体，不是内存队列。** Node observation 先写本地
   SQLite WAL + JSONL（`crates/remuda-journal/src/store.rs:460-467`
   打开/WAL/同步级别，`:596` 本地 watermark+1 赋 seq，`:618-620`
   先 JSONL 后索引，`:476-484` 事件表主键 `(instance_id, seq)`），
   Node 以 `FsyncPolicy::Data` 打开（`crates/remuda-node/src/store.rs:450-455`）。
2. **tail-only 游标重放 + Hub watermark 对齐。** 每实例 pump
   （`crates/remuda-node/src/transport/wss/runtime_wss.rs:791-815`），
   页 256 / 窗口 16（`runtime_wss.rs:821,824`），flush 计划只信游标对 durable
   seq（`crates/remuda-node/src/journal_flush.rs:137-148`），失败梯子
   250 ms→5 s（`journal_flush.rs:34-37`，`runtime_wss.rs:879-960`）；
   重连 hello 回 watermark 后游标可回退重放
   （`crates/remuda-node/src/daemon.rs:794-815`，
   `crates/remuda-node/src/transport/wss.rs:1325-1328`）。
3. **Hub 侧幂等落库。** 旧 seq 命中返回 `replayed=true` 且不重复发布/推送
   （`crates/remuda-hub/src/store.rs:8116-8141`，
   `crates/remuda-hub/src/ws.rs:643-648`）；seq 跳变是 gap 硬错误
   （`store.rs:8136-8141`）；journal 表主键
   （`store.rs:4863-4870`）。
4. **断连不停进程。** daemon 监听器/组合根与 Hub 连接解耦
   （`crates/remuda-node/src/daemon.rs:344-349,441-463`），
   外层监督日志明示 local daemon 继续可用
   （`crates/remuda/src/cmd/node/daemon.rs:49-52`）。
5. **无限重连、命令不重放。** `loop {}` 重连
   （`crates/remuda-node/src/transport/wss.rs:1288-1343`），warn 明写
   commands are not replayed（`wss.rs:1292-1296`），在途 append 以
   Disconnected 失败（`wss.rs:1275` + `fail_pending` `wss.rs:1244-1256`）。
6. **30 s ACK 硬失败契约（NDJSON 桥）。**
   `crates/remuda-node/src/daemon.rs:670-672`；ACK 不匹配同样硬失败
   `daemon.rs:710-714`。规格 §3.1 明确不削弱。
7. **Hub host-lost 是 600 s 后的 Hub 投影。** socket 结束置 offline
   （`crates/remuda-hub/src/ws.rs:385-392` →
   `store.rs:2010-2024`），reaper 宽限默认 600_000 ms
   （`crates/remuda-hub/src/config.rs:142-144`，
   `store.rs:2027-2041`，1 s tick `crates/remuda-hub/src/lib.rs:676-691`）。
8. **interaction 先落 Node journal。** broker pump
   （`crates/remuda-node/src/interactions.rs:153-168`）；本地 TTL 15 分钟
   （`crates/remuda-driver/src/interaction.rs:20`，
   sweeper 30 s `crates/remuda-node/src/interactions.rs:23`）。
9. **badge = 持久 pending 计数。** `crates/remuda-hub/src/alerts.rs:172-181`；
   规格 §5.5 不改。
10. **egress 明文只跳 Hub→目标 Node。** 快照结构
    `crates/remuda-hub/src/api_relay.rs:130-135`，取明文
    `:328`/`:2011-2014`，发出 `:351`，重连重装 `:426-453`。

## 3. 关键发现：只在计划里写过、代码里没有的（照实记录）

1. **「过夜缓冲」不是既有能力。** journal 无大小/时长水位、无回收轮转（全
   `remuda-journal` crate 无 prune/retention 机制）；写满错误经
   `crates/remuda-node/src/store.rs:369-381` 上抛为 `NodeError::Driver`，
   没有产品化的超限语义。内存背压只覆盖转发快慢（mpsc 32
   `crates/remuda-node/src/transport/wss.rs:46`、窗口 16
   `runtime_wss.rs:821`、daemon pending 16 `daemon.rs:852`）。
   规格 §3 把水位、超限、driver 背压、follow 降级先设计再承诺，并要求
   UI 呈现按实测字节率反算的分钟数，不无条件宣称「可过夜」。
2. **无主动 Ping/keepalive。** Node 只回 Ping
   （`crates/remuda-node/src/transport/wss.rs:529-531`）；Hub 侧无主动 Ping；
   两侧无 `SO_KEEPALIVE` 设置（静态搜索无命中）。
3. **退避窗窄且种子确定性。** 1 s/30 s/±25%
   （`crates/remuda-node/src/transport/mod.rs:66-68`），种子分别是帧计数器
   （`wss.rs:1291`）与进程 pid
   （`crates/remuda/src/cmd/node/daemon.rs:166`）；Hub hello 无退让
   （`crates/remuda-hub/src/ws.rs:437-523`，成功认证即受理）。规格 §4
   给 2–8 s/60 s/每主机随机种子与 `retryAfterMs`。
4. **手机无陈旧态。** `/v1/follow` 无空闲心跳帧
   （`crates/remuda-hub/src/ws.rs:1288-1406`），gap 只在背压/lag 时发
   （`ws.rs:1390-1399,1809-1824`）；会话 follow 不处理 close
   （`web/src/lib/api.ts:1586-1592`），工作区 follow 固定 1 s 重连
   （`web/src/features/workspaces/follow.ts:25-27`）。规格 §5 补
   `follow.tick`（5 s）与 live/stale/disconnected/recovering（15 s/45 s）。
5. **interaction 无「到达即过期」。** Hub interactions 表无 deadline 列
   （`crates/remuda-hub/src/store.rs:4912-4924`），落库只按事件状态翻转
   （`store.rs:8300-8339`）；空窗后迟到的 requested 先进 pending 再被同批
   expired 改正。规格未扩表（本轮不改 schema），只在 §2.5 记录该行为与
   Node 侧 15 分钟 TTL 的合成效果。
6. **Node 的 `journal duplicate seq` 兼容分支无现役对端。** 现存 Hub 不发该
   字符串（命中只在 Node 分支与其测试，
   `crates/remuda-node/src/transport/wss.rs:1367-1388`）；现役等价路径是
   Hub 的 replayed 静默（上文第 3 点）。规格 §2.4 如实记载。
7. **egress 公钥密封未实现**（规格 §6 记为不变量与在册做法，明确「不是对当前
   风险的修复」，且不夹带前门论证）。

## 4. 产物与校验

- 新增 `docs/design/hub-resilience.md`（现状 + 四项规格）；
  `docs/design/api-routing.md` 追加 §11（egress 载荷不变量）。
- 四个允许触碰的 Rust 文件仅加模块/类型 doc comment，指向规格：
  `crates/remuda-node/src/daemon.rs`、
  `crates/remuda-node/src/transport/mod.rs`、
  `crates/remuda-hub/src/api_relay.rs`、`crates/remuda-hub/src/ws.rs`。
- `cargo check -p remuda-node -p remuda-hub` 通过。
- `python3 -m unittest scripts.tests.test_hub_resilience_doc`（13 项）与
  `scripts.tests.test_topology_doc`（23 项，未改其管辖文件）全部通过。
- `git diff --stat origin/main` 仅含上述独占文件；无主机名/域名/IP/用户名/
  家目录/token 字面量（静态测试含泄漏扫描）。

## 5. 未决与后续（不在本轮）

- 连接层 half-open 快速检出（主动 Ping/keepalive/接口切换 watcher）依赖 Hub
  出站形态，所有者裁定前不设计；手机侧规格已做到不依赖它做诚实状态。
- 缓冲容量默认字节数需要在目标机型取 p99 字节/分钟后定值；本轮只定水位形状与
  「按实测分钟数呈现」的要求。
- interaction「到达即过期」若要做，需要 Hub 表加 deadline——属于 schema 变更，
  留给后续批次。
