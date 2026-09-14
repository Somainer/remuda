# G2 「工作区当前变更」只读视图实现证据

- 日期：2026-09-14
- 批次：workbench G2（实现期）
- 契约：[../files-view-contract.md](../files-view-contract.md)
- 勘察基线：[workbench-g-files-contract-1.md](./workbench-g-files-contract-1.md)
- 源码基线：分支 `wt/ux-g2/workspace-changes-view`（从 `origin/main` = `31dbbe4`，含 G1 契约）
- 方法：Node 侧在临时 git 仓库上做真实只读计算的单测；Hub 侧对脚本化 in-process Node 做 403/409/422 路由测试；Web 侧纯投影单测 + hub-live Playwright（fake node + 合成 fixture，fake-harness，不调用真实模型）

## 交付物

| 层 | 文件 |
| --- | --- |
| Node | `crates/remuda-node/src/workspace_scm.rs`（新模块 + 10 个单测）；`server.rs` / `runtime_link.rs` / `stdio.rs` / `transport/hubnode_codec.rs` 仅新增显式派发分支 |
| Hub | `crates/remuda-hub/src/workspaces.rs`（三个 GET 代理路由）；`openapi/openapi.json`（三条路径）；`tests/changes.rs`（403/409/422/400/401/404 + 无缓存）；`examples/hub_e2e.rs`（合成 SCM 夹具）；`lib.rs` / `transport.rs` 的 `#[doc(hidden)]` 测试辅助 |
| Web | `web/src/features/files/{FilesView.tsx,filesView.ts,filesView.test.ts,filesApi.ts,files.module.css}`（新目录，未碰 `session.module.css`）；`web/src/types/scm.ts`；`web/src/pages/SessionPage.tsx` 仅 files 分支与手机入口；`web/src/lib/api.generated.ts`（`gen:api` 重新生成）；`web/tests/e2e/ux-files.spec.ts` |
| 文档 | `docs/design/protocol.md` 新增 §2.7；本文 |

未触碰：`store.ts`（files 自带只读 `filesApi.ts`）、`Shell.tsx`、`SpacesPanel.tsx`、`Transcript.tsx/assemble.ts`、`styles/ui.module.css`、journal/observation 类型、任何写侧 git 命令。

## 只读白名单的可行性（契约 §7 末段的前置验证）

在本机 git 2.55.0 上实测：以 `git --no-optional-locks -C <root>` 配合 `GIT_OPTIONAL_LOCKS=0` 运行 `status --porcelain=v2 -z --untracked-files=all`、`rev-parse HEAD`、`rev-parse --show-toplevel`、`symbolic-ref`、`diff [--cached] --no-color --no-ext-diff -- <path>` 后，`.git/index` 的 size 与 mtime 均不变，且无 `index.lock` 残留。因此不需要回退到「不可用」状态；写锁不被触达的事实由单测 `reads_never_take_the_index_lock_or_mutate_the_workspace` 持续断言（见第 10 行）。

两个实现期发现：

1. **porcelain v2 用 `.` 而不是空格表示未改动的一侧**（`.M` / `A.`）。Node 解析后在响应里规整为契约示例的经典 XY（空格），rename 的 `2` 行带第二个 NUL 字段（原路径），解析器按 9 字段处理并回填 `origPath`。
2. **非 git 判定必须要求 toplevel 等于注册根本身。** 本机 `/tmp` 自身是一个 git 仓库，仅判 `rev-parse --show-toplevel` 成功会把 `/tmp` 下的非 git 临时目录误判为受支持；现在只有报告路径与 canonical 注册根相等才返回 `availability:"ok"`。

## 验收清单（契约 §7 十二行）

| # | 场景 | 覆盖与结果 |
| --- | --- | --- |
| 1 | 合法文件（已跟踪被修改） | **Rust** `status_diff_and_file_round_trip_on_a_temp_repo`：临时真实 git 仓库，条目 `xy=" M"`、`kind="modified"`、`oldOid/newOid`、size 正确，diff 含 `+edited`，回传 40 字符 `headOid`。**e2e** `wsp_e2e`：点 `src/main.rs` 看到 unified diff 与 `HEAD …`。 |
| 2 | 合法未跟踪文件 | **Rust** 同上：`??` 条目存在，`workspace.scm.file` 回传文本、`sizeBytes` 与 `sha256:` digest。**e2e** 点 `notes/todo.md` 看到受限预览内容与 `sha256:` 摘要。 |
| 3 | 越界路径 | **Rust** `contained_file_path_rejects_traversal_and_absolute_input`：`../outside`、绝对路径、空段、`a/../../b` 以及指向根外的 symlink 全部在读取前被 `path_guard`/白名单拒绝；叶子另以 `O_NOFOLLOW` 打开防 TOCTOU。argv 层额外拒绝 `--output=` 形式的选项注入与以 `-` 开头的 pathspec。 |
| 4 | 无变化 | **Rust** `clean_repo_reports_no_entries`：空条目集。**e2e** `wsp_g2_clean` 显示独立的「无变化」态（`files-clean`），文案无任何会话归因字样。 |
| 5 | 不支持 | **Rust** `non_git_directory_is_structured_unsupported`（三个 RPC 均 `availability:"unsupported"`、`not-a-git-repository`）与 `binary_changed_file_is_entry_level_unsupported_and_not_inlined`（二进制 diff/file 为条目级不支持，整视图仍可用）。**e2e** `wsp_g2_nogit` 显示「不支持」并说明 git 原因；`wsp_e2e` 的 `assets/logo.bin` 显示条目级「不支持」。 |
| 6 | 离线 | **Hub** `changes.rs`：注册 host 摘除 live Node → 409 `HOST_OFFLINE`。**e2e** 对 `/changes` 边界回 409，视图显示独立「离线」态且不渲染任何旧列表。 |
| 7 | 权限不足 | **Rust** git 探针超时/`Permission denied` 归类为 `availability:"denied"`（`timed-out` / `permission-denied`）。**Hub** 非 operator（in-session agent 凭据）→ 403。**e2e** `wsp_g2_denied` 显示独立「权限不足」，不与「采集失败」合并。 |
| 8 | 截断 | **Rust** `file_and_diff_cap_with_explicit_truncation_markers`：超 `maxFileBytes` 的文件 `truncated:true` 且不返回字节；超 `maxDiffBytes` 的 diff 按整行截断并回传 `bytesOmitted`；超 `maxEntries` 的列表回传 `entries:true` 与 `entriesOmitted`。**e2e** `wsp_g2_trunc`：列表「另有 12 项被省略」、大文件 diff 截断标记、未跟踪大文件预览上限标记分别出现。 |
| 9 | 内容变化 | **Rust** `content_change_is_detectable_via_head_and_digest`：提交后再取 file，`headOid` 与 `sizeBytes` 均变化。**e2e** `wsp_g2_changed`：列表 head 与路由层改写的 diff head 不同 → 出现「内容在采集后变化，请刷新」；1.5 s 内 status 调用数不增长（无轮询），点「手动刷新」后重新采集。 |
| 10 | 只读性（关键安全验收） | **Rust** `reads_never_take_the_index_lock_or_mutate_the_workspace`：连续 5 轮 status+staged/unstaged diff+file 后，`.git/index` size+mtime 不变、无 `index.lock`、夹具文件内容不变、`git status` 前后一致；`whitelist_accepts_exact_templates_and_rejects_everything_else` 断言全部 argv 命中固定模板（`add`/`commit`/`update-index`/裸 `status`/裸 `diff` 等被拒），无 shell。**e2e** 打开/读取条目期间拦截到的 `/commands` 写请求为 0；视图只发三个 GET。 |
| 11 | 命名与归因 | **e2e** 断言「工作区当前变更」标题恒在、副标题含「采集于」、正文无「本次会话」，且有中性说明「可能由本会话或同目录的其他会话产生」。响应载荷本身不含 `instanceId`/`turn` 字段。 |
| 12 | 手机 | **e2e** 390×780：`文件` 入口不再被 `.deskOnly` 隐藏，可进入既有全屏 `/files` 路由，返回 `/structured` 后 transcript 滚动位置保持一致。 |

六态在 Web 纯投影层由 `filesView.test.ts`（19 个断言）逐一锁定：`clean` / `changes` / `unsupported` / `denied` / `offline`（409 与 422）/ `missing`（404）/ `forbidden`（403）/ `failed`（其他错误保持可重试，绝不冒充某个可用性态）。

## 安全要点

- 定轴只用 `hostId + workspaceId`（REST 路径）与相对 `path`（query）；不接受绝对路径、`repo`、`base`、通配。
- 路径在读取前再次 containment 校验，并拒绝 `..`、glob、`:(magic)`；叶子 `O_NOFOLLOW` 打开。
- git 无 shell、argv 白名单、字面 `--` 分隔、`--no-optional-locks` + `GIT_OPTIONAL_LOCKS=0`、独立进程组、5 秒截止、输出字节上限（status 4 MiB、diff 256 KiB/请求、file 1 MiB）。
- Hub operator-only（`require_operator`），agent 凭据 403；不缓存、不落盘，离线即 409/422。
- 二进制/超大文件默认不内联；digest 仅在取内容时计算（列表不全量读盘）。

## 本机验证命令与结果

- `cargo test -p remuda-node --lib workspace_scm`：10 passed。
- `cargo test -p remuda-hub --test changes` 与 `--test openapi`：均 passed。
- `cargo clippy -p remuda-node -p remuda-hub --all-targets -- -D warnings`：干净；`cargo fmt` 后 `--check` 干净。
- `pnpm --dir web run lint` / `tsc --noEmit` / `vitest run`：无 files 相关告警，450 个单测全过。
- `pnpm --dir web run gen:api` 后 `git diff --exit-code -- web/src/lib/api.generated.ts` 干净。
- hub-live `ux-files.spec.ts`（8 个用例，含手机）：8 passed，全程 fake node + 合成夹具。

## 局限与未做（契约 §5）

- 未做会话/轮次归因、目录浏览、文件编辑、提交/分支等任何写操作；未解析 file-history opaque blob；未在 Hub 缓存或跨主机聚合。
- 未实现「会话提及」非权威角标（明确留待后续评审）。
- Node 的截止时间与上限值沿用契约提案（5 秒 / 5000 条 / 256 KiB / 1 MiB）；真实大仓库时延未在生产主机上测量。
- e2e 的「离线」通过在浏览器代理边界回 409 模拟（共享 fake Node 同时服务其他用例，不能真杀）；真实断连路径由 Hub 409 单测覆盖。
