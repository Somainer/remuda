# Spike test apparatus; see README.md for provenance and evidence limits.
import json, pathlib, time, threading
from http.server import ThreadingHTTPServer, BaseHTTPRequestHandler
root=pathlib.Path('/tmp/remuda-codexspike/tui')
lock=threading.Lock()
class Handler(BaseHTTPRequestHandler):
 def log_message(self,*args): pass
 def do_POST(self):
  body=json.loads(self.rfile.read(int(self.headers.get('content-length','0'))))
  with lock:
   with (root/'requests.jsonl').open('a') as f: f.write(json.dumps(body)+'\n')
  inputs=body.get('input',[])
  users=[i for i in inputs if i.get('role')=='user']
  latest=' '.join(c.get('text','') for c in users[-1].get('content',[]) if isinstance(c,dict)) if users else ''
  marker=next((m for m in ['RUN_TOOL','STEER_BASE','QUEUE_BASE','ESC_BASE','APPROVAL_BASE'] if m in latest),None)
  call='spike_'+marker.lower() if marker else ''
  done=any(i.get('call_id')==call and i.get('type')=='function_call_output' for i in inputs)
  rid='resp_'+str(time.time_ns())
  events=[{'type':'response.created','response':{'id':rid}}]
  if marker and not done:
   cmd='printf "SPIKE_TOOL_OK thread=%s\\n" "$CODEX_THREAD_ID"'
   if marker in ['STEER_BASE','QUEUE_BASE','ESC_BASE']: cmd='sleep 20; printf "'+marker+'_TOOL_DONE\\n"'
   args={'cmd':cmd,'yield_time_ms':10000,'max_output_tokens':1000}
   if marker=='APPROVAL_BASE':
    args['sandbox_permissions']='require_escalated'; args['justification']='Approve the throwaway spike command that prints SPIKE_TOOL_OK?'
   events.append({'type':'response.output_item.done','item':{'type':'function_call','call_id':call,'name':'exec_command','arguments':json.dumps(args)}})
  else:
   events.append({'type':'response.output_item.done','item':{'type':'reasoning','id':'rs_spike','summary':[{'type':'summary_text','text':'Deterministic spike reasoning.'}]}})
   events.append({'type':'response.output_item.done','item':{'type':'message','id':'msg_spike','role':'assistant','content':[{'type':'output_text','text':'SPIKE_COMPLETE '+latest[:100]}]}})
  events.append({'type':'response.completed','response':{'id':rid,'usage':{'input_tokens':100,'output_tokens':20,'total_tokens':120,'input_tokens_details':{'cached_tokens':10},'output_tokens_details':{'reasoning_tokens':5}}}})
  data=''.join('event: '+e['type']+'\ndata: '+json.dumps(e)+'\n\n' for e in events).encode()
  self.send_response(200); self.send_header('Content-Type','text/event-stream'); self.send_header('Content-Length',str(len(data))); self.end_headers()
  try: self.wfile.write(data)
  except BrokenPipeError: pass
server=ThreadingHTTPServer(('127.0.0.1',0),Handler)
(root/'port').write_text(str(server.server_port))
server.serve_forever()
