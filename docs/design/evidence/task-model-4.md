# task-model 任务 4（t-board-api）证据：看板投影端点 + archived_at

- 日期：2026-09-20
- 分支：`wt/c-tboardapi/b-tboardapi-md`
- 计划：`briefs/plans/task-model.md` (B.4、D5、D6、D12、E6、E9、任务 4)
- 设计依据：**D-050**（由并行任务 c-tspec 落档；本任务 PR 引用 D-050，实现口径与 D5/D6/D12 完全一致）
- 范围：8 态 ledger → 4 列只读投影（待办/进行中/已完成 + 已归档），独立于池层，状态机保持唯一权威。

> 合规：本文只用泛称（看板/参考看板产品），不含任何内部产品名、主机名、用户名或个人目录；本任务没有看板 UI（看板界面是任务 6 t-board-ui），因此**没有截图**——截图只在有 Remuda 自身渲染面时才提交。

## 交付物

| 文件 | 内容 |
|---|---|
| `crates/remuda-protocol/src/task.rs` | `archived_at: Option<Timestamp>`（serde default + skip_serializing_if）、`BoardColumn` wire 枚举、`Task::board_column()` 只读派生 helper（**不进** `can_transition_to`） |
| `crates/remuda-protocol/src/schema.rs`、`schema/protocol.schema.json`、`web/src/types/generated.ts` | 新类型注册并重新生成（`just gen-types` 等价物：`cargo run -p remuda-protocol --example gen_types`） |
| `crates/remuda-hub/src/tasks.rs` | `GET /v1/board?project=`、`POST /v1/tasks/{id}/archive`；迁移在本文件 `migrate()` 内用 `ensure_column` 加一个增量可空列 `archived_at`（幂等，旧行 NULL 且 doc_json 不动）；insert/update 镜像该列；迁移单测 |
| `crates/remuda-hub/src/agent_scope.rs` | board 读、archive 写加入 agent 路由许可（门控仍是 grant-verb：无 dispatch grant 的 leaf agent 403） |
| `crates/remuda-hub/openapi/openapi.json` + `web/src/lib/api.generated.ts` | 两条新路由与 `BoardView/BoardItem/BoardColumn/Task.archivedAt` 文档并重生成 |
| `web/src/features/tasks/boardColumns.ts` + `.test.ts` | UI 侧纯投影镜像（任务 6 看板界面复用）：列投影、failed 按 placement、归档正交、多跳拖拽守卫、done≠解锁、SE-nn 派生 |
| `web/tests/e2e/task-model-board.hub.spec.ts` | 通过 in-process Hub + 假 Node 的 hub e2e（4 个用例，连跑 3 次全绿） |

## 验收逐条对照（计划任务 4 的六项）

1. **三列严格映射 + 归档正交**：pending/placed/deferred/parked→待办；running/stalled→进行中；只有 done→已完成；`archivedAt != null`→已归档且 state 逐字节不变（不碰 `is_terminal` 的 done/failed 语义）。
   - 单测：`board_column_projects_the_eight_states_onto_three_columns`、`board_column_archive_is_orthogonal_to_state_and_skipped_on_wire`（protocol）；集成 `board_projects_states_placement_failures_and_the_archive_flag`（hub）。
2. **failed 按 placement 派生**：无 placement→待办，有 placement→进行中；`state` 仍是 `failed`（红角标由状态本身表达），`blockedReason` 随行携带；不单开列、不折进已完成、不新增 pre-fail 存储。hub 集成测试同时构造了 dispatch 后 failed（进行中）与未派发 failed（待办）两条断言。
3. **列→列多跳映射**：待办→进行中目标态 `running`，pending/deferred 走 `→placed→running` 两跳、placed/parked 单跳；进行中→已完成目标态 `done`（stalled 先回 running）；回拖进行中→待办落 `parked`。每一跳都过 `can_transition_to`；TS 侧 `columnMoveHops` 在发任何 PATCH 前校验整条链，任一跳非法返回 `null` 整体拒绝。e2e 实证：直接 `pending→running` 得 409、合法两跳后卡片落入进行中列。
4. **grant-verb 门控**：`set_task_state` 与 `archive` 都走 `require_dispatch`（`GrantVerb::Dispatch`）。集成测试 `scoped_agents_are_enforced_on_task_routes` 断言无 dispatch grant 的 leaf worker 对 archive 返回 403，且可读 board 但只看到自己 project scope 的卡；持 grant 的协调员 agent 被授权（不是「agent 一律 403」）。
5. **done 永不解锁，只有 landed_sha 解锁**（I1/E6）：hub 集成与 e2e 都断言 done-but-unlanded 的 dependent `lockedDeps` 仍含上游 id，`POST /land` 带 sha 后才清空；TS 侧 `lockedDepIds` 同语义（含「上游缺失绝不静默解锁」）。
6. **旧行字节一致**：`archived_at` 为 `#[serde(default, skip_serializing_if = "Option::is_none")]`，迁移只 `ADD COLUMN`（NULL），不回填、不重写 doc_json。protocol 单测断言旧 JSON 文档可解码且无 `archivedAt` 键；迁移单测对**精确的旧 schema** 建库后迁移，验证旧行列为 NULL、迁移幂等。

## 端点行为摘要

- `GET /v1/board?project=prj_…`（project 可省，省=scope 内全部项目）
  - 200：`{ "project": string|null, "columns": { "todo": BoardItem[], "in-progress": BoardItem[], "done": BoardItem[], "archived": BoardItem[] } }`
  - 每个 `BoardItem` = 完整 Task 文档 + 只读 `boardColumn` + 派生 `displayKey`（形如 `SE-01`，每项目按 created_at 最旧优先编号，D12，**不存储**）。
- `POST /v1/tasks/{id}/archive`：门控 `GrantVerb::Dispatch` + project scope；置 `archivedAt=now`，不动 state；已归档再归档→409；任务不存在→404。
- 列移动**没有**新写端点：UI 把拖拽拆成一串合法 `PATCH /v1/tasks/{id}`（每跳独立过状态机），整体失败即停在原处。

## 测试记录

| 检查 | 命令 | 结果 |
|---|---|---|
| protocol 单测 | `cargo test -p remuda-protocol` | （见下方「验证记录」） |
| hub 单测/集成 | `cargo test -p remuda-hub`（含 archived_at 默认 + tasks 表迁移单测） | （见下方「验证记录」） |
| OpenAPI 覆盖 | `cargo test -p remuda-hub --test openapi` | 4 passed |
| web 纯函数 | `pnpm --dir web test`（含 21 条 boardColumns） | 1411 passed |
| typecheck/lint | `pnpm --dir web typecheck` / `oxlint`（新文件） | PASS / 0 findings |
| hub e2e（本规格 ×3） | `pnpm --dir web run test:e2e:hub task-model-board`（lock slot b，PW_CHANNEL=chromium） | 4 passed × 3 |
| hub e2e（全量 ×1） | `pnpm --dir web run test:e2e:hub`（同一 lock slot） | （见下方「验证记录」） |
| 密钥/隧道扫描 | `bash scripts/ci/secret-scan.sh` / `no-tunnel-scan.sh` | （见下方「验证记录」） |

端口（lock slot `flock …/locks/e2e.lock-b`）：`HUB_E2E_LISTEN=127.0.0.1:59270 HUB_E2E_WEB_PORT=59279 HUB_E2E_UPSTREAM_LISTEN=127.0.0.1:59271`。

### 端点 transcript

对**完全组装的 live Hub**（in-process Hub + 已 enroll 的假 Node，`cargo run -p remuda-hub --example hub_e2e`，cookie 会话 = 浏览器登录后的等价形态）实际跑出。造 8 张卡（pending/deferred/placed/running/stalled/done + 两个 failed：一个未派发、一个先写 dispatch placement），id 已脱敏。

建卡与状态迁移均为 200：`deferred 200｜placed 200｜running: placed→running 200/200｜stalled: placed→running→stalled 200/200/200｜done: placed→running→done 200/200/200｜failed(无 placement) 200｜failed(dispatch 后) 200`。

```jsonc
// GET /v1/board?project=prj_e2e —— 响应的【标注摘要】：真实卡片是完整 Task 文档
// （含 id/revision/createdAt/updatedAt/projectId/title/mandate/class/owns/deps/
// budget 等全部 Task 字段）外加 boardColumn 与 displayKey；下面为可读只保留与
// 投影相关的字段。placement 在 None 时由 skip_serializing_if 省略（即键不
// 存在），仅 failed(dispatch) 卡带真实 TaskPlacementRef 对象（字段已脱敏省略）。
{
  "project": "prj_e2e",
  "columns": {
    "todo": [
      { "id": "tsk_e2e", "state": "pending",  "boardColumn": "todo", "displayKey": "SE-01", "blockedReason": null },
      { "id": "tsk_e2e", "state": "deferred", "boardColumn": "todo", "displayKey": "SE-02", "blockedReason": null },
      { "id": "tsk_e2e", "state": "placed",   "boardColumn": "todo", "displayKey": "SE-03", "blockedReason": null },
      { "id": "tsk_e2e", "state": "failed",   "boardColumn": "todo", "displayKey": "SE-07", "blockedReason": "supply exhausted" }
    ],
    "in-progress": [
      { "id": "tsk_e2e", "state": "running", "boardColumn": "in-progress", "displayKey": "SE-04", "blockedReason": null },
      { "id": "tsk_e2e", "state": "stalled", "boardColumn": "in-progress", "displayKey": "SE-05", "blockedReason": null },
      { "id": "tsk_e2e", "state": "failed",  "boardColumn": "in-progress", "displayKey": "SE-08", "blockedReason": "worker exited 42",
        "placement": { /* 真实 TaskPlacementRef：placementId/hostId/instanceId/branch/model，此处脱敏省略 */ } }
    ],
    "done": [
      { "id": "tsk_e2e", "state": "done", "boardColumn": "done", "displayKey": "SE-06", "blockedReason": null }
    ],
    "archived": []
  }
}
```

要点：failed 无 placement→待办、有 placement→进行中；`state` 仍是 `failed`（角标即状态本身），`blockedReason` 随行；只有 done 在已完成列；已归档列为空。

```jsonc
// POST /v1/tasks/{id}/archive （归档那张 stalled 卡）—— 完整 Task 响应的标注摘要
{ "id": "tsk_e2e", "state": "stalled", "archivedAt": "2026-09-20T12:38:17.893Z" }
// 再次 POST archive → HTTP 409
```

归档后再 `GET /v1/board`（下面同样为只留投影字段的标注摘要）：stalled 卡只出现在 `archived`（`{ "state": "stalled", "boardColumn": "archived" }`），进行中列不再含它，待办/已完成两列状态不变——归档移动卡片但不改 state。

```text
非法捷径 PATCH state=running（pending→running）→ HTTP 409（整跳拒绝）
合法多跳 pending→placed→running → 200 → 200（卡片随后在进行中列出现）
```

依赖解锁的 live 断言由 hub e2e `the done column never unlocks dependencies; only a landed sha does` 覆盖：done 卡（未 land）时 dependent 的 `lockedDeps == [depId]`，`POST /v1/tasks/{dep}/land {sha}` 后 `lockedDeps == []`。

## 验证记录

- `cargo test -p remuda-hub -p remuda-protocol`：protocol 全部 + hub 全部通过（hub lib 167 单测含 2 条新迁移测试；hub 集成 tests 8 个文件，其中 `tests/tasks.rs` 8 用例含新 board/archive 用例）。
- `cargo clippy --workspace --all-targets -- -D warnings`：PASS（exit 0）。
- `cargo fmt --all --check`：clean。
- `pnpm --dir web run gen:api` + `cargo run -p remuda-protocol --example gen_types -- --check`：生成物均为最新。
- `pnpm --dir web test`：144 文件 / 1411 passed（含 21 条 boardColumns）。
- `pnpm --dir web typecheck`：PASS；`oxlint src`（含新文件）：exit 0。
- 本规格 3 连跑（lock slot b，PW_CHANNEL=chromium）：每次 4 passed。
- **全量 hub e2e**（同锁槽/端口，2026-09-20）：161 用例 **159 passed / 2 failed**。两条失败为已记录的既有负载型 flaky（与 main 最新证据 `6d83680a` 一致）：
  - `ux-nextstep.hub.spec.ts:159`（main 证据已点名的 90s PTY 长用例）；
  - `promoted-claude.hub.spec.ts:89`（`instance create 409 HOST_OFFLINE`，假 Node 繁忙瞬断，同类负载时序）。
  - 两者隔离复跑（同锁槽）：**2 passed**，与本改动无关；本任务的 `task-model-board.hub.spec.ts` 在全量中 4/4 通过。
- `bash scripts/ci/secret-scan.sh`：pass；`bash scripts/ci/no-tunnel-scan.sh`：passed。

## 备注

- 看板 UI（三列界面、拖拽手势、红角标渲染、归档过滤器）是**任务 6 t-board-ui**；本任务只交付端点、协议派生与纯函数镜像。
- 与并行任务 c-tpool 零文件重叠：本任务未碰 `crates/remuda-node/**`、lease 路由或 `store.rs` 新表（唯一存储增量是 tasks 表的一个可空列）。
- 并行任务 c-tspec（D-050）落档后，本任务 PR 引用 D-050；协议/端点口径（failed 按 placement、8→4 只读投影、archived_at 增量列、多跳拖拽、grant-verb 门控）以本实现与计划 B.4/D5/D6/D12 为准。
