#!/bin/sh
# Remuda-authored fake-claude stdout gate: keep the native turn in flight until
# the test releases it after disconnecting the entire bridge peer.
case "$1" in --version|-V) printf '2.1.268 (Claude Code)\n'; exit 0;; esac
"$FAKE_CLAUDE_TEST_BINARY" "$@" | while IFS= read -r line; do
    case "$line" in
        *'"type":"assistant"'*)
            printf 'ready\n' > "$FAKE_CLAUDE_TEST_GATE"
            while [ ! -f "$FAKE_CLAUDE_TEST_RELEASE" ]; do sleep 0.02; done
            ;;
    esac
    printf '%s\n' "$line"
done
