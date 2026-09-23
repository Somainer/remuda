# c-jevspike 离线阶段证据：闸门失败分诊语料、规则基线与离线判定器

日期：2026-09-23。分支 `wt/c-jevspike/b-jevspike-md`。

范围：候选 A1（闸门失败后 `likely_flake` vs `likely_regression` 分诊）的**离线**
可行性 spike。本阶段**零网络请求、零 Rust 代码、零模型调用**——我们还没有 API
key，真调是下一阶段的事，需所有者另发 key。设计权威：`briefs/c-jevspike.md`，
冲突以 brief 为准。

交付物：

- `scripts/tests/fixtures/jev-spike/generate_corpus.py` — 固定种子的语料/响应生成器（仅标准库）
- `scripts/tests/fixtures/jev-spike/corpus.jsonl` — 204 条手工黄金标签语料
- `scripts/tests/fixtures/jev-spike/manifest.json` — 种子、分类统计、家族清单、roster、sha256
- `scripts/tests/fixtures/jev-spike/responses.fixture.json` — 合成 predictor 的 **pass** 响应（harness 自检用）
- `scripts/tests/fixtures/jev-spike/responses.fail.fixture.json` — 合成 **no-go 负对照**
- `scripts/tests/test_jev_spike_eval.py` — 脱敏断言 + 规则基线报表 + go/no-go 判定器

所有制品可由生成器**逐字节复现**（测试锁定）：第二个人仅凭仓内文件
`python3 scripts/tests/test_jev_spike_eval.py` 就能复跑出本文全部数字。

## 1. 语料构成

每条记录的手工黄金标签字段是 **`gold`**（取值 `regression` / `flake`；语义：
同样代码在健康 runner 上不改任何东西重跑，是否必然复发）。完整 schema：
`id`（`c0001`…`c0204`，稳定不随重排变化）、`category`、`step`、`signature`
（模板家族，家族级固定 gold）、`gold`、`log`（多行日志尾部，只用仓库相对路径）。

共 204 条、53 个模板家族、种子 `20260923`，六类独立报表：

| category | n | regression | flake | 形态样板 |
|---|---:|---:|---:|---|
| cargo-test | 48 | 27 | 21 | libtest 断言、`error[E....]` 编译错误、wire_golden `trailing characters`、请求序列不符；抖动侧为已知 timing/attachment/journal 名单、端口争用、锁等待、TZ/umask、cgroup OOM |
| clippy | 28 | 24 | 4 | `-D warnings` lint、target-gated dead-code；抖动侧为 rustc ICE、镜像源 spurious network |
| fmt | 20 | 18 | 2 | `Diff in … at line N:` 确定性差异；抖动侧为 runner rustfmt toolchain drift |
| vitest | 36 | 21 | 15 | AssertionError、role 缺失、esbuild transform、snapshot drift、业务 invariant；抖动侧为 EffortSlider verbatim fallback（真机红/另一台绿）、fake timer、TZ、teardown 泄漏、worker 强杀 |
| playwright | 44 | 21 | 23 | toHaveText/toHaveCount/DOM 缺节点、webServer 构建失败；抖动侧为 30s load 超时、effort pending follow frame 丢失、端口占用、browser 崩溃、socket gap、trace.zip EBUSY |
| infra | 28 | 15 | 13 | frozen-lockfile、`error TS...`、secret-scan/no-tunnel-scan、vite resolve、迁移失败；抖动侧为 ENOSPC、ECONNRESET、gate-e2e.lock flock、链接期 SIGKILL、缓存损坏、EACCES |
| **合计** | **204** | **126** | **78** | |

每个家族都照着 brief「背景」里本周真实见过的失败形态造（包括两个明确点名的
真实 flake：`never_reaches_the_hub` 重试通过、EffortSlider 跨机红绿），包括一个
**陷阱家族** `ct-perf-regression`：长得像 timing 抖动，实际是新增阻塞代码导致
预算超支——gold = regression。

脱敏：语料绝不含真实路径/用户名/主机名/IP/token；`@playwright/test`、
`@vitejs/plugin-react`、`@/features/...` 是 npm 作用域包名/别名，不是邮箱。
脱敏断言（下节）对每条记录的序列化 JSON 检查 `/home/`、`/Users/`、点分 IPv4、
邮箱形态，命中即测试失败。

## 2. 规则基线实测（真实数字）

确定性规则基线（brief 指定的形态）：

1. 日志命中已知抖动名单（`manifest.known_flaky_roster`，8 个测试名/文件名，
   含本周真实 flake）→ **flake**；
2. 否则日志含 `contain formatting differences` / `could not compile` /
   `error[Edddd]` → **regression**（fmt 差异、clippy、Rust 编译错误）；
3. 其余 → **unknown**（弃权：不猜测）。

在 204 条语料上的实测（`--baseline` 原样输出）：

```
category        n  gR  gF  pR  pF  pU  R prec   R cov  F prec
cargo-test     48  27  21   6  12  30   1.000   0.222   1.000
clippy         28  24   4  24   0   4   1.000   1.000     -
fmt            20  18   2  18   0   2   1.000   1.000     -
vitest         36  21  15   0   4  32     -     0.000   1.000
playwright     44  21  23   0   9  35     -     0.000   1.000
infra          28  15  13   0   0  28     -       -       -
ALL           204 126  78  48  25 131   1.000   0.381   1.000
```

读法（这是本阶段唯一的实测结论）：

- 规则**开口时精确率 1.000**（48/48 regression、25/25 flake），但**弃权
  131/204（64%）**。
- fmt/clippy 两类被规则完全覆盖（regression coverage 1.000），连这两类的
  抖动形态（toolchain drift、ICE、镜像源）都被正确丢去 unknown 而不是误判。
- regression 总覆盖只有 **48/126 = 0.381**：Rust 业务断言、vitest/playwright
  的产品行为断言、infra 的 lockfile/tsc/scan/迁移，规则全部只能弃权。
- 「模型要赢的区域」因此非常明确：不是 fmt/clippy（那里 plain rule 已正确），
  而是剩下 78 个规则不敢说话的 regression。门槛 2 的 +15pp 增量也是相对这个
  0.381 基线算的。

## 3. 离线判定器：四条阈值与用法

`scripts/tests/test_jev_spike_eval.py`（标准库，无网络）：

```bash
python3 scripts/tests/test_jev_spike_eval.py              # 跑全部 unittest
python3 scripts/tests/test_jev_spike_eval.py --baseline   # 打印基线报表
python3 scripts/tests/test_jev_spike_eval.py --judge \
    scripts/tests/fixtures/jev-spike/responses.fixture.json   # 打印逐条判定，go 退出 0 / no-go 退出 1
```

判定器读 Jev `choice` 原语形态的信封（`answers.triage.choice` /
`.probabilities.{flake,regression}` / `.confidence` + 每条 `latency_ms`），按 id
与语料黄金标签内连接，缺 id / 重复 id / 概率越界 / 结构缺字段直接报错，然后
逐条计算 brief 的四条阈值，**任一不过即 no-go**：

1. **regression 精确率**：在 0.500–0.999 的 0.001 阈值网格上，取精确率 ≥ 0.95
   时 coverage 最大的工作点（「跳过重试」工作点；不存在合规工作点直接 FAIL）。
2. **coverage 增量**：该工作点的 regression coverage 减规则基线 0.381，
   必须 ≥ +0.15。
3. **p95 延迟**：nearest-rank p95 ≤ 900ms（真调时必须记客户端观测的端到端
   wall time，含网络，不能只记服务端）。
4. **尾部校准**：概率桶 [0,0.2) 与 [0.8,1.0] 的经验 regression 频率与桶中点
   (0.1 / 0.9) 偏差均 ≤ 0.15；空桶判 FAIL（无支撑不能声称校准）。

## 4. harness 自检（合成 predictor，**不是模型输出，对 Jev 不提供任何证据**）

两份响应 fixture 都由生成器里的种子化合成 predictor 产生，信封里带
`artifact_kind: synthetic-offline-fixture-not-model-output` 明示，唯一用途是
证明判定器本身工作正常。

- `responses.fixture.json`（pass profile）：判定器输出 **GO**——工作点
  p≥0.678 上精确率 0.9508（116/122）、coverage 0.9206（对基线 +0.540）、
  p95 675ms、尾桶偏差 0.100 / 0.090。
- `responses.fail.fixture.json`（负对照，种子 `SEED ^ 0xFA17ED`）：14 条
  flake 被以 0.985–0.995 的高概率答成 regression（且高于所有 regression 的
  概率，任何阈值都凑不出 0.95 精确率），约 10% 延迟注入 950–1400ms。判定器
  输出 **NO-GO**，阈值 1/2/3 与尾桶校准（偏差 0.159）同时变红。

**这些数字是判定器的单元测试，不是 Jev 的成绩。** 只在会通过的数据上测过
的判定器证明不了任何事，所以负对照与正对照同样是交付物。

## 5. 下一阶段真调需要什么

1. **Key 与模型钉版**：所有者签发 API key（经环境变量注入，绝不入库），
   模型钉 `jev-1.13.0`（调研 D6：永不使用 `jev-latest`）。数据出境批准仅限
   **合成语料**：真闸门日志一条都不许发。
2. **调用次数上限（硬顶 620 次）**：每轮全量 = 204 次 choice 调用（一记录
   一题，假设无批量）。预算 = 1 次连通性 smoke + 204 次正式评估 + 415 次
   余量（传输失败重试 + 一轮独立复跑确认阈值稳定性）。调用计数写进 runner，
   撞顶即停。
3. **预计 token 量（基于语料实测体积）**：日志文本合计 88,102 字符
   （均 431、p95 741、最大 878 字符 / 最多 16 行），粗估日志侧输入约
   2.2 万 token；加固定题干/选项语义（约 80 token/次），一轮典型输入约
   3.8 万 token；按最大日志逐条取严的上界约 6.1 万 token。输出每题仅
   choice + 两个概率 + confidence（<20 token），一轮 < 0.5 万 token。
   **整个真调阶段（≤620 次）输入的严上界约 19 万 token（典型约 12 万）。**
4. **预计费用**：离线阶段不掌握、也不应编造单价。费用 =
   `max(实际调用数,…) × 单价`，以上面的调用/token 上界为量，请所有者在
   发 key 时按对应价目卡确认金额；量级上这是几百次短调用、十几万 token 的
   **一次性** kill-gate 花费。
5. **runner 必须记录**：每题客户端端到端 wall time（阈值 3 用）、原始响应
   落盘（供二次复算与审计）、实际调用计数。
6. **样本量的诚实声明**：阈值 1 在 126 个 regression 上估，0.95 工作点的
   边界很薄（合成自检里 116/122 = 0.951，点估计刚过线，二项区间下界会低于
   0.95）。真调若点估计同样贴线，应按预案扩大语料后复测，而不是宣告通过。

## 6. 独立判断：值不值得花这几百次真调

**值得做，但以 kill-gate 的心态做，且现在不预设 go。**

- 规则基线在 fmt/clippy/编译错误上已经完美，那里加模型纯增成本、延迟和网络
  依赖——模型若只在这些类别上有效，应当直接 no-go。
- 但规则对 62% 的 regression 只能弃权，而弃权区域恰好包含最贵的误判：
  看起来像抖动、实则真回归的形态（`ct-perf-regression` 这类）会让系统白等
  一轮约 30 分钟的自动重试。这正是 A1 的产品价值所在，也是纯规则结构上
  吃不到的区域。
- 成本一次性且有硬顶（≤620 次、输入严上界约 19 万 token），即使结论是 no-go，
  买到的也是「最强存活候选被真实数据否决」的确定性，避免把网络依赖接进
  闸门后再发现没用。
- 已知风险（真调后若触发就是 no-go）：厂商明示校准只保证**组级**、不保证
  单条答案；样本量使精确率估计偏薄；延迟含真实网络后未必还在 900ms 内。

**本阶段结论仅限于：规则基线实测如上（精确率 1.000、regression 覆盖
0.381、64% 弃权）；Jev 值不值得，按第 3 节判定器对真实响应跑出的四条阈值
说话，在此之前没有结论。**
