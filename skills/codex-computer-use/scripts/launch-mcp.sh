#!/bin/sh
# Reuse the installed runtime; do not copy the proprietary application into this skill.
set -eu

if [ "$(uname -s)" != "Darwin" ]; then
  echo 'codex-computer-use: this launcher requires macOS.' >&2
  exit 1
fi

# Same real-home rule as launch-cua-repl.sh: when Remuda shadows the agent's
# CODEX_HOME, the operator's real home arrives as REMUDA_CODEX_HOME.
cu_root="${REMUDA_CODEX_HOME:-${CODEX_HOME:-${HOME}/.codex}}"
cu_client="${cu_root}/computer-use/Codex Computer Use.app/Contents/SharedSupport/SkyComputerUseClient.app/Contents/MacOS/SkyComputerUseClient"

if [ ! -x "$cu_client" ]; then
  echo 'codex-computer-use: installed Codex Computer Use client was not found.' >&2
  echo 'Install/enable Computer Use in Codex, or set CODEX_HOME to its existing installation.' >&2
  exit 1
fi

exec "$cu_client" mcp
