#!/bin/sh
# Codex Computer Use JS surface (cua_repl). Use when the native Sky MCP
# helper rejects the host as unauthenticated. Does not copy the app.
#
# This launcher grants desktop control: the REPL it starts evaluates
# arbitrary JS with click / typeText / pressKey bindings over every app on
# the operator's desktop. It therefore refuses to start unless a Remuda
# launch explicitly requested the capability, which is signalled by
# injecting REMUDA_CAPABILITY_COMPUTER_USE=1 into the child environment.
set -eu

if [ "${REMUDA_CAPABILITY_COMPUTER_USE:-}" != "1" ]; then
  echo 'codex-computer-use: refusing to start the cua-repl desktop-control surface.' >&2
  echo 'It evaluates arbitrary JS with click/typeText/pressKey over every app on this desktop.' >&2
  echo 'A Remuda launch grants it with --capability computer-use, which sets' >&2
  echo 'REMUDA_CAPABILITY_COMPUTER_USE=1 for the agent. There is no interactive bypass.' >&2
  exit 1
fi

if [ "$(uname -s)" != "Darwin" ]; then
  echo 'codex-computer-use: cua-repl launcher requires macOS.' >&2
  exit 1
fi

cua_node="${CUA_NODE:-/Applications/ChatGPT.app/Contents/Resources/cua_node}"
node="${cua_node}/bin/node"
repl="${cua_node}/lib/node_modules/@oai/cua-repl/bin/cua-repl.mjs"
node_repl="${cua_node}/bin/node_repl"
# The vendor app lives in the *operator's* real codex home. When Remuda
# shadows CODEX_HOME for the agent (per-instance shadow home), it passes the
# resolved real home as REMUDA_CODEX_HOME on the MCP server entry only; the
# shadow value must never be used to locate the vendor app.
codex_home="${REMUDA_CODEX_HOME:-${CODEX_HOME:-${HOME}/.codex}}"
sky_app="${codex_home}/computer-use/Codex Computer Use.app"

if [ ! -x "$node" ] || [ ! -f "$repl" ] || [ ! -x "$node_repl" ]; then
  echo 'codex-computer-use: ChatGPT cua_node runtime was not found.' >&2
  echo 'Install ChatGPT/Codex desktop, or set CUA_NODE to its cua_node directory.' >&2
  exit 1
fi

if [ ! -d "$sky_app" ]; then
  echo 'codex-computer-use: Codex Computer Use.app was not found under CODEX_HOME.' >&2
  echo 'Enable Computer Use in Codex, or set CODEX_HOME to the existing install.' >&2
  exit 1
fi

export CODEX_HOME="$codex_home"
export CUA_REPL_ENABLED_SURFACES="${CUA_REPL_ENABLED_SURFACES:-computer}"
export CUA_REPL_NODE_REPL_PATH="$node_repl"
export NODE_REPL_NODE_PATH="$node"
export NODE_REPL_NODE_MODULE_DIRS="${cua_node}/lib/node_modules"
export NODE_REPL_TRUSTED_CODE_PATHS="${codex_home}:${cua_node}/lib/node_modules"
export SKY_CUA_SERVICE_PATH="$sky_app"

exec "$node" "$repl"
