# Independently written Grok 1.0.30 spike apparatus; see README.md.
import json,time,threading
from http.server import BaseHTTPRequestHandler,ThreadingHTTPServer
from pathlib import Path
root=Path('/tmp/remuda-grokspike/tui')
class H(BaseHTTPRequestHandler):
 def log_message(self,*a): pass
 def do_GET(self):
  body=json.dumps({'object':'list','data':[{'id':'spike','object':'model','created':1,'owned_by':'local'}]}).encode(); self.send_response(200);self.send_header('Content-Type','application/json');self.end_headers();self.wfile.write(body)
 def do_POST(self):
  data=json.loads(self.rfile.read(int(self.headers['Content-Length'])))
  with (root/'requests.jsonl').open('a') as f:f.write(json.dumps({'path':self.path,'data':data})+'\n')
  msgs=data.get('messages',[]); last=msgs[-1] if msgs else {}; text=str(last.get('content',''))
  tools=data.get('tools',[])
  prompt=next((str(m.get('content','')) for m in reversed(msgs) if m.get('role')=='user'),'')
  tool_name=None; args={}
  if tools and last.get('role')=='user' and 'RUN_TOOL' in prompt:
   tool_name='run_terminal_command';args={'command':'printf SPIKE_TOOL_OK > spike-result.txt','description':'Write the fixed probe marker in the throwaway directory.'}
  if tools and last.get('role')=='user' and 'QUESTION' in prompt:
   tool_name='ask_user_question';args={'questions':[{'question':'Choose the probe result.','options':[{'label':'Alpha','description':'Record Alpha.'},{'label':'Beta','description':'Record Beta.'}]}]}
  answer='SPIKE_COMPLETE'
  self.send_response(200);self.send_header('Content-Type','text/event-stream');self.end_headers()
  def emit(delta,finish=None):
   self.wfile.write(('data: '+json.dumps({'id':'spike-response','object':'chat.completion.chunk','created':1,'model':'spike','choices':[{'index':0,'delta':delta,'finish_reason':finish}]})+'\n\n').encode());self.wfile.flush()
  try:
   emit({'role':'assistant','reasoning_content':'Checking the probe.'})
   if 'SLOW' in text and last.get('role')=='user': time.sleep(12)
   if tool_name:
    emit({'tool_calls':[{'index':0,'id':'call-spike-'+str(time.time_ns()),'type':'function','function':{'name':tool_name,'arguments':json.dumps(args)}}]});emit({},'tool_calls');self.wfile.write(b'data: [DONE]\n\n');self.wfile.flush();return
   for chunk in ['SPIKE_','COMPLETE']:emit({'content':chunk});time.sleep(.2)
   emit({},'stop');self.wfile.write(b'data: [DONE]\n\n');self.wfile.flush()
  except (BrokenPipeError,ConnectionResetError):pass
server=ThreadingHTTPServer(('127.0.0.1',int((root/'port').read_text()) if (root/'port').exists() else 0),H)
(root/'port').write_text(str(server.server_port))
server.serve_forever()
