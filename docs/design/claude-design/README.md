# Claude Design 画布（Remuda UI Spec v0.2）

来源：claude.ai/design 项目 "Remuda UI design system"（`3effb65c-0d94-457d-adde-8c0c647919f4`），由用户在 Claude Design 里基于 `docs/design/ui-spec.md` v0.2 产出，2026-09-12 同步。

- `Remuda UI Spec v0.2.dc.html`：设计画布（`<x-dc>` 模板 + 内联样式），6 组画板（桌面 1440 / 手机 390）：1a 会话列表、1b 会话页·结构化、1c 会话页·终端、1d 新建会话 sheet、1e 审批中心、1f 主机 + Provider。视觉方向「Night Corral」深色，另有 `data-theme="ledger"` 浅色 token。
- `support.js`：dc-runtime（生成文件），在浏览器里直接打开 `.dc.html` 即可渲染（需要能加载 Google Fonts）。
- `github.md`：Claude Design 侧的同步记录。

前端实现以此画布为像素级依据；token 定义在画布 `<style>` 的 `body{--canvas…}` 段，与 `web/src/styles/tokens.css` 对齐。
