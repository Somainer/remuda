# 附件经 MCP 工具直投 agent（D-028 §4.5）

工作树 `wt/r-mcpattach/mcp-attachment-tool`（从 `origin/main`），2026-09-14。
本轮是**实现 + 自动化证据**：下面每一段 transcript 都由仓库里的测试打印，
可以重跑复核，不是手抄的。没有真机 claude / codex / grok 会话——本轮验证的是
**Remuda 侧的工具、路由与作用域**，三家 harness 的实机验收留给 D-028 的
parity gate（见 §7 未验证项）。

复核命令：

```
cargo test -p remuda      --locked --test mcp_attachment -- --nocapture   # §4 transcript
cargo test -p remuda      --locked --test mcp_schema_golden               # §2 schema golden
cargo test -p remuda      --locked --bin remuda mcp::attachment           # 映射与拒绝分支
cargo test -p remuda-hub  --locked --test attachments                     # §3 Hub 路由
```

---

## 1 为什么要这个工具

D-027 的投递是**按 driver 分层**的：只有 `claude-print` 发真的 base64 image
block，其余 driver 只能在 prompt 里追加一句「请读取 `<绝对路径>`」。代价有两条，
D-028 §4.5 要一次解决：

1. **codex 不一定会去读那个路径**（design §3 标 UNVERIFIED），**grok 根本没有读图能力**
   ——D-027 给 grok 的是一条 journal note「该 agent 不支持读图」。
2. `claude-print` 按 D-028 §12 要**条件退役**。退役之后，如果没有替代，
   *全队没有任何一条路径能把真实图片字节交给 agent*（D-028 §2.2 表格原话）。

MCP 工具反过来：claude / codex / grok 都接受 tool result 里的 `image` content
block，所以**同一次调用在三家都成立**，路径提及降级为 fallback。D-027 的 Hub 暂存、
Node 落盘、限额与 403 规则**一条没动**。

---

## 2 工具 schema（契约）

Golden 固定在 `crates/remuda/tests/mcp_golden/attachment-tools.json`，由
`crates/remuda/tests/mcp_schema_golden.rs` 比对。改名或改参数会让该测试红，
并在失败信息里要求同步更新本文件的 §5 mention 契约。

```json
[
  {
    "name": "remuda_attachment",
    "description": "Fetch one attachment staged for this session by objectId. Images come back as an image content block you can see directly; text comes back as text. Other media types are refused with their size and type.",
    "inputSchema": {
      "type": "object",
      "required": ["objectId"],
      "properties": {
        "objectId": {
          "type": "string",
          "description": "`obj_…` id, as named in the prompt's attachment mention or by remuda_attachments_list."
        }
      }
    }
  },
  {
    "name": "remuda_attachments_list",
    "description": "List attachments staged for this session (objectId, mediaType, size). Call this to discover what `remuda_attachment` can fetch.",
    "inputSchema": {
      "type": "object",
      "properties": {
        "instanceId": {
          "type": "string",
          "description": "Session to list. Omit inside an agent session: it defaults to your own, and naming another session is refused."
        }
      }
    }
  }
]
```

返回形状：

| 情况 | tool result |
|---|---|
| `image/*` | `{"content":[{"type":"image","data":"<base64>","mimeType":"image/png"}],"isError":false}` |
| `text/*` | `{"content":[{"type":"text","text":"<解码后的 UTF-8>"}],"isError":false}` |
| 其它 media type | `isError:true`，文案含 **size + mime**：`attachment obj_… is application/pdf (512 bytes); only image/* and text/* can be delivered as content blocks` |
| 超过 3.5 MiB | `isError:true`，`RESOURCE_LIMIT: attachment obj_… is <n> bytes (image/png); the limit is 3670016` |
| 非本 session | `isError:true`，`… is limited to this Agent instance's own session`（本地拦截）或 Hub 403 |

`remuda_attachments_list` 返回 JSON text block：
`{"instanceId":"ins_…","count":1,"items":[{"objectId","mediaType","size","expiresAt"}]}`。
**刻意不回 `digest` / `name`**：都帮不上 agent 选哪个附件，而 `name`（派生的
`<objectId>.<ext>`）只会诱导它去猜一个本地路径。

### 2.1 尺寸上限 = 3.5 MiB，两处对齐

`MAX_INLINE_ATTACHMENT_BYTES = 3_584 * 1024 = 3670016`，与
`claude_print.rs:MAX_INLINE_IMAGE_BYTES` 同值，Hub 侧
（`crates/remuda-hub/src/attachments.rs`）与 MCP 侧
（`crates/remuda/src/cmd/mcp/attachment.rs`）各查一次。

两道都要，理由不同：Hub 那道是**权威**，不让超限字节离开 Hub；MCP 那道是为了
在对着旧 Hub 时仍能在 agent 自己的 transcript 里给出解释，而不是把一段截断的
base64 当成图片。MCP 侧额外按 base64 长度反算解码大小，所以**谎报的 `size`
也过不去**（`an_oversize_attachment_is_refused_with_its_size_and_type`）。

注意这**低于** D-027 的暂存上限 5 MiB：一个附件可以被成功暂存、却拿不到 inline。
因此错误文案必须点名 size 与 mime——而且该对象**仍然出现在 list 里**，让 agent
看见「它在、但我拿不到」，而不是以为图片丢了
（`an_attachment_over_the_inline_cap_is_refused_with_its_size_and_type`）。

---

## 3 Hub 侧：为什么必须新增端点

D-027 的 `GET /v1/objects/{id}` 有两个合法读者：上传的 operator 设备，和
托管该实例的 Node。**agent 不在其中**——`authorize_read` 走
`require_operator`，而 `require_operator` 对 instance-bound 凭据直接 403。
session 内的 MCP server 拿的正是这种凭据，所以那条路对它是关着的。

新增（additive，未改 `objects.rs` / `http.rs` / `store.rs` 的既有逻辑）：

| 路由 | 用途 |
|---|---|
| `GET /v1/attachments?instanceId=…` | 列本 session 的未过期附件 |
| `GET /v1/attachments/{objectId}/content` | 读单个附件：metadata + base64 |

作用域**比 Agent 面其余部分更窄**：instance 凭据只读**自己 session**的附件，
**不含直接子实例**。理由是附件绑定在某一次 `instance.send` 上，子会话对父会话的
图片没有任何正当主张；同时把 instance token 泄漏的爆炸半径压在「本来就交给这个
session 的那几张图」。`owns()`（self + 直接子实例）在这里**故意不复用**。

中间件 `restrict_agent_routes` 只放行这两个形状，多一段路径就不放行
（`only_the_two_attachment_read_shapes_bypass_the_instance_path_check`）。

返回 JSON 而不是裸字节：唯一的调用方要拼 MCP content block，本来就要 base64；
在这里编码还让「超限」变成一个正常的结构化错误，而不是一段被截断的 body。

审计：两条路由各打一条 `tracing::info!`——`attachment.listed`
（instance / device / origin / count）与 `attachment.read`
（object / instance / media_type / bytes / device / origin），与 D-027 的
`attachment.uploaded` 同一风格，凑齐 **staged → read** 两端。

实测覆盖（`crates/remuda-hub/tests/attachments.rs`，5 passed）：

```
an_instance_credential_reads_its_own_attachment_as_base64        # 含断言：D-027 路由对同一凭据仍 403
another_sessions_attachment_is_refused                           # 兄弟 session 403；匿名 401；未知 id 404
an_attachment_over_the_inline_cap_is_refused_with_its_size_and_type
listing_covers_this_session_and_operators_must_name_one          # operator 不给 instanceId → 400
staging_is_images_only_so_no_text_row_can_reach_the_read_route
```

第一条里有一句刻意的反向断言：同一个 agent token 打 `GET /v1/objects/{id}`
必须仍是 403。**如果哪天它变成 200，说明 D-027 的边界被挖松了，本端点也就失去理由。**

---

## 4 fake-agent transcript（实跑）

`cargo test -p remuda --locked --test mcp_attachment -- --nocapture`。
真 Hub、真 Node WS、真 `remuda mcp` 子进程，持有 session 自己的
`POST /v1/instances/{id}/mcp-token` 凭据（`REMUDA_INSTANCE_ID` + `REMUDA_TOKEN`）。
图片是 168 B 的 PNG（真签名 + 填充）。

```
instanceId = ins_01a09cab-92ce-7776-8298-5fb0631c5eca
objectId   = obj_01a09cab-9393-70e6-9301-ede4cea52540

-> {"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2024-11-05","capabilities":{},"clientInfo":{"name":"fake-agent","version":"0"}}}
<- {"jsonrpc":"2.0","id":1,"result":{"protocolVersion":"2024-11-05","capabilities":{"tools":{"listChanged":false}},"serverInfo":{"name":"remuda","version":"0.1.0"}}}

-> {"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"remuda_attachments_list","arguments":{}}}
<- {"jsonrpc":"2.0","id":2,"result":{"content":[{"type":"text","text":"{\"instanceId\":\"ins_01a09cab-…\",\"count\":1,\"items\":[{\"objectId\":\"obj_01a09cab-…\",\"mediaType\":\"image/png\",\"size\":168,\"expiresAt\":\"2026-09-14T21:28:05.267Z\"}]}"}],"isError":false}}

-> {"jsonrpc":"2.0","id":3,"method":"tools/call","params":{"name":"remuda_attachment","arguments":{"objectId":"obj_01a09cab-…"}}}
<- {"jsonrpc":"2.0","id":3,"result":{"content":[{"type":"image","data":"iVBORw0KGgpaWlpaWlpaWlpa…<224 base64 chars>","mimeType":"image/png"}],"isError":false}}

-> {"jsonrpc":"2.0","id":4,"method":"tools/call","params":{"name":"remuda_attachments_list","arguments":{"instanceId":"ins_someone_else"}}}
<- {"jsonrpc":"2.0","id":4,"result":{"content":[{"type":"text","text":"remuda_attachments_list is limited to this Agent instance's own session"}],"isError":true}}
```

测试断言 base64 解码后**逐字节等于**暂存的 168 B，不只是「非空」。
第 4 帧是本地拦截（`scope.rs`），在任何 Hub 请求之前就返回——
`an_agent_naming_another_session_never_reaches_the_hub` 断言 mock Hub 只收到
`GET /v1/caller`，**连直接子实例也不例外**。

---

## 5 mention 契约（给 driver / composer owner）

**本任务不改 web 与 driver。**下面是 driver owner 需要照抄的字符串契约。
D-027 现在追加的是绝对路径（`crates/remuda-driver/src/attachment.rs`
`append_path_mentions`，文案 `附件: <path>`）。D-028 之后**mention 要点名
objectId 与工具名**，因为 agent 现在有一条比 Read 更好的路。

约定格式，**逐字**（`<objectId>` 替换为实际 id，其余原样）：

```
附件（共 N 个）。请调用 MCP 工具 remuda_attachment 读取，参数 objectId：
附件: <objectId>  (image/png, 168 B)
附件: <objectId2> (image/jpeg, 1.2 MB)
```

要点：

1. **objectId 是唯一必需的标识**。工具的 `inputSchema.required` 就是
   `["objectId"]`，golden 测试
   `the_tool_accepts_exactly_the_argument_a_mention_quotes` 钉死了这一点，
   同时断言它**不接受** `file` / `path` / `instanceId` / `hostId`。
2. **工具名逐字写 `remuda_attachment`**，不要写 `mcp__remuda__remuda_attachment`：
   后者是 Claude Code 的 allowlist 写法，不是调用名；codex / grok 的前缀不同。
   agent 自己的 MCP 客户端知道怎么加前缀。
3. **保留绝对路径作为 fallback 行**（可选，建议保留一轮）：MCP server 未注入、
   或工具被 allowlist 挡掉时，claude 仍可用 Read 兜底。建议排在工具行之后，
   措辞明确是备选，避免 agent 两条都走、把同一张图读两遍。
4. **size 与 mime 写进 mention**：让 agent 在调用前就知道哪张图超 3.5 MiB
   会被拒，省一次往返。
5. `instanceId` **不要**出现在 mention 里。session 内调用时它是隐含的，
   显式写反而给了 agent 一个它无权使用的旋钮（写别的 session 一律拒）。

Claude Code allowlist 需要加：
`mcp__remuda__remuda_attachment,mcp__remuda__remuda_attachments_list`
（`docs/design/remuda-mcp.md` §55 那一行，由 MCP 文档 owner 更新）。

---

## 6 作用域与审批小结

| 调用方 | `remuda_attachments_list` | `remuda_attachment` |
|---|---|---|
| Agent（本 session） | 允许，无需审批 | 允许，无需审批 |
| Agent（指名别的 session，含直接子实例） | **拒**（本地，未发 Hub 请求） | **拒**（同左） |
| Human / Bot | 允许，必须显式给 `instanceId` | 允许（受 Hub 侧 object → instance 绑定约束） |
| Node host token | 不适用（401；Node 继续走 D-027 `GET /v1/objects/{id}`） | 同左 |

两个工具都**不进** `scope.rs` 的 `approvalId` 注入名单：它们是读操作，
且只读本会话已经被交付的内容，没有需要人类逐次批准的外溢动作。
Agent **上传**附件依旧一律 403（D-027 原规则，未改）。

---

## 7 本轮**未**验证 / 留给后续

按 D-028a (2) 的「未验证一律标 unknown，不许假装支持」：

1. **三家 harness 的实机收图未测**。本轮没有真的 claude / codex / grok 会话
   调用过这个工具。「三家都支持 MCP image content block」是 D-028 §4.5 的
   设计论断，**本文件不为它背书**。D-028 §12 的 print 退役条件里那条
   「MCP 附件三家验证」**仍然未满足**。
2. **composer / driver 侧未改**：§5 是契约文本，仓库里还没有任何代码产出这段
   mention。在 driver owner 落地之前，agent 只能靠 `remuda_attachments_list`
   自己发现附件。
3. **`text/*` 只在 fake Hub 上验证**。D-027 的上传 allowlist 是四种图片格式，
   Hub 里造不出 `text/*` 行（`staging_is_images_only_so_no_text_row_can_reach_the_read_route`
   断言了这个缺口）。文本分支的映射、UTF-8 拒绝在
   `cmd::mcp::attachment` 单测里覆盖，真实文本附件要等上传 allowlist 放宽。
4. **未做**：附件的 `DELETE`、读后标记已读、缩略图、多附件一次取回。

---

## 8 改动清单

| 文件 | 改动 |
|---|---|
| `crates/remuda-hub/src/attachments.rs` | 新增：两条路由、session 作用域、3.5 MiB 上限、审计日志 |
| `crates/remuda-hub/src/lib.rs` | 挂载 `attachments::routes()` |
| `crates/remuda-hub/src/agent_scope.rs` | 中间件放行两个 attachment 读形状 + 其单测 |
| `crates/remuda-hub/openapi/openapi.json` | `AttachmentRef` / `AttachmentPage` / `AttachmentContent` 与两条路径 |
| `web/src/lib/api.generated.ts` | `pnpm --dir web run gen:api` 重生成（纯新增） |
| `crates/remuda/src/cmd/mcp/attachment.rs` | 新增：两个工具、content block 映射、拒绝分支 |
| `crates/remuda/src/cmd/mcp/registry.rs` | `Tool::blocks()`：handler 自带 content 时直通，不字符串化 |
| `crates/remuda/src/cmd/mcp/scope.rs` | 两个工具的 Agent 策略（self-only） |
| `crates/remuda/src/cmd/mcp/mod.rs` | 注册 `attachment` 工具组 |
| `crates/remuda/src/cmd/test_hub.rs` | mock Hub 增加 attachment fixture（含 403 分支） |
| `crates/remuda/tests/mcp_attachment.rs` | 新增：§4 端到端 transcript |
| `crates/remuda/tests/mcp_schema_golden.rs` + `tests/mcp_golden/` | 新增：schema golden |
| `crates/remuda-hub/tests/attachments.rs` | 新增：Hub 路由 5 项 |

`crates/remuda-hub/src/objects.rs`、`http.rs`、`store.rs` 与所有 driver、
protocol enum、web UI **未改动**。
