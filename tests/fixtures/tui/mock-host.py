#!/usr/bin/python3
"""Prepared explicit host IPC only. No credentials, network or providers."""
import json
import pathlib
import sys

root = pathlib.Path('__FIXTURE_ROOT__')
request = json.load(sys.stdin)
entries = json.loads((root / 'remote-entries.json').read_text())
assert request['identity'] == json.loads((root / 'remote-scope.json').read_text())
operation = request['operation']
with (root / 'mock-calls.jsonl').open('a') as log:
    log.write(json.dumps({'operation': operation, 'acknowledged': request.get('acknowledged', []), 'queued': request.get('queued', [])}) + '\n')
if operation == 'terminal-detach':
    (root / 'mock-detached').touch()
    print(json.dumps({'ok': None}))
elif operation == 'terminal-updates':
    ack_path = root / 'mock-acks.json'
    acknowledged = json.loads(ack_path.read_text()) if ack_path.exists() else []
    for item in request['acknowledged']:
        if item not in acknowledged:
            acknowledged.append(item)
    ack_path.write_text(json.dumps(acknowledged))
    ids = [{key: entry[key] for key in ('writer', 'sequence', 'checksum')} for entry in entries]
    if all(item in acknowledged for item in ids):
        (root / 'mock-acknowledged').touch()
    ready = (root / 'remote-ready').exists() and not (root / 'mock-acknowledged').exists()
    available = [entry for entry, item in zip(entries, ids) if item not in acknowledged and item not in request['queued']]
    if (root / 'mock-reordered').exists():
        batch = available[-1:] if ready else []
    else:
        batch = [entries[0], entries[0]] if ready and available else []
    print(json.dumps({'ok': batch[:request['capacity']]}))
else:
    raise AssertionError('unexpected host operation')
