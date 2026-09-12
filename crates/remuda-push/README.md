# remuda-push

Web Push for the Remuda Hub. Native sessions remain the resume authority; this crate only wakes a subscribed device.

Hub mounts the fragment:

```rust
Router::new().nest("/push", remuda_push::router(service))
```

| Method | Path | Role |
| --- | --- | --- |
| `GET` | `/push/config` | VAPID public key (`public_key`) |
| `POST` | `/push/subscriptions` | upsert `{endpoint, keys:{p256dh,auth}, deviceId?}` |
| `DELETE` | `/push/subscriptions?endpoint=` | drop that endpoint |

Keys live at `<data_dir>/vapid.json` mode `0600`. Subscriptions are SQLite (`push.sqlite`). Payloads are RFC 8291 `aes128gcm` via the `web-push` crate. `tag` / Topic collapse keys are `interaction:{id}` and `instance:{id}` (Topic is a 32-character hash; the JSON `tag` keeps the full spelling). 404/410 prune the row; 429/5xx retry with backoff.

Behaviour matches herdrx `internal/push` (MIT), rewritten in Rust — not a copy of the Go sources.
