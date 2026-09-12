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

接真实 Hub（JSON-RPC `POST /v1/rpc` + `WSS /v1/client`）：

```bash
VITE_MOCK=0 VITE_HUB_URL=https://hub.example pnpm dev
```

## 脚本

| 命令 | 作用 |
| --- | --- |
| `pnpm dev` | Vite 开发服 |
| `pnpm build` | `tsc -b` + 生产包 |
| `pnpm test` | Vitest（journal seq/gap/重连、tool registry 分派） |
| `pnpm lint` | oxlint `src` |
| `pnpm preview` | 预览生产包（注册 `public/sw.js`） |

PWA：`public/manifest.webmanifest`（standalone，`start_url=/sessions`）+ `public/sw.js` 只预缓存壳，不缓存 `/v1/` journal。生产且 HTTPS/`isSecureContext` 才 `register('/sw.js')`。
