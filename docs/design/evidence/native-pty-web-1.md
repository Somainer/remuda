# D-028 P2 web — native PTY default, composer steer/queue/interrupt, node-restart UX

Hub-live Playwright run against the fake Node in `crates/remuda-hub/examples/hub_e2e.rs`
(`pnpm run test:e2e:hub`, `REMUDA_EVIDENCE=1`). The fake Node now advertises a
`driverInventory` matrix on `node.hello` (`shell-pty` launchable) and reports a
native `agent_status` lifecycle on create/send/cancel so the web composer can
be driven through its real working/idle states. Night Corral.

Spec: `docs/design/native-pty-first.md` §1.0 / §5.1 / §6 / §8 (D-028 / D-028a,
2026-09-14). Protocol increments landed on `main` in
`wt/x-p1-proto/d028-protocol-increments` (`launchedBy`, `signalTier`,
`Capability.provision`, `PromptMode`).

## What landed (web)

- **New Session = prefill.** For claude/codex/grok/agy the default driver is
  `shell-pty`（「原生终端 (shell-pty)」）whenever the selected host's
  Node-reported kind/driver matrix (`host.capabilities.driverInventory`,
  stored verbatim by the Hub) says the pairing is launchable and the CLI is
  installed. No host data is hardcoded: no matrix reported → CLI presence
  only → native default stays; an explicit non-launchable `shell-pty`
  descriptor removes it and falls back to `claude-print` / `generic-pty`.
  Legacy drivers remain selectable but secondary. A read-only launch preview
  shows the §5.1 recipe summary (`claude --settings … --effort …`,
  CODEX_HOME/GROK_HOME shadow dirs, yolo flags) until the Hub exposes the
  materialized recipe GET.
- **Both projections.** `tty/gate.ts` no longer special-cases driver names:
  the terminal projection is allowed for any PTY-backed/tty-capable session;
  the structured projection is enabled by the reported `signalTier`
  (hook/file/osc) — not by `driver !== "claude-print"`. A new shell-pty agent
  session opens on 结构 and keeps 终端 one switch away.
- **Composer states (§6, revised by c-steer 2026-09-17).** A pure state
  machine (`features/composer/state.ts`) maps instance phase × measured
  steer/queue/interrupt provisions (`native|emulated|unknown`) to controls:
  - idle → 发送 (new-turn);
  - working → Enter QUEUES (codex: native Tab posted immediately; every other
    harness: Remuda-held client-side until the turn ends); 插队
    (⌘/Ctrl+Enter or its button) posts `mode:steer` — the Node interrupts the
    running turn and delivers the message ahead of the held queue, journaling
    origin+reason; 打断 (`instance.cancel`) ends the turn without a message;
  - blocked (AskUserQuestion / approval / elicitation / plan review pending)
    is NOT working: the composer stays enabled, offers 打断 but never 插队
    (Esc would hit the dialog), and Enter holds with「待回答后送出」until the
    interaction resolves;
  - provision is reported honestly — native Esc, Remuda-sent cancel sequence,
    or 尚未验证 — never a faked button.
  - Queued items render removable chips AND pending transcript rows tagged
    「排队中 · 第 n 条 · 回车后送出」/「待回答后送出」; delivered rows lose
    the tag. Enter = queue, Shift+Enter = newline, ⌘/Ctrl+Enter = 插队,
    Esc while the composer is focused = 打断 with a desktop confirm. 400px fits.
- **Mobile dock (§5.2).** `LocalInput` no longer appends `\r`; it routes
  through `hubStore.send` → `instance.send`, so the driver performs the
  body-then-Enter two writes. Raw-key buttons stay on the binary channel.
- **Node restart (§8 方案 A).** An instance whose `lastError` is
  `node-epoch-changed` shows「Node 重启，会话已结束」with a Resume button
  (`instance.resume`), moves into the 已退出 session-list group, and keeps
  its sidebar exited group.
- **Provenance chips.** `launchedBy` shows a subtle `user`/`remuda` mark on
  sidebar rows, the session header and (as a compact dot) tab chips.
  Provenance only — it never gates capability.
- Fake Node (`crates/remuda-hub/examples/hub_e2e.rs`, minimal Rust): hello
  carries `capabilities.driverInventory`; create/send append a native
  `agent_status` event (working on steer/queue, idle on cancel/new-turn).

## Screenshots (captured by hub-live e2e, REMUDA_EVIDENCE=1)

| | |
|---|---|
| New Session, native shell-pty default + launch preview, 1440px | [native-pty-web-1-new-session-1440.png](./native-pty-web-1-new-session-1440.png) |
| Same sheet at 400px (driver choices wrap, preview scrolls) | [native-pty-web-1-new-session-400.png](./native-pty-web-1-new-session-400.png) |
| Working composer: Enter 排队（Remuda 代持）/ 插队(⌘/Ctrl+↵) / 打断, queued chip, 1440px | [native-pty-web-1-composer-working-1440.png](./native-pty-web-1-composer-working-1440.png) |
| Same at 400px — queue + 插队 + 打断 + removable chip fit the bar | [native-pty-web-1-composer-working-400.png](./native-pty-web-1-composer-working-400.png) |

## Stable testids

| id | meaning |
|---|---|
| `new-session-driver-shell-pty` etc. | driver choice rows; `data-default=0\|1`; disabled when the matrix refuses |
| `new-session-launch-preview` | read-only materialized-argv summary |
| `composer` | attrs add `data-phase=idle\|working\|blocked\|exited` |
| `composer-send` | idle primary send; `data-mode=new-turn` |
| `composer-queue` | busy primary queue (Enter); `data-mode=queue`, `data-holder=remuda\|native` |
| `composer-steer` | c-steer 插队 (interrupt + deliver first); `data-provision=native\|emulated\|unknown`; ⌘/Ctrl+Enter |
| `composer-interrupt` | interrupt; `data-provision=native\|emulated\|unknown` |
| `composer-queued-chip` / `composer-queued-remove` / `composer-queue-status` | held/native queue ledger; chip carries `data-reason=turn\|answer` and `data-ordinal` |
| `held-queue-tag` / `held-queue-cancel` | pending transcript row tag / per-message cancel |
| `composer-interrupted-chip` / `steer-delivered-tag` | post-cancel status / delivered 插队 badge |
| `composer-cap-note` | honest emulation/unverified note |
| `node-restart-banner`, `node-restart-resume` | §8 banner |
| `launched-by` | provenance mark; `data-launched-by=user\|remuda` |

## Verification

- `pnpm test` — 382 unit tests, including the composer state machine across
  native/emulated/unknown × idle/working/blocked, driver-default matrix
  cases, and LocalInput routing.
- `pnpm run test:e2e:hub` — 14 specs green, including the two new D-028 specs
  (native PTY default + both projections; steer/queue/interrupt states and
  wire modes).
