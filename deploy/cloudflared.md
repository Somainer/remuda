# Cloudflare Tunnel → existing Caddy :443

Phone and bot callbacks need a public HTTPS name. The Hub host already
terminates TLS on Caddy (`0.0.0.0:443`) with Let's Encrypt via Cloudflare
DNS-01, but the A record is a private address, so cellular networks cannot
reach it. A **named Cloudflare Tunnel** publishes `remuda.example.com`
without opening any extra inbound ports (no 80/443 security-group change,
no frp, no host `ports:` on the Hub container).

Do not put a Tunnel in front of every Node. Nodes dial Hub outbound WSS.

## Layout

```
phone / bot  --HTTPS 443-->  Cloudflare edge
                                  |
                                  |  named tunnel (outbound from sg-host)
                                  v
                           cloudflared (host or compose)
                                  |
                                  |  https://127.0.0.1:443
                                  v
                           existing Caddy (deploy-caddy-1)
                                  |
                                  |  remuda.example.com server block
                                  v
                           remuda-hub:8080  (Docker network deploy_default)
```

cloudflared originates the connection to Cloudflare. The host remains
inbound-closed on the public Internet.

## Named tunnel config

Install `cloudflared` on `devbox-sg-host` (or a tiny sidecar on
`deploy_default`). Create one named tunnel; do not reuse an unrelated
tunnel credential.

`/etc/cloudflared/config.yml`:

```yaml
tunnel: <REMUDA_TUNNEL_UUID>
credentials-file: /etc/cloudflared/<REMUDA_TUNNEL_UUID>.json
ingress:
  - hostname: remuda.example.com
    service: https://127.0.0.1:443
    originRequest:
      originServerName: remuda.example.com
      httpHostHeader: remuda.example.com
      noTLSVerify: false
  - service: http_status:404
```

The catch-all `http_status:404` is required. Origin is **Caddy 443**, not
the Hub container and not AsterGate `:8080`. Caddy keeps Host checks,
HSTS, and the Remuda site block. Pointing the Tunnel at Hub:8080 would
skip that boundary.

If cloudflared runs in Docker on `deploy_default`, `service` may be
`https://deploy-caddy-1:443` with the same `originServerName`.

HTTP origin `http://127.0.0.1:80` also works (Caddy currently serves HTTP
without redirect on the existing site). Prefer 443 so the path matches
production TLS.

## DNS and Access

- Create a CNAME (or Cloudflare-managed route) `remuda.example.com` →
  `<tunnel-id>.cfargotunnel.com`. Do not publish a public A record for
  the private Hub IP.
- Existing `CF_API_TOKEN` on Caddy is DNS-edit for the zone. Remuda
  containers must not receive it. Tunnel credentials are a different
  file, mounted only into cloudflared, mode `0400`.
- Optional: Cloudflare Access in front of the hostname. Hub cookie
  auth still applies; Access is an extra gate, not a replacement.
- Verify WebSocket upgrade (`wss://remuda.example.com/...`), SSE flush,
  long-lived connections, and that a phone on cellular (not corp VPN)
  can load the PWA. Cloudflare 200 only proves the edge accepted TCP.

## What not to do

- Do not open host 80/443 to the world; they are already bound, but the
  public path must be the Tunnel, not a public A record.
- Do not add Hub `ports:` in compose.
- Do not route AsterGate `/v1` through this hostname.
- Do not copy tunnel JSON into Hub env, image layers, or backups.
- Rotate the tunnel credential if it leaks; Hub master key is independent.
