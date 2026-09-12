#!/usr/bin/env bash
set -euo pipefail
# shellcheck source=deploy/public/common.sh
source "$(dirname -- "${BASH_SOURCE[0]}")/common.sh"
require_root
load_env
check_docker
need dig
need ip
need ss
[[ -z $(ss -H -ltn '( sport = :80 or sport = :443 )') ]] || die 'TCP ports 80/443 are occupied.'
[[ ! $(docker ps --format '{{.Ports}}') =~ :80-\>|:443-\> ]] || die 'Docker already publishes port 80 or 443.'

# Prefer the directly assigned public IPv4; NAT VPSs need the console-provided IP.
if [[ -z ${VPS_PUBLIC_IP:-} ]]; then
  VPS_PUBLIC_IP=$(ip -j -4 address show | python3 -c '
import ipaddress,json,sys
values={a["local"] for i in json.load(sys.stdin) for a in i["addr_info"] if ipaddress.ip_address(a["local"]).is_global}
if len(values)!=1: sys.exit("Set VPS_PUBLIC_IP from the provider console; no unique directly assigned public IPv4.")
print(values.pop())')
fi
python3 - "$VPS_PUBLIC_IP" <<'PY'
import ipaddress, sys
address = ipaddress.IPv4Address(sys.argv[1])
if not address.is_global:
    sys.exit('VPS_PUBLIC_IP must be a public IPv4 address.')
PY
records=$(dig +time=3 +tries=1 +short A "$HUB_DOMAIN")
python3 - "$VPS_PUBLIC_IP" "$records" <<'PY'
import ipaddress, sys
addresses = set()
for line in sys.argv[2].splitlines():
    try: addresses.add(str(ipaddress.IPv4Address(line)))
    except ValueError: pass  # CNAME lines precede the final A records.
if addresses != {sys.argv[1]}:
    sys.exit('DNS A records must all equal VPS_PUBLIC_IP before requesting an ACME certificate.')
PY
[[ -z $(dig +time=3 +tries=1 +short AAAA "$HUB_DOMAIN" | sed '/\.$/d') ]] || die 'This preflight supports IPv4 only: remove AAAA or validate dual-stack separately before using the package.'
printf 'Preflight passed: Docker available, TCP 80/443 free, DNS A matches VPS public IPv4.\n'
