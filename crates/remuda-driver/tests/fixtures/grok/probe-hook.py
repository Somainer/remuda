# Independently written Grok 1.0.30 spike apparatus; see README.md.
import sys,json,os
payload=json.load(sys.stdin)
record={"source":sys.argv[1],"payload":payload,"env":{k:os.environ.get(k) for k in ("GROK_HOOK_EVENT","GROK_HOOK_NAME","GROK_SESSION_ID","GROK_WORKSPACE_ROOT","CLAUDE_PROJECT_DIR")}}
with open("/tmp/remuda-grokspike/tui/hooks-captured.jsonl","a") as f: f.write(json.dumps(record)+"\n")
if payload.get("hookEventName")=="pre_tool_use" and payload.get("toolName")=="run_terminal_command" and "SPIKE_TOOL_OK" in payload.get("toolInput",{}).get("command",""): print(json.dumps({"decision":"ask","reason":"SPIKE_PERMISSION_PROBE"}))
