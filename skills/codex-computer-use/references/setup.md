# 连接与验证

本 skill 使用用户已经安装的 **Codex Computer Use**。它不重新分发 OpenAI 应用，也不安装后台服务。仅仅落盘这个目录不会连接到任何 Mac；只有所在的 agent 会话真的调用下面的 launcher，才会连本机已装的运行时。

## 两条启动路径

| 路径 | 何时用 | 入口 |
| --- | --- | --- |
| 原生 MCP | 宿主会话已经暴露 `list_apps` / `get_app_state`，且调用不报未认证 | 宿主 MCP 工具 |
| cua-repl | 宿主没有这些工具，或工具返回 `Sender process is not authenticated` | [launch-cua-repl.sh](../scripts/launch-cua-repl.sh)（需 Remuda 按次授权，见下） |

`SkyComputerUseClient mcp` 的 `initialize` / `tools/list` 可以在未认证宿主上成功。随后的 `list_apps` 会失败。不要把元数据探测当成 UI 已通。

不要直连 `~/Library/Group Containers/2DC432GLL2.com.openai.sky.CUAService/IPC/computeruse.sock`：未签名进程会被服务端断开。

## 原生 MCP（Claude Code 等已认证宿主）

**本 skill 不安装自己，也不写宿主的用户级配置。** Remuda 按次授权时把 MCP 配置物化到本次启动的实例目录（`<launch_dir>/mcp-cua.json`），再作为 `--mcp-config` 交给 agent；宿主自己的配置始终由使用者自己拥有。

因此：

- **人**可以手工注册：用宿主自己的 MCP 注册方式，把本目录的 `scripts/launch-mcp.sh` 作为 stdio 服务加入；`CODEX_HOME` 非默认时用环境变量传入。已有同名配置时先看现有内容，不要自动删除或替换。不要把 Codex 认证信息、socket 或会话标识拷进宿主配置。
- **agent** 不得替人安装：不写宿主用户级配置目录，不改宿主的 MCP 注册。缺注册就报告，由人决定。

只探测协议元数据（不调用 `list_apps`，不修改任何配置）：

```sh
python3 <本 skill 目录>/scripts/probe_mcp.py
```

`metadata_ok` = `initialize` + `tools/list` 成功。`--schema` 打印当前参数定义。

用 `/mcp` 看连接。只有宿主实际返回可解释的界面或截图，才算观察链路可用。

## cua-repl

Grok 或其他未注入原生 Computer Use 工具的宿主走这条路。Launcher 使用已安装的 ChatGPT `cua_node` 和 `CODEX_HOME` 下的 `Codex Computer Use.app`。

```sh
/bin/sh skills/codex-computer-use/scripts/launch-cua-repl.sh
```

**未授权时这条命令会直接拒绝退出**（`REMUDA_CAPABILITY_COMPUTER_USE` 不是 `1`）：它求值任意 JS 并对本机每个应用持有点击、键入、按键绑定，所以只能由 Remuda 的按次授权（`--capability computer-use`）打开。没有交互式绕过；agent 不得自行导出该变量。

需要：

- macOS
- `/Applications/ChatGPT.app/Contents/Resources/cua_node`（可用 `CUA_NODE` 覆盖）
- `${CODEX_HOME:-$HOME/.codex}/computer-use/Codex Computer Use.app`

以 stdio JSON-RPC 对接。`initialize` 带 `capabilities.elicitation`。服务端会发 `elicitation/create` 做按应用审批；只批准用户点名的应用。

`CUA_REPL_ENABLED_SURFACES` 默认 `computer`。不要自己设 `NODE_REPL_JS_BANNER`：cua-repl 启动器会写入 `nodeRepl.env`。直接跑 `node_repl` 且不经过启动器时，`CUA_REPL_ENABLED_SURFACES is required`。

## 能力与故障边界

- 原生 MCP 是窗口级应用工具。cua-repl 额外提供 `cua.getTab` / 浏览器 API；本 skill 默认只用 computer 面。不要在原生 MCP 上调用 `cua.*`。
- 元数据可达不保证截图或操作权限。以本机实测为准。
- 客户端路径来自已安装插件。升级可能改路径或签名；失败时核对本机 `.mcp.json` 和 launcher。
- 不修改 TCC，不设绕过权限的参数，不伪装成 Codex 内部会话。
- 协议探测丢弃 stderr；它不是完整日志诊断器。

## 本机验证记录

本 skill 的结论来自哪些实测、哪些仍未验证，见 [codex-cua-1.md](../../../docs/design/evidence/codex-cua-1.md)（“验证记录”）。要点：**原生 MCP 元数据可达，但 Claude Code 这类宿主实际消费 CUA 截图与操作 UI 尚未验证**。别把下面任何一条当成 UI 链路可用的证明。

## 参考来源

- [Claude Code skills](https://code.claude.com/docs/en/skills)
- [Claude Code MCP](https://code.claude.com/docs/en/mcp)
- [Claude Code computer use](https://code.claude.com/docs/en/computer-use)
- [OpenAI Computer Use](https://learn.chatgpt.com/docs/computer-use)
- 本机 Codex `computer-use` 插件的 `.mcp.json`、`bin/computer-use-client-launcher`，以及 MCP `tools/list` schema
- 本机 ChatGPT `cua_node` 的 `@oai/cua-repl` / `@oai/sky`
