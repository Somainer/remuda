# Hub authentication hardening

Security review 2 tasks 1, 2, 10, 11, 15, and 17 are implemented on the Hub and Node boundaries.

- Static disk assets and the SPA index must resolve inside the canonical web root. Raw parent traversal, absolute paths, NUL, and backslashes are rejected before file access.
- Bootstrap enrolls new hosts. Existing host identities require their own host token.
- All Hub responses carry CSP, `frame-ancestors 'none'`, `X-Frame-Options: DENY`, `X-Content-Type-Options: nosniff`, and `Referrer-Policy: no-referrer`. IBM Plex fonts are bundled locally. CSP permits inline styles for React and xterm layout; scripts remain restricted to same-origin files.
- Browser REST and follow connections use the HttpOnly, SameSite=Strict device cookie. REST also accepts native-client bearer headers. Follow query parameters never authenticate. Device tokens returned at login/pairing stay in memory; localStorage retains only device metadata and removes legacy token copies. Self-revocation expires the cookie on the server.
- The Node checks every logical `tty.write` key before enqueueing a command. The accepted vocabulary is the CLI key encoder's named keys, `ctrl+a` through `ctrl+z`, and printable non-whitespace single characters. Raw terminal byte streams have a separate transport contract.

## Authentication attempt budgets

`POST /v1/login` and `POST /v1/devices/pair` each allow a burst of 10 requests per peer IP, replenishing one request every 10 seconds. A shared global bucket allows a burst of 64 and replenishes 8 requests per second. Every attempt counts, including successful and malformed requests. Denial returns HTTP 429 with `Retry-After: 10`. The limits run before JSON parsing or secret verification.

The address is the TCP peer supplied by Axum `ConnectInfo`. Caller-supplied `Forwarded` and `X-Forwarded-For` are ignored. Users behind a reverse proxy share that proxy's per-IP budgets. Missing peer metadata fails closed on these endpoints. IPv4-mapped IPv6 addresses share the corresponding IPv4 budget. Bucket storage is capped at 4096 entries; entries idle for 10 minutes can be reclaimed.

## Indexed verification and upgrades

Host and device tokens retain their existing 64-hex format. Their first 16 characters select one row through a unique SQLite index; authentication still verifies the entire token against its salted Argon2 hash. A missing selector performs no Argon2 verification. No token scan remains.

Eight-character pairing codes use OS randomness and a 32-symbol alphabet. A unique four-character selector chooses one active candidate; issuance retries selector collisions. Argon2 checks the full code. Ten failed checks against a candidate lock it out. Codes expire after 10 minutes and can be consumed only once. Used, expired, and locked codes are removed when issuing another code.

Existing salted hashes cannot be backfilled without presenting the original secret. The schema migration keeps existing records and adds nullable selectors:

- Legacy hosts send their persisted `hostId`; the Hub verifies that single row and indexes it after successful authentication. Bootstrap does not regain access to an existing identity.
- The Web REST client sends `X-Remuda-Device-Id` from its non-secret stored metadata. A legacy cookie can migrate by verifying that one device row. Later REST and WebSocket requests use the index without this hint.
- Legacy device clients that lack a device identifier must log in or pair again. The identifier alone grants no access. Pairing codes issued before this migration must be reissued.

The production-browser smoke command is `pnpm build`, then `pnpm exec playwright test -c playwright.security.config.ts` from `web/`, with the agent's isolated Cargo target environment. It serves the built app through the real Hub on port 58880 by default and uses a fake Node; it does not invoke a model.
