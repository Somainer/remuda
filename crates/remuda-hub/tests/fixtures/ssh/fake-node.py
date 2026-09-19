#!/usr/bin/env python3
# Original deterministic Hub SSH fixture: no remote access or model invocation.
import json
import os
import subprocess
import sys
import time
from pathlib import Path
if 'version' in sys.argv:
    print('remuda 0.1.0')
    sys.exit(0)
data = Path(os.environ['REMUDA_DATA_DIR'])
if 'status' in sys.argv:
    sys.exit(0 if (data / 'fixture-daemon-alive').exists() else 1)
if 'install' in sys.argv or 'run' in sys.argv:
    (data / 'fixture-daemon-alive').touch()
    sys.exit(0)
host = json.loads((data / 'enrollment.json').read_text())['hostId']
instance_file = data / 'fixture-instance.json'
journal_file = data / 'fixture-journal.jsonl'
if 'complete' in sys.argv:
    time.sleep(1.2)
    instance = json.loads(instance_file.read_text())
    instance.update(lifecycle='exited', activity='idle', durableSeq='2')
    instance_file.write_text(json.dumps(instance))
    with journal_file.open('a') as journal:
        journal.write(json.dumps({'instanceId': instance['id'], 'seq': '2', 'event': {
            'kind': 'lifecycle', 'payload': {'type': 'entity', 'state': 'exited', 'reasonCode': 'fixture-completed'}
        }}) + '\n')
    (data / 'fixture-DONE').write_text('DONE\n')
    sys.exit(0)
if (data / 'fixture-pause-bridge').exists():
    sys.exit(1)
(data / 'fixture-pid').write_text(str(os.getpid()))
def emit(value):
    print(json.dumps(value), flush=True)
emit({'jsonrpc': '2.0', 'id': 'hello-1', 'method': 'node.hello', 'params': {
    'hostId': host, 'label': 'fixture', 'nodeVersion': '0.1.0', 'transport': 'ssh-stdio', 'bridge': True, 'daemon': True,
    'instances': [json.loads(instance_file.read_text())] if instance_file.exists() else [],
    'host': {'hostname': 'fixture-node', 'labels': ['egress=gateway'], 'cli': [{'kind': 'codex', 'version': 'fixture', 'path': '/fixture/codex'}]},
    'capabilities': {'apiRelay': True, 'features': ['api-relay-v1']}
}})
for line in sys.stdin:
    frame = json.loads(line)
    if 'method' not in frame:
        if frame.get('id') == 'hello-1':
            watermarks = frame.get('result', {}).get('instanceWatermarks', [])
            (data / 'fixture-watermarks.json').write_text(json.dumps(watermarks))
            acked = {item['instanceId']: int(item['durableSeq']) for item in watermarks}
            if journal_file.exists():
                for saved in journal_file.read_text().splitlines():
                    entry = json.loads(saved)
                    if int(entry['seq']) > acked.get(entry['instanceId'], 0):
                        emit({'jsonrpc': '2.0', 'id': 'replay-' + entry['seq'], 'method': 'journal.append', 'params': entry})
        continue
    method = frame['method']
    if method == 'worktree.list':
        result = {'items': [], 'workspaceRoot': str(data / 'workspace'), 'fixtureHostId': host}
    elif method == 'instance.create':
        # Build the full result before emitting: echo the requested route so
        # the Hub persists the observed route on the instance.
        result = {'accepted': True, 'fixtureHostId': host}
        spec = frame['params'].get('spec', {})
        if 'apiRoute' in spec:
            result['apiRoute'] = spec['apiRoute']
    else:
        result = {'accepted': True, 'fixtureHostId': host}
    emit({'jsonrpc': '2.0', 'id': frame['id'], 'result': result})
    if method == 'instance.create':
        instance_id = frame['params']['instanceId']
        instance_file.write_text(json.dumps({'id': instance_id, 'hostId': host, 'lifecycle': 'ready', 'activity': 'working', 'durableSeq': '1'}))
        entry = {'instanceId': instance_id, 'seq': '1', 'event': {'kind': 'lifecycle', 'payload': {'type': 'entity', 'state': 'ready'}}}
        journal_file.write_text(json.dumps(entry) + '\n')
        emit({'jsonrpc': '2.0', 'id': 'start-1', 'method': 'journal.append', 'params': entry})
        if os.environ.get('REMUDA_FAKE_API_OPEN') == '1':
            # Test-only: act as a worker Node that opens one relay stream for
            # the fresh instance. The pause lets the Hub persist the create
            # echo (observed route) before the open is dispatched.
            time.sleep(0.5)
            emit({'jsonrpc': '2.0', 'method': 'api.open', 'params': {
                'instanceId': instance_id, 'streamId': 'st_ssh_cancel',
                'method': 'POST', 'path': '/v1/messages', 'query': '',
                'headers': [], 'bodyChunked': False, 'deadlineMs': 60000,
            }})
        subprocess.Popen([sys.executable, __file__, 'complete'], stdin=subprocess.DEVNULL, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL, start_new_session=True)
