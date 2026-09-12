#!/usr/bin/env bash
# Local OpenSSH stand-in for remuda-ssh tests. Does not open a network connection.
set -euo pipefail
here="$(cd "$(dirname "$0")" && pwd)"
root="${REMUDA_FAKE_REMOTE:-/tmp/remuda-ssh-fake-remote}"
mkdir -p "$root"

is_g=0
remote=()
seen_end=0
alias=""
for arg in "$@"; do
  if [[ "$arg" == "-G" ]]; then
    is_g=1
    continue
  fi
  if [[ "$arg" == "-O" ]]; then
    exit 0
  fi
  if [[ "$seen_end" -eq 1 ]]; then
    remote+=("$arg")
    continue
  fi
  if [[ "$arg" == "--" ]]; then
    seen_end=1
    continue
  fi
  if [[ "$arg" == -* ]]; then
    continue
  fi
  if [[ -z "$alias" ]]; then
    alias="$arg"
  fi
done

if [[ "$is_g" -eq 1 ]]; then
  case "${alias:-devbox-sg}" in
    forge-doloris) cat "$here/ssh-G-forge-doloris.txt" ;;
    *) cat "$here/ssh-G-devbox-sg.txt" ;;
  esac
  exit 0
fi

if [[ ${#remote[@]} -eq 0 ]]; then
  echo "fake-ssh: missing remote command" >&2
  exit 2
fi

export HOME="$root"
# OpenSSH joins remote argv into one shell string; remuda-ssh quotes it as one arg.
if [[ ${#remote[@]} -eq 1 ]]; then
  eval "${remote[0]}"
else
  exec "${remote[@]}"
fi
