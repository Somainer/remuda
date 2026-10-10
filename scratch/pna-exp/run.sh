#!/usr/bin/env bash
set -u
cd /home/wangruming.nana/Projects/remuda-agents/wt/c-authrace/scratch/pna-exp
pkill -9 -f "pna-exp/server" 2>/dev/null || true
pkill -9 -f "scratch/pna-exp/probe" 2>/dev/null || true
for p in $(ss -ltnp 2>/dev/null | grep ":8099" | grep -oP 'pid=\K[0-9]+' | sort -u); do kill -9 "$p" 2>/dev/null || true; done
sleep 1
BIND="${4:-127.0.0.1}"
if [ "${5:-0}" = "tls" ]; then export TLS=1; else export TLS=0; fi
( PORT=8099 HOST="$BIND" node server.mjs >/tmp/pna-server.log 2>&1 & )
sleep 1
echo "INVOKE node probe.mjs $1 $2 $3 $TLS"
timeout 90 node probe.mjs "$1" "$2" "$3" "$TLS"
echo "RC=$?"
for p in $(ss -ltnp 2>/dev/null | grep ":8099" | grep -oP 'pid=\K[0-9]+' | sort -u); do kill -9 "$p" 2>/dev/null || true; done
