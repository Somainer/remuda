#!/usr/bin/env bash
set -euo pipefail
# shellcheck source=deploy/public/common.sh
source "$(dirname -- "${BASH_SOURCE[0]}")/common.sh"
require_root
archive=''
if [[ $# == 2 && $1 == --image-archive ]]; then archive=$2
elif [[ $# != 0 ]]; then die 'Usage: install.sh [--image-archive IMAGE.tar]'; fi
[[ -z $archive || -f $archive ]] || die 'Image archive does not exist.'
[[ $(uname -s) == Linux ]] || die 'Installer requires a fresh Ubuntu 24.04 VPS.'
[[ $(sed -n 's/^ID=//p' /etc/os-release) == ubuntu && $(sed -n 's/^VERSION_ID=//p' /etc/os-release) == '"24.04"' ]] || die 'Installer requires Ubuntu 24.04.'
if ! command -v python3 >/dev/null; then apt-get update; apt-get install -y python3; fi
load_env
lock_operation
export DEBIAN_FRONTEND=noninteractive
apt-get update
apt-get install -y ca-certificates curl python3 dnsutils iproute2 openssl age
if ! command -v docker >/dev/null; then
  # Official Docker apt repository; no downloaded installation script execution.
  install -m 0755 -d /etc/apt/keyrings
  curl --fail --silent --show-error --location --proto '=https' --tlsv1.2 \
    https://download.docker.com/linux/ubuntu/gpg -o /etc/apt/keyrings/docker.asc
  chmod 0644 /etc/apt/keyrings/docker.asc
  cat > /etc/apt/sources.list.d/docker.sources <<EOF
Types: deb
URIs: https://download.docker.com/linux/ubuntu
Suites: noble
Components: stable
Architectures: $(dpkg --print-architecture)
Signed-By: /etc/apt/keyrings/docker.asc
EOF
  apt-get update
  apt-get install -y docker-ce docker-ce-cli containerd.io docker-buildx-plugin docker-compose-plugin
fi
systemctl enable --now docker
"$PUBLIC_DIR/preflight.sh"
[[ ! -e $DATA_DIR/hub.sqlite ]] || die 'Existing Hub database found; use upgrade.sh.'
install -d -o 65532 -g 65532 -m 0700 "$DATA_DIR"
if [[ ! -e $DATA_DIR/bootstrap-token ]]; then
  # Secret stays in a private file; it is never printed or put in a URL/env/argv.
  ( set -o noclobber; openssl rand -hex 32 > "$DATA_DIR/bootstrap-token" )
fi
[[ -f $DATA_DIR/bootstrap-token && ! -L $DATA_DIR/bootstrap-token && -s $DATA_DIR/bootstrap-token ]] || die 'Invalid bootstrap-token file.'
chown 65532:65532 "$DATA_DIR/bootstrap-token"
chmod 0600 "$DATA_DIR/bootstrap-token" "$ENV_FILE"
if [[ -n $archive ]]; then docker load --input "$archive"; fi
if ! docker image inspect "$HUB_IMAGE" >/dev/null 2>&1; then compose pull remuda-hub; fi
compose pull caddy
compose up -d --wait --wait-timeout 180
check_volume
curl --fail --silent --show-error --retry 12 --retry-all-errors --retry-delay 5 \
  --connect-timeout 5 --max-time 15 "https://$HUB_DOMAIN/healthz" >/dev/null
printf 'Bootstrap access code file: %s/bootstrap-token\n' "$DATA_DIR"
printf 'First login: https://%s/login\nPairing URL: https://%s/login?pair\n' "$HUB_DOMAIN" "$HUB_DOMAIN"
printf 'Log in with the file value, then generate a short-lived phone pairing code in Settings.\n'
