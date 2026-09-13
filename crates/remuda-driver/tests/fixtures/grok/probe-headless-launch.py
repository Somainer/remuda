# Independently written Grok 1.0.30 hook apparatus; see README.md.
import os,pathlib,sys
base=pathlib.Path("/tmp/remuda-grokspike/hooks");port=(base/"port").read_text()
env={"PATH":"/usr/bin:/bin:/usr/sbin:/sbin:/opt/homebrew/bin","TERM":"xterm-256color","GROK_HOME":str(base/"home"),"GROK_LEADER_SOCKET":str(base/"leader.sock"),"XAI_API_KEY":"remuda-local-placeholder","GROK_MODELS_BASE_URL":f"http://127.0.0.1:{port}/v1","GROK_XAI_API_BASE_URL":f"http://127.0.0.1:{port}/v1","GROK_TELEMETRY_ENABLED":"0","GROK_DISABLE_AUTOUPDATER":"1","GROK_MANAGED_MCPS_ENABLED":"0","GROK_TITLE_REFRESH":"0","GROK_PROMPT_SUGGESTIONS":"0","GROK_AUTO_WAKE":"0","GROK_IDLE_NOTIFICATION_DELAY_MS":"1000","GROK_FOLDER_TRUST":"0"}
for vendor in ["CLAUDE","CURSOR","CODEX"]:
 for capability in ["HOOKS","MCPS","RULES","AGENTS","SKILLS","SESSIONS"]:env[f"GROK_{vendor}_{capability}_ENABLED"]="0"
os.chdir(base/"work")
os.execve(os.environ["GROK_BIN"],["grok","--model","spike","--no-subagents","--disable-web-search"]+sys.argv[1:],env)
