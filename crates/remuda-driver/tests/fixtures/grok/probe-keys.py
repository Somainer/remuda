# Independently written Grok 1.0.30 spike apparatus; see README.md.
import subprocess,time,json,sys
from pathlib import Path
root=Path('/tmp/remuda-grokspike/tui');pane=sys.argv[1];mode=sys.argv[2]
def cmd(*args):return subprocess.run(['herdr','pane',*args],check=True,capture_output=True).stdout
def text(s):cmd('send-text',pane,s);time.sleep(.7)
def key(*s):cmd('send-keys',pane,*s)
def snap(name):
 b=cmd('read',pane,'--source','visible','--lines','35','--format','ansi');(root/(name+'.ansi')).write_bytes(b)
 print(cmd('read',pane,'--source','visible','--lines','13').decode())
text('SLOW_'+mode);key('enter');time.sleep(1.2);snap(mode+'-working')
if mode=='QUEUE':
 text('QUEUED_FOLLOWUP');snap('queue-typed');key('enter');time.sleep(.6);snap('queue-submitted')
elif mode=='INTERRUPT':
 text('UNSENT_DRAFT');key('esc');time.sleep(.4);snap('esc-draft');key('ctrl+c');time.sleep(.4);snap('ctrlc-cleared');key('ctrl+c');time.sleep(.6);snap('ctrlc-cancelled')
elif mode=='SENDNOW':
 text('NOW_FOLLOWUP');key('enter');time.sleep(.3);key('enter');time.sleep(.7);snap('sendnow')
