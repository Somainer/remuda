#!/usr/bin/env bash
# Local disposable HTTPS smoke; never loads production .env or contacts a VPS.
set -euo pipefail
cd "$(dirname "${BASH_SOURCE[0]}")/../.."
umask 077
scratch=$(mktemp -d)
export COMPOSE_PROJECT_NAME="remuda-public-ci-$$"
export HUB_IMAGE=${HUB_IMAGE:-remuda-hub:ci}
export HUB_DOMAIN=localhost ACME_EMAIL=ci@example.invalid
export DATA_DIR=/var/lib/remuda-public-ci
export PROXY_IP=${CI_PROXY_IP:-172.30.29.2}
export PROXY_SUBNET=${CI_PROXY_SUBNET:-172.30.29.0/24}
compose=(docker compose --env-file "$scratch/env" -f deploy/public/docker-compose.yml -f deploy/public/compose.ci.yml)
: > "$scratch/env"
cleanup() {
  local status=$?
  if (( status != 0 )); then "${compose[@]}" logs --no-color >&2 || true; fi
  "${compose[@]}" down --volumes --remove-orphans >/dev/null || true
  rm -rf "$scratch"
  return "$status"
}
trap cleanup EXIT
docker compose --env-file "$scratch/env" -f deploy/public/docker-compose.yml config --format json > "$scratch/production.json"
"${compose[@]}" config --format json > "$scratch/ci.json"
python3 - "$scratch/production.json" "$scratch/ci.json" <<'PY'
import json, sys
prod, ci = (json.load(open(path)) for path in sys.argv[1:])
assert not prod['services']['remuda-hub'].get('ports'), 'Hub must not publish ports'
assert {p['published'] for p in prod['services']['caddy']['ports']} == {'80', '443'}
assert all(p['host_ip'] == '127.0.0.1' for p in ci['services']['caddy']['ports'])
assert not ci['volumes']['hub_data'].get('driver_opts'), 'CI must use disposable data'
PY
# Check the production config grammar without starting it or requesting ACME.
docker run --rm --entrypoint caddy \
  -e HUB_DOMAIN=hub.example.invalid -e ACME_EMAIL=ci@example.invalid \
  -v "$PWD/deploy/public:/etc/caddy:ro" "${CADDY_IMAGE:-caddy:2-alpine}" \
  validate --config /etc/caddy/Caddyfile
"${compose[@]}" up -d --wait --wait-timeout 120
"${compose[@]}" cp caddy:/data/caddy/pki/authorities/local/root.crt "$scratch/root.crt"
port=${CI_HTTPS_PORT:-18443}
url="https://localhost:$port"
curl_args=(--silent --show-error --fail --noproxy '*' --cacert "$scratch/root.crt" --resolve "localhost:$port:127.0.0.1")
curl "${curl_args[@]}" -D "$scratch/headers" "$url/healthz" > "$scratch/health.json"
python3 - "$scratch/health.json" "$scratch/headers" <<'PY'
import json, sys
assert json.load(open(sys.argv[1])) == {'ok': True}
headers = open(sys.argv[2]).read().lower()
assert 'strict-transport-security: max-age=31536000' in headers
assert 'x-content-type-options: nosniff' in headers
PY
"${compose[@]}" cp remuda-hub:/data/bootstrap-token "$scratch/bootstrap"
python3 - "$scratch/bootstrap" "$scratch/login.json" <<'PY'
import json, sys
with open(sys.argv[2], 'w') as out:
    json.dump({'bootstrapToken': open(sys.argv[1]).read().strip()}, out)
PY
curl "${curl_args[@]}" -D "$scratch/cookies" -H 'Content-Type: application/json' \
  -H "Origin: $url" --data-binary "@$scratch/login.json" "$url/v1/login" > /dev/null
python3 - "$scratch/cookies" <<'PY'
import sys
headers = open(sys.argv[1]).read().lower()
cookie = next(line for line in headers.splitlines() if line.startswith('set-cookie: remuda_device='))
assert all(flag in cookie for flag in ('; secure', '; httponly', '; samesite=strict'))
PY
echo 'deploy-public: compose, internal-CA HTTPS, health, headers and Secure login cookie passed'
