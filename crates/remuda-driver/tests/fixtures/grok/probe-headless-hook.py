# Independently written Grok 1.0.30 hook apparatus; see README.md.
import sys,json,pathlib,os
payload=json.load(sys.stdin)
record={"source":sys.argv[1],"payload":payload,"env":{k:os.environ.get(k) for k in ("GROK_HOOK_EVENT","GROK_HOOK_NAME","GROK_SESSION_ID","GROK_WORKSPACE_ROOT","CLAUDE_PROJECT_DIR")}}
with open("/tmp/remuda-grokspike/hooks/captured.jsonl","a") as f: f.write(json.dumps(record)+"\n")
if payload.get("hookEventName")=="pre_tool_use" and pathlib.Path("/tmp/remuda-grokspike/hooks/deny").exists(): print(json.dumps({"decision":"deny","reason":"SPIKE_DENIED"}))
