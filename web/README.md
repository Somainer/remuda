# runtime web

React + TypeScript + Vite PWA，按 `docs/design/ui-spec.md` v0.2。Night Corral token（`src/styles/tokens.css`）+ CSS Modules；图标 lucide-react；终端 `@xterm/xterm`。

Markdown 用 **react-markdown + remark-gfm + rehype-sanitize**：GFM、禁 raw HTML。不做首屏 Shiki/highlight——Shiki WASM 会撑大 PWA 预缓存壳。

## 启动

```bash
pnpm install
pnpm dev
```

开发环境 `.env.development` 默认 `VITE_MOCK=1`。Mock 回放 `src/fixtures/claude-p-init.json`（从 `docs/research/cli-help/claude-p-init.json` 拷入，前端不能 import `docs/`）和手工 Observation，无 Hub 也可打开：

- `/sessions` 会话列表
- `/sessions/new` 新建
- `/s/:instanceId` 结构化 transcript
- `/approvals` 审批中心

显式 mock：

```bash
VITE_MOCK=1 pnpm dev
```

接 `remuda dev`（默认 loopback `:8787`）。REST：`POST /v1/instances`、`POST /v1/instances/:id/commands`；journal 仍走 JSON-RPC + `WSS /v1/client`（同一连接上的 binary 帧是 tty-binary-v1，本页不解码）。访问码 header `X-Remuda-Access-Code`（也可 `Authorization: Bearer`）；不要把长期 token 放进 WS URL。若 REST 404 则回退 `POST /v1/rpc`（M0-11 router 若尚未合入）。

```bash
VITE_MOCK=0 VITE_API_BASE=http://127.0.0.1:8787 VITE_ACCESS_CODE=dev pnpm dev
```

## 脚本

| 命令 | 作用 |
| --- | --- |
| `pnpm dev` | Vite 开发服 |
| `pnpm build` | `tsc -b` + 生产包 |
| `pnpm test` | Vitest（status 投影、tool registry、journal seq/gap、transcript assemble） |
| `pnpm test:e2e` | Playwright `session-structured` / `new-session` / `approvals`（chromium + mobile-webkit） |
| `pnpm lint` | oxlint `src` |
| `pnpm preview` | 预览生产包（构建时生成 `dist/sw.js`） |

PWA：`public/manifest.webmanifest`（standalone，`start_url=/sessions`）+ `sw.src.js`（只预缓存壳，不缓存 `/v1/` journal）。它不是 public 目录里逐字节拷贝的静态文件：`vite.config.ts` 经 `sw-build.ts` 插件在构建时把按构建派生的缓存名盖进去、输出 `dist/sw.js`（dev 服在 `/sw.js` 提供同一 worker），所以每次部署 worker 字节都变、旧壳在 activate 时被清掉。生产且 HTTPS/`isSecureContext` 才由 `startPWA` 注册 `/sw.js`。
