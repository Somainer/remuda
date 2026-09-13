# Spike test apparatus; see README.md for provenance and evidence limits.
import subprocess,time,pathlib,json,sys
root=pathlib.Path('/tmp/remuda-codexspike/tui'); pane=__import__('os').environ['SPIKE_PANE']
def run(*args):
 return subprocess.check_output(['herdr','pane',*args],text=True)
def snap(name):
 data=run('read',pane,'--source','recent-unwrapped','--lines','90')
 (root/(name+'.txt')).write_text(data)
 print(data[-5500:])
def text(s): run('send-text',pane,s); time.sleep(.7)
def key(s): run('send-keys',pane,s)
mode=sys.argv[1]
text(mode+'_BASE'); key('enter'); time.sleep(1.5)
if mode=='STEER':
 text('STEER_FOLLOWUP'); snap('steer-before'); key('enter'); time.sleep(1); snap('steer-after')
elif mode=='QUEUE':
 text('QUEUED_FOLLOWUP'); snap('queue-before'); key('tab'); time.sleep(1); snap('queue-after')
elif mode=='ESC':
 snap('esc-before'); key('esc'); time.sleep(1); snap('esc-after')
elif mode=='APPROVAL': snap('approval-before')
