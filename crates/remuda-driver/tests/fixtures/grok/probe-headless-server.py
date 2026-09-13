# Independently written Grok 1.0.30 hook apparatus; see README.md.
import json,time
from http.server import BaseHTTPRequestHandler,ThreadingHTTPServer
from pathlib import Path
root=Path("/tmp/remuda-grokspike/hooks")
class H(BaseHTTPRequestHandler):
 def log_message(self,*a): pass
 def do_GET(self):
  body=json.dumps({"object":"list","data":[{"id":"spike","object":"model","created":1,"owned_by":"local"}]}).encode();self.send_response(200);self.send_header("Content-Type","application/json");self.end_headers();self.wfile.write(body)
 def do_POST(self):
  data=json.loads(self.rfile.read(int(self.headers["Content-Length"])))
  with (root/"requests.jsonl").open("a") as f:f.write(json.dumps({"path":self.path,"data":data})+"\n")
  msgs=data.get("messages",[]);last=msgs[-1] if msgs else {};prompt=str(last.get("content",""))
  self.send_response(200);self.send_header("Content-Type","text/event-stream");self.end_headers()
  def emit(delta,finish=None):
   self.wfile.write(("data: "+json.dumps({"id":"hook-response","object":"chat.completion.chunk","created":1,"model":"spike","choices":[{"index":0,"delta":delta,"finish_reason":finish}]})+"\n\n").encode());self.wfile.flush()
  try:
   emit({"role":"assistant","reasoning_content":"Hook probe."})
   if last.get("role")=="user" and "TOOL" in prompt:
    emit({"tool_calls":[{"index":0,"id":"hook-tool-1","type":"function","function":{"name":"run_terminal_command","arguments":json.dumps({"command":"printf HOOK_TOOL_EXECUTED","description":"Emit fixed probe marker"})}}]},"tool_calls")
   else:
    emit({"content":"HOOK_COMPLETE"});emit({},"stop")
   self.wfile.write(b"data: [DONE]\n\n");self.wfile.flush()
  except (BrokenPipeError,ConnectionResetError):pass
server=ThreadingHTTPServer(("127.0.0.1",0),H)
(root/"port").write_text(str(server.server_port))
server.serve_forever()
