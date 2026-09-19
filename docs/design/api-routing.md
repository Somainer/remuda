# 模型 API 交付方式：可选 `via:<hostId>` 代理与 `api.*` 带内流类

**状态**：设计已定（D-047 / D-048）；协议类型与线格式已落地，Hub/Node/CLI/web 行为按
api-routing 计划的任务顺序推进。

**决策记录**：[decisions.md](./decisions.md) D-047（交付方式、`--api-via`、拒绝而非
改道、D-031 例外条款）与 D-048（`api.*` 流类）。**线格式**：protocol.md
[§4.4.1](./protocol.md) 与 [§7.6](./protocol.md)。

本文记录设计本身——为什么是这个形状、边界在哪、失败时发生什么。任务拆分与验收
标准不在这里。

---

## 1. 缺口

模型网关凭据是**主机绑定**的，而它保护的 origin 未必从每台机器都可路由。今天的
交付方式（下称 `direct`）把 base URL 与凭据一起发到 worker 主机 W，于是：

* 只在某台机器（下称 H）上可达的模型，**无法**交给另一台机器上的 worker；
* 把凭据发过去也不解决问题——若该 origin 从 W 不可路由，凭据只是白白多了一个
  副本，同时削弱了「secret 只留一台机器」（D-021）。

需要一个**可选**参数，让一次派发的模型 API 请求都从指定机器出去。这台机器可以是
**Hub 主机**（跑 Hub 的那台，通常也是操作员自己 relay 的所在），也可以是**远程
主机**。

## 2. 两种交付方式

```
delivery = direct                  # 今天的行为：baseUrl 与凭据一起送到 W
delivery = via:<hostId H>          # 该会话的每个模型 API 请求都从 H 出去
```

* **profile 级**：`ProviderProfile.delivery`，缺省 `direct`。
* **逐次派发覆盖**：`POST /v1/workers/dispatch` 与 `POST /v1/instances` 上的
  `apiVia`（`<hostId>` | `self`＝Hub 主机 | `none`＝强制 direct）。
* **项目层**夹在两者之间，与既有 provider 瀑布同构。

**瀑布**：请求 `apiVia` > 项目 > profile `delivery` > `direct`。

**决议时机在主机放置之后**（`providers.rs::resolve_and_attach_with_project`），
因为 `via:<H>` 在 `H == W` 时**收敛为 `direct`**：worker 主机自己就是出口主机，
没有东西可代理。收敛只发生在决议时，不改写线上的值。

### 线格式：嵌套，不是字符串

profile 上的 `delivery` 是一个**对象**，因为路由子模式（§4）在一个关键字里无处
安放：

```json
{ "delivery": { "mode": "via", "viaHostId": "hst_…", "route": "auto" } }
```

缺省（也即早于本决策的每一行）是 `{ "mode": "direct", "route": "auto" }`。
CLI 保留人话拼写 `provider set --delivery direct | via:<host> [--route …]`，
由 CLI 负责解析——两种拼法不在同一层，不混为一谈。

`via` 而缺 `viaHostId` 是**解析错误**，不是默认值：它没有指明任何出口机器，
接受它只会把失败推迟到启动时，而那时操作员已经被告知派发已被受理。

**逐次覆盖 `apiVia` 仍是字符串**（`hst_…` | `self` | `none`）：三个值里两个是
关键字，只有一个带 id，包成对象只会让调用方更啰嗦。

## 3. 路由：在带内转发

```
claude on W ──http──▶ 127.0.0.1:<port>   (W 的 Node，每实例监听器)
                         │  api.open / api.body            (Node→Hub JSON-RPC)
                         ▼
                        Hub ──▶ H == Hub 主机: 进程内 reqwest ──▶ gateway
                            └──▶ 否则: api.open 到 H 的 Node ──▶ reqwest ──▶ gateway
                         ▲
                         │  api.head / api.chunk / api.end  (回程 notification)
```

* **W 的 Node** 在启动时为每个实例绑 `127.0.0.1:0`，铸造一枚每实例 bearer
  （32 随机字节），并按今天的形状写 overlay——只有**值**变了：
  `ANTHROPIC_BASE_URL = http://127.0.0.1:<port>/v1`、`ANTHROPIC_AUTH_TOKEN =
  <relay bearer>`、`CLAUDE_CODE_ENABLE_GATEWAY_MODEL_DISCOVERY = 1`。
  只改输入，不改 driver API。
* **监听器只接受**：loopback 对端、精确 bearer（常量时间比较）、存活实例、
  方法属 `{POST, GET}`、路径在该 profile 的 base path 之下。其余一律 403/404
  且不带细节。
* **H 的 Node（或 H 就是 Hub 主机时的 Hub 进程本身）** 是**唯一**加载网关凭据的
  地方。它对着 `profile.baseUrl` 重建请求，用 profile 凭据替换
  `authorization`/`x-api-key`，补上 `profile.headers`，把响应流式送回。
* **白名单，不是隧道**：目标 origin 固定为 `profile.baseUrl` 的 origin，路径必须
  是 base path 下的后缀，请求头与响应头各有白名单（`set-cookie` 丢弃）。
  一条 relay 流只能到达恰好一个 origin、为恰好一个存活实例、只在该实例存活期间。

## 4. 路由子模式：W 与 H 之间可能没有直连路径

operator 的常见拓扑正是如此：H 是 Hub 主机，无公网入口，而 D-031 禁止为它造一个。
因此 `via` 有一个路由子模式：

| `route` | 行为 |
| --- | --- |
| `auto`（默认） | H 配了非 loopback 的 `relayBind` 时，W 先探直连路径（带实例 bearer 的 HEAD/OPTIONS，3 s）；没有配置或探测失败则回落 `hub-relay`。 |
| `hub-relay` | 总是走既有 Hub↔Node 链路（W Node → Hub → H Node，或 H 就是 Hub 主机时由 Hub 进程自己出去）。**永远可行**，也是 W 到不了 H 时唯一可行的一条。 |
| `direct-net` | 要求直连；探测失败就在启动时以 `api-via-unreachable` 拒绝。 |

* 路由**只在启动时决议一次**，并由 Node 作为 `apiRoute: direct-net | hub-relay`
  回显。
* 会话中途直连失败**不静默切换**到 hub-relay：请求失败、实例报
  `blocked{api-route-down}`，操作员重派。
* **H 的 relay 端点默认只绑 loopback**；只有操作员在 host 上显式设置
  `relayBind`（一个明确地址，默认绝不是 `0.0.0.0`，从不自动发现）才绑非
  loopback。两条路径都要求每实例 bearer。

## 5. 拒绝，绝不改道

| 情形 | 行为 |
| --- | --- |
| `--api-via <H>`，H 未知/未注册 | 400 `api-via-unknown-host` |
| H 已注册但离线 | 409 `api-via-host-offline`（在任何名字/端口/worktree 分配**之前**） |
| H 的 Node 太旧不会说 `api.*` | 409 `api-via-unsupported` |
| `route: direct-net` 且探测失败 | 409 `api-via-unreachable` |
| W 的 Node 绑不上监听器 | `instance.create` 带 reason code 拒绝；不启动 |
| H 会话中途掉线 | 在途流收到 `api.end{error:"via-host-offline"}`；监听器答 503 与 Anthropic 形状的错误体；roster 观测变为 `blocked{reason:"api-route-down"}` |
| profile 轮换/修订号变化 | H 按已安装的 `api.egress` 上下文在**新流**重读凭据（Hub 重发 `api.egress`）；中途轮换对下一个请求生效 |

**任何情况下代理会话都不会回落到 `direct`。** 那会把请求——**连同凭据**——推到
操作员明确排除的机器上，并让 UI 说谎（D-035）。所以协议里**没有**任何一个「回落
到 direct」的码：不存在这样的路径，代码里也不允许出现一条。

## 6. secret

网关凭据**只在 H**（或 H 就是 Hub 主机时的 Hub 进程里）加载，**从不**落到 H 的
磁盘上。W 收到的是一枚每实例 relay bearer，在别处一文不值。

这**加强**了 D-021：`host:<hostId>` 作用域的 profile 现在能服务任意 worker，而不
必把凭据放出那台主机。「凭据只留一台机器」的检查对象是 **H**，不是 W。

## 7. `api.*` 流类（D-048）

七个帧，走**既有** Hub↔Node 链路——与 `object.pull` 同一条路径、同一套授权、同一个
1 MiB 帧上限。

| 帧 | 方向 | params |
| --- | --- | --- |
| `api.egress` | Hub→H Node | `{instanceId, profileId, baseUrl, headers[], authToken?, revoke}` |
| `api.open` | W Node→Hub，Hub→H Node | `{instanceId, streamId, method, path, query, headers[], bodyBase64?, bodyChunked, deadlineMs}` |
| `api.body` | W Node→Hub→H Node | `{streamId, seq, dataBase64, last}` |
| `api.head` | H→Hub→W Node | `{streamId, status, headers[]}` |
| `api.chunk` | H→Hub→W Node | `{streamId, seq, dataBase64, last}` |
| `api.end` | 双向 | `{streamId, error?:{code,message}, bytesUp, bytesDown, ms}` |
| `api.cancel` | 双向 | `{streamId, reason}` |
| `api.credit` | 消费者→生产者 | `{streamId, chunks}` |

* **B.2 凭据下发（`api.egress`）。** 凭据**绝不**搭在 `api.open` 上。Hub 在
  `via:<H>` 路由启动决议时、以及 H 每次重连后，向 H 发一条 `api.egress`
  notification，按 `instanceId` 安装出口上下文（`profileId`、`baseUrl`、
  profile headers、`authToken`）；实例退出或路由失效时发 `revoke: true` 清除。
  Node 按 instanceId 安装/清除，`revoke` 后、新 `api.egress` 到达前拒绝该实例的
  新流（`destination-refused`）。凭据只存在于 H 内存、不经过 W、不出现在任何
  数据流帧或 journal 中。

* **独立的 stream 表，不进 RPC pending map。** 七个帧全是 notification，各自维护
  每链路 stream 注册表，因此**永不消耗**那个被 `instance.create`/`tty.*` 依赖的
  32 槽在途 RPC 上限。几条热流卡死控制面是最容易犯的错，所以这是一条硬要求。
* **信用**：Hub→Node 出站队列容量 32 且与 tty 帧共享，所以生产者每流最多 4 个
  未确认 chunk，消费者边排空边发 `api.credit`，块间 `yield_now()`——一条长 SSE
  流不能饿死 tty 帧。
* **合并是必需的，不是优化**：每个 token 一帧在 NDJSON/ssh-stdio 上是病态的。
  H 在 **≥16 KiB 或 ≥50 ms 或流结束** 时合并成一块，严格保序——body 是**不透明
  字节**，不按 SSE 解析。
* **限额**：`maxApiStreams`（默认 8/链路、2/实例）与 `apiChunkBytes`（默认 64 KiB
  原始 ≈ 87 KiB base64，远低于 1 MiB 帧上限）。
* **超时阶梯**：连接 10 s、首字节 60 s、块间空闲 120 s（SSE keepalive）、硬上限
  30 min。越界即向上游 `api.cancel`、向下游 `api.end{error}`，监听器答 `504`。

**为什么不用 channel 2 `BinaryChannel::ObjectChunk`**：与 `object.pull` 的理由完全
相同——ssh-stdio 桥只逐帧转发 JSON 文本，为桥引入二进制隧道会改变桥的**全部**
安全边界。

## 8. 这不是隧道（D-031）

不安装、不探测任何隧道二进制；不用 `ssh -L/-R/-D`；默认配置下不在任何非 loopback
接口上开端口（只有操作员显式设置 `relayBind` 才会）；不能到达任意主机。

字节走的是**已经授权、已经审计**的 Hub↔Node 链路，和今天的 `object.pull` 附件
字节完全一样。被转发的是一个**单一 origin、白名单、实例作用域**的应用请求——与
`host.files.read`、`tty.write` 同类，不是内网穿透。

D-047 把这条例外明确写下，以免日后重新争论。

## 9. journal、usage 与操作员看到的东西

* **journal 从不带 body 或 header**。启动时一条 `apiRoute` 观测
  `{mode, viaHostId, profileId, listenerBound:true}`（无端口、无 token）；每条流在
  `api.end` 时记 `{streamId, status, bytesUp, bytesDown, ms, errorCode?}`——
  只有计数器。
* **usage 仍归属 worker 实例**：既有 adapter 读 transcript/stream 发
  `kind:"usage"` 事件，`usage_events` 行仍写真实网关 `profile_id`，所以供给核算、
  429 park 与预算区间零迁移。H **不**记实例维度的 usage。
* **新信号（严格更好）**：H 观测到真实 HTTP 状态码，把 `429`/`529` 投影进既有
  供给证据路径（`remuda profile event --http-status`），把限流检测从「读屏」升级
  成「真状态码」。
* **Provider 页**增加「交付方式」：`直连` / `经由 <host label>`，profile 行显示
  生效中的交付方式，指定的主机离线时给琥珀色提示。
* **Session 条**在既有 `providerSourceHint` 后面接一个路由子句，例如
  `将使用 relay (host:hst_…) · 经由 <host label> 代理`——**从 Node 回显的
  `apiRoute` 渲染，不是请求的那个**（D-035：记录说的是实跑值，不是被要求的值）。
* `remuda watch` 增加路由列与 `api-route-down` 这一 blocked reason。

## 10. 已知风险与对应

1. **饿死 tty/控制帧**：出站队列 32 且共享。对应：独立 stream 表、每流信用、块间
   `yield_now()`，以及一条在 5 MB relay 流跑着时测 tty p99 的 e2e。
2. **ssh-stdio 上的延迟与膨胀**：base64 +33 %、NDJSON 行框、多一跳 RTT。对应：
   16 KiB/50 ms 合并写进验收标准而不是当作优化；对比 direct 记录 TTFT 与 tokens/s。
3. **H 上的凭据爆炸半径**：H 现在为每一路由到它的会话持有网关凭据。对应：H 上
   只在内存、按 profile 修订号逐流取、`secret_release_allowed` 对着 **H** 检查，
   审计只记 fingerprint/last4。
4. **静默改道是最糟的一类 bug**：从 `via:<H>` 回落到 `direct` 会把请求与凭据推到
   被排除的机器上，并让 Session 条说谎。对应：`apiRoute.mode == "via"` 之后没有任何
   代码路径构造 direct overlay；测试断言启动**失败**而不是改道。
5. **范围蔓延成通用 HTTP 代理**：对应：origin+path 白名单在 Node egress 强制、
   在 Hub 复核，方法白名单，两个方向都有头白名单。
6. **版本偏移**：旧 Node 不认识 `api.*`。对应：能力在 `node.hello` 里声明；需要
   路由到没有该能力的主机时以 `api-via-unsupported` 拒绝，**绝不**降级。
7. **W 上的本地权限边界**：以 worker 用户运行的同机进程能读 0600 overlay 并使用
   监听器。接受并记录：这正是今天网关凭据的暴露面，而 relay bearer 的价值严格
   更低。
8. **在途 RPC 耗尽**：若 `api.*` 走了那个 32 槽 pending map，几条流就会卡住
   `instance.create` 与 `tty.write`。对应：独立 stream 表是硬要求，并有「8 条活流
   不影响 RPC 容量」的测试。
