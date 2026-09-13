# 附件与图片直通 MVP 实跑证据（D-027）

工作树 `wt/x-clip/image-passthrough`，自起 `remuda dev`（Hub 60780 / Node 60787，
data dir `/tmp/x-clip-dev/data`），claude 2.1.270，模型 sonnet。所有结果来自真实运行，
不是构造的 fixture。

## 0 §8.1 阻塞项（开工第一件事）

**结论：Node 持有可用的 Hub HTTP base URL + token，MVP 走 HTTP 回拉主路径，
不启用 channel 2 `ObjectChunk` 兜底。** 源码复核表见
[clipboard-images.md §8.1](../clipboard-images.md)，决策见 D-027a。

关键事实：`WssConfig.url` / `.token` 在生产 Node、daemon、`remuda dev` 三条路径上都齐备，
且 Hub 已经用**同一个** host token 认证 WS 握手（`ws.rs` `presented_token` →
`store.rs` `authenticate_host`）。缺的只是 Hub 侧一个复用同一查询的 HTTP `require_host`
提取器和 Node 侧一个 `reqwest` 客户端。

## 1 claude-print：贴图后问「什么颜色」，答「Red」

这是本任务要求的主验收项。64×64 纯红 PNG（168 B）。

```
# 1) 暂存（浏览器 POST /v1/objects 走的同一条路）
$ curl -X POST "http://127.0.0.1:60780/v1/objects?instanceId=ins_01a09aed-…" \
       -H 'content-type: image/png' --data-binary @red.png
HTTP 200
{
  "objectId":  "obj_01a09aed-f84d-7073-bf21-48d34af94253",
  "mediaType": "image/png",
  "size":      168,
  "digest":    "ca96adb50da6a5f6cbfe03c9c5eb8ad8f1a6c88f526b8a4b9d9d7d3b9ef90157",
  "name":      "obj_01a09aed-f84d-7073-bf21-48d34af94253.png",
  "expiresAt": "2026-09-14T13:17:15.480Z"
}

# 2) 发送，命令里只带 metadata
$ curl -X POST ".../v1/instances/ins_01a09aed-…/commands" -d '{
    "operation":"instance.send",
    "payload":{"prompt":"what color is the image? answer with one word",
               "attachments":[{"objectId":"obj_01a09aed-…"}]}}'
HTTP 200  state=accepted
```

Hub 用自己的行改写了 metadata（调用方只给了 objectId）：

```json
[{"objectId":"obj_01a09aed-f84d-7073-bf21-48d34af94253",
  "mediaType":"image/png","name":"obj_01a09aed-….png","size":168}]
```

journal：

```
seq=11 [user]      'what color is the image? answer with one word'
seq=20 [assistant] 'Red'
seq=21 [assistant] 'Red'
seq=22 [assistant] 'Red'
```

**答「Red」**——浏览器形状的上传 → Hub 暂存 → Node 回拉落盘 → base64 image block →
模型正确识别颜色，整条链路在真实运行里闭合。

落盘（目录 0700、文件 0600、名字由 object id 派生）：

```
-rw-------  168  /tmp/x-clip-dev/data/dev-hub/node/instances/ins_01a09aed-…/attachments/
                 obj_01a09aed-f84d-7073-bf21-48d34af94253.png
```

审计行：

```
INFO attachment.uploaded object_id=obj_01a09aed-… digest=ca96adb5… bytes=168
     media_type=image/png instance_id=ins_01a09aed-… device_id=dev_01a09ae7-…
```

### 1.1 先踩到的坑（记录以免重复）

第一个 claude-print 会话没有指定 model，落到 `fake`，模型答
"There's an issue with the selected model (fake)"。**图片链路本身是通的**——附件已落盘、
block 已发出——只是模型不存在。显式 `"model":"sonnet"` 后即得 "Red"。

## 2 claude-pty：路径提及被 TUI 采纳为 `[Image #1]`

PTY 不能内联字节，按设计走绝对路径提及。pane 实况（经 `/v1/follow?tty=1` 抓取、
去掉 ANSI）：

```
❯ [Image #1]what color is the image? answer with one word
  请读取下面这个本地图片文件（附件）：
  附件:
  ⎿  file://<HOME>/.claude/image-cache/01a09afb-…/1.png  [Image #1]
```

**Claude Code 把路径读进去并转成了 `[Image #1]`**，还把图片拷进了自己的 image-cache——
这正是设计 §3 里「远端唯一可靠通路」要验证的行为，现在有实跑证据。

随后的模型调用没能返回颜色，两次都卡在与本改动无关的外部原因：

- 第一次：`API Error: 400 … Claude Code Artifacts are enabled`（本机 Claude 配置）；
- 第二次（已用 `settingsOverlayPath` 关掉 Artifacts）：`API Error: 503 no eligible
  upstream account`——同一时段整个 relay gateway 的故障，本 session 自己也被它打断过两次。

即：**pty 侧「把图片送到 agent 眼前」这一步已验证成功**，未完成的只是其后的模型回答，
原因在网关而不在链路。print 侧（§1）已经给出端到端的颜色答案。

pty 侧落盘同样正确：

```
-rw-------  168  …/instances/ins_01a09afb-…/attachments/obj_01a09afc-….png
```

## 3 安全行为（实跑）

| 检查 | 结果 |
|---|---|
| `GET /v1/objects/{id}`，owner device cookie | `200` |
| `GET /v1/objects/{id}`，匿名 | `401` |
| 声明 `image/png` 实为 PDF 字节 | `400`（magic bytes 嗅探否决声明） |
| 上传返回的文件名 | `<obj_id>.png`，原始文件名被丢弃 |
| 单文件 > 5 MiB | `400 RESOURCE_LIMIT`（路由测试） |
| Agent-origin 上传 | `403`（路由测试） |
| 他机 host token 读别人的附件 | `403`（路由测试） |

## 4 清理（实跑）

`instance.close` 之后：

```
attachments dir: No such file or directory     # Node 侧递归删除
GET /v1/objects/{id}: 404                      # Hub 侧随终态 lifecycle 删行
```

## 5 自动化测试

| 范围 | 数量 | 说明 |
|---|---|---|
| `remuda-protocol` | 14 | `attachments` 增量、缺字段降级为纯文本 |
| `remuda-hub` tests/objects.rs | 11 | 鉴权矩阵、嗅探、限额、host 绑定、随实例清理 |
| `remuda-node` tests/attachments.rs | 9 | 回拉落盘、权限位、失败即失败、穿越 id、orphan sweep |
| `remuda-driver` | 73（lib） | base64 往返（真 PNG）、超限降级、pty 路径提及、shell 引号 |
| web vitest | 232 | 含 11 条剪贴板单测 + 8 条 composer 附件交互 |
| hub-live e2e | +1 | 贴入真 PNG → 暂存 → metadata 抵达 Node |

## 6 已知边界（留 v2，均在设计内）

- Hub 不回显附件到 journal，发送后的缩略图用本地 blob URL（设计 §6 明确留 v2）。
- `GET /v1/instances/{id}/screen` 在 Hub 上没有路由（既有缺口，与本改动无关），
  pty 实况改用 `/v1/follow?tty=1` 抓取。
- codex 的 `LocalImage` 结构化通路未接（wire 类型已建模），MVP 按路径提及处理。
- grok 无任何远端图片入口，发送时记一条 warning 级 journal note。
