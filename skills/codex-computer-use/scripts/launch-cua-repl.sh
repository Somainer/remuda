#!/bin/sh
# Codex Computer Use JS surface (cua_repl). Use when the native Sky MCP
# helper rejects the host as unauthenticated. Does not copy the app.
set -eu

if [ "$(uname -s)" != "Darwin" ]; then
  echo 'codex-computer-use: cua-repl launcher requires macOS.' >&2
  exit 1
fi

cua_node="${CUA_NODE:-/Applications/ChatGPT.app/Contents/Resources/cua_node}"
node="${cua_node}/bin/node"
repl="${cua_node}/lib/node_modules/@oai/cua-repl/bin/cua-repl.mjs"
node_repl="${cua_node}/bin/node_repl"
codex_home="${CODEX_HOME:-${HOME}/.codex}"
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
