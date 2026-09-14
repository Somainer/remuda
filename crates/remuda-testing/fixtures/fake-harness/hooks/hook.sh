#!/bin/sh
# fake-harness round-trip hook (D-028 P7 fixture).
#
# Appends one JSON line per invocation to $FAKE_HARNESS_HOOK_LOG containing the
# received stdin payload plus identity environment variables. For permission
# events it optionally blocks until $FAKE_HARNESS_HOOK_WAIT_FILE appears
# (proving synchronous blocking) and then returns the decision named by
# $FAKE_HARNESS_HOOK_DECISION, in the dialect's documented response shape
# ($FAKE_HARNESS_HOOK_STYLE: claude|codex|grok).
set -u

event_log="${FAKE_HARNESS_HOOK_LOG:-/tmp/fake-harness-hook.log}"
payload="$(cat)"
event_name="$(printf '%s' "$payload" | sed -n 's/.*"hook_event_name"[[:space:]]*:[[:space:]]*"\([^"]*\)".*/\1/p')"
style="${FAKE_HARNESS_HOOK_STYLE:-claude}"

mkdir -p "$(dirname "$event_log")" 2>/dev/null || true
{
  printf '{"event":"%s","env":{' "$event_name"
  printf '"GROK_HOOK_EVENT":"%s","GROK_SESSION_ID":"%s","GROK_WORKSPACE_ROOT":"%s","CLAUDE_PROJECT_DIR":"%s"' \
    "${GROK_HOOK_EVENT:-}" "${GROK_SESSION_ID:-}" "${GROK_WORKSPACE_ROOT:-}" "${CLAUDE_PROJECT_DIR:-}"
  printf '},"payload":%s}\n' "$payload"
} >> "$event_log"

case "$event_name" in
  PermissionRequest|PreToolUse)
    if [ -n "${FAKE_HARNESS_HOOK_WAIT_FILE:-}" ]; then
      i=0
      while [ ! -e "$FAKE_HARNESS_HOOK_WAIT_FILE" ] && [ "$i" -lt 500 ]; do
        sleep 0.02
        i=$((i + 1))
      done
    fi
    decision="${FAKE_HARNESS_HOOK_DECISION:-}"
    if [ -n "$decision" ]; then
      case "$style" in
        codex)
          printf '{"hookSpecificOutput":{"hookEventName":"PermissionRequest","decision":{"behavior":"%s","message":"fake-harness hook %s"}}}' \
            "$decision" "$decision"
          ;;
        grok)
          printf '{"decision":"%s","reason":"fake-harness hook %s"}' "$decision" "$decision"
          ;;
        *)
          printf '{"behavior":"%s","message":"fake-harness hook %s"}' "$decision" "$decision"
          ;;
      esac
    fi
    ;;
esac
exit 0
