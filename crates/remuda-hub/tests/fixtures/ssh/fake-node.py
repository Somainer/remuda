#!/usr/bin/env python3
# Original deterministic Hub SSH fixture: no remote access or model invocation.
import json
import os
import sys
from pathlib import Path
if 'version' in sys.argv:
    print('remuda 0.1.0')
    sys.exit(0)
data = Path(os.environ['REMUDA_DATA_DIR'])
host = json.loads((data / 'enrollment.json').read_text())['hostId']
(data / 'fixture-pid').write_text(str(os.getpid()))
def emit(value):
    print(json.dumps(value), flush=True)
emit({'jsonrpc': '2.0', 'id': 'hello-1', 'method': 'node.hello', 'params': {
    'hostId': host, 'label': 'fixture', 'nodeVersion': '0.1.0', 'transport': 'ssh-stdio',
    'host': {'hostname': 'fixture-node', 'labels': ['egress=gateway'], 'cli': [{'kind': 'codex', 'version': 'fixture', 'path': '/fixture/codex'}]}
}})
for line in sys.stdin:
    frame = json.loads(line)
    if 'method' not in frame:
        continue
    method = frame['method']
    if method == 'worktree.list':
        result = {'items': [], 'workspaceRoot': str(data / 'workspace'), 'fixtureHostId': host}
    else:
        result = {'accepted': True, 'fixtureHostId': host}
    emit({'jsonrpc': '2.0', 'id': frame['id'], 'result': result})
