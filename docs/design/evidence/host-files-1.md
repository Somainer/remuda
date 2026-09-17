# Read-only host files (ls/get) acceptance — host-files-1

Exercised the end-to-end read-only host-file path with the Hub e2e harness
(`cargo run -p remuda-hub --example hub_e2e`) on `127.0.0.1:58380` and the
real `remuda` CLI built from this branch. The fake Node answers
`host.files.list` from a real seeded directory and implements
`host.files.read` by uploading bytes to `POST /v1/hosts/{id}/files/objects`
with its durable host token — no scripted object ids. No live model is
involved anywhere in this flow.

Seed (created by the harness under the registered workspace root
`/tmp/remuda-e2e`, advertised as `wsp_e2e`):

- `host-files-e2e.txt` — 22 bytes, content `remuda host files e2e\n`
- `host-files-dir/inside.txt` — nested directory entry

## 1. `remuda host files ls` of a registered workspace

Command (device minted from the bootstrap token via the standard client
flow):

```text
$ remuda host files ls e2e-fake-node wsp_e2e \
    --hub http://127.0.0.1:58380 --bootstrap-token <access-code>
/tmp/remuda-e2e
NAME                                  KIND      SIZE        MODE        MODIFIED
host-files-dir                        dir       4096        rwxrwxr-x   2026-09-17T14:32:47Z
host-files-e2e.txt                    file      22          rw-rw-r--   2026-09-17T15:10:22Z
```

The listing came back from the Node through
`GET /v1/hosts/{hostId}/files?workspaceId=wsp_e2e`; entries are reported
from `symlink_metadata`, so kind/size/mode describe the link itself rather
than any target.

## 2. `remuda host files get` of a small file

```text
$ remuda host files get e2e-fake-node wsp_e2e host-files-e2e.txt \
    -o /tmp/remuda-hostfiles-evidence/host-files-e2e.txt \
    --hub http://127.0.0.1:58380 --bootstrap-token <access-code>
{
  "objectId": "obj_01a0afe2-4a0b-74b9-88c5-a0990a4c56be",
  "digest": "9aefb88c39862a4b3f521dfeb8f1875ecd0dfcf056c3dc8079ee46155e653a22",
  "size": 22,
  "writtenTo": "/tmp/remuda-hostfiles-evidence/host-files-e2e.txt"
}
```

The Node read the contained regular file, staged it through the objects
channel with the host token, and the CLI pulled bytes back through the
existing `GET /v1/objects/{objectId}`. The on-disk SHA-256 matches the
staged digest:

```text
$ sha256sum /tmp/remuda-hostfiles-evidence/host-files-e2e.txt
9aefb88c39862a4b3f521dfeb8f1875ecd0dfcf056c3dc8079ee46155e653a22  /tmp/remuda-hostfiles-evidence/host-files-e2e.txt
$ cat /tmp/remuda-hostfiles-evidence/host-files-e2e.txt
remuda host files e2e
```

## 3. A `..` traversal is rejected

```text
$ remuda host files ls e2e-fake-node wsp_e2e ../remuda-e2e-second \
    --hub http://127.0.0.1:58380 --bootstrap-token <access-code>
Error: hub HTTP 400: {"error":"host file path ../remuda-e2e-second escapes the workspace: '..' is not allowed","code":"BAD_REQUEST"}
traversal exit: 1
```

The Node rejects the `..` component before any canonical filesystem call.
Symlink escapes are refused on the canonical path after resolution
(node unit `symlink_pointing_outside_is_rejected`), irregular files
(fifo/socket/device/symlink) fail the regular-file check, and files over
the 25 MiB attachment ceiling fail with `RESOURCE_LIMIT`. The
`/tmp/remuda-*` scratch prefix is accepted while other `/tmp` paths are
rejected (node unit
`scratch_area_accepts_remuda_prefixed_tmp_and_rejects_other_tmp`).

## 4. Secret scan

```text
$ ./scripts/ci/secret-scan.sh
secret-scan: pass
```

## 5. Automated coverage recorded for this slice

- Node unit tests (`remuda-node` `files::tests`, 7): containment success,
  `..` rejection, external symlink rejection, irregular-file rejection,
  oversize rejection, `/tmp/remuda-*` acceptance vs other `/tmp` refusal,
  unknown workspace/method rejection.
- Hub route tests (`tests/host_files.rs`, 5): Human 200 list/read through
  a scripted Node, Agent origin 403 on both routes, unknown host 404 and
  disconnected host 409, host-token staging bound to the path host
  (another host's token 403, anonymous 401, download parity through
  `GET /v1/objects/{id}`), oversize 413 and unsanitized-name 400.
- Playwright (`web/tests/e2e/host-files.hub.spec.ts`, 5 passing against
  the fake Node): real directory listing, end-to-end read with client
  SHA-256 verification against the staged digest, traversal/unknown
  workspace/scratch-scope refusals, scratch-area listing, 404 for an
  unknown host and 401 for an anonymous caller.
