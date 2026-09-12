# remuda-feishu

Feishu dispatcher adapter for Remuda (M2 predecessor). This crate is a **channel adapter**: it normalizes inbound events, renders Card JSON 2.0, and wraps `lark-cli` outbound calls. It does **not** run an agent loop, create Feishu apps, send live messages, or change `lark-cli` configuration.

Default outbound mode is **DryRun** (argv is recorded, nothing is executed).

## Independent app (required)

Feishu long-connection delivery is cluster mode: one event is delivered to **one** client of that `app_id`. Sharing an app with meeting notes / another bot will steal events.

1. Open [Feishu Open Platform](https://open.feishu.cn/) → create a **new enterprise custom app** used only by Remuda.
2. Bind a dedicated `lark-cli` profile (do this yourself; this crate never runs it):

   ```text
   lark-cli config init --new
   ```

3. In the developer console, subscribe:

   - Event: `im.message.receive_v1`
   - Callback: `card.action.trigger` (JSON 2.0 cards; old “message card callbacks” cannot use the WS bus)

4. Visibility: restrict the app to the owner. Do not publish it as a store app.

`lark-cli event consume` needs stdin kept open (EOF = graceful exit) and should be stopped with **SIGTERM**, never `kill -9` (leaks server-side subscriptions). This crate’s `ConsumeSupervisor` does both. One consume process per EventKey.

## Scopes

Request these on the dedicated app (bot identity):

| Scope | Why |
| --- | --- |
| `im:message` | Send/reply as the bot |
| `im:message.p2p_msg:readonly` | Receive p2p (`im.message.receive_v1` schema) |
| `im:message.group_at_msg` | Receive group `@bot` (ask for `im:message.group_msg` only if you must see un-@’d group traffic) |
| `im:chat:read` | Chat metadata |
| `im:resource` | Download images/files |
| `im:message:readonly` | Card action consume (`card.action.trigger` schema) |
| `cardkit:card:write` | Optional CardKit streaming (v1 progress cards are **static**; streaming is a reserved interface) |

Also enable `im:message:send_as_bot` if the console lists it separately.

## Session key

```text
feishu:{chat_id}:{thread_id || root_id || main}
```

Topic groups must consider `root_id` when `thread_id` is absent. Replies in a topic should use `lark-cli im +messages-reply --reply-in-thread`.

Inbound idempotency: IM uses `message_id` (not `event_id`). Card deliveries prefer `event_id`.

## Commands

Owner-only, always win over heuristics: `/new` `/host` `/agent` `/model` `/status` `/stop` `/yes` `/no`.

Allowlist: `owner_open_ids`, `chat_allowlist`. Groups require `@bot` unless `allow_unaddressed`. Card clicks re-check `operator_id`.

## Cards and Interaction expiry

JSON 2.0 templates: approval (`Allow` / `Deny` / `Allow once`, `behaviors.callback.value = {tid, a}`), AskUserQuestion (native `form` + one submit), static progress, completion, recorded, expired.

Runtime ticket TTL is **10–15 minutes** (default 12), shorter than the Feishu card token (30 min / 2 updates). First valid answer wins; later clicks are `already answered`.

## Tests

All tests are offline (fixtures + `fake-lark-cli.sh`). Do not point this crate at a live app from CI.
