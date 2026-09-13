# Spike test apparatus; see README.md for provenance and evidence limits.
import json,pathlib,sys
with pathlib.Path('/tmp/remuda-codexspike/tui/notify.jsonl').open('a') as f:
 f.write(json.dumps({'argc':len(sys.argv),'payload':json.loads(sys.argv[-1])})+'\n')
