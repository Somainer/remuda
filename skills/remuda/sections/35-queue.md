## queued commands and runtime config

A send to a worker whose host is offline, or that its Node holds while the
worker is busy, has not reached the harness yet. Until it does you can take
it back:

```bash
remuda cmd ls                                   # your queued commands, with revision and held
remuda cmd edit cmd_… --text "…" --revision 0   # same commandId, revision + 1
remuda cmd withdraw cmd_…                       # settles withdrawn, never delivered
remuda cmd deliver cmd_…                        # host back: forward it once, same id
```

You see only what your lineage queued; the owner sees everyone's. Anything
already delivered answers 409 `already-delivered`: read the command, then
decide; never resend it under a new id. A queued configure cannot be edited:
withdraw it and send a new one.

Read before you change a worker's model or effort, and pass the generation
you read, so you never silently overwrite the owner (or they you):

```bash
remuda instance config worker-a                 # values, generation, pending
remuda instance configure worker-a --effort high --expected-generation 3
```

A 409 `generation-conflict` carries the current values: decide again from
them. A `deferred` answer names when it takes effect (`next-turn`: the
worker's next idle moment; `next-chapter`: its next launch).
