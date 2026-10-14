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
    log.write(json.dumps({'operation': operation, 'acknowledged': request.get('acknowledged', [])}) + '\n')
if operation == 'terminal-detach':
    (root / 'mock-detached').touch()
    print(json.dumps({'ok': None}))
elif operation == 'terminal-updates':
    expected = {key: entries[0][key] for key in ('writer', 'sequence', 'checksum')}
    if expected in request['acknowledged']:
        (root / 'mock-acknowledged').touch()
    ready = (root / 'remote-ready').exists() and not (root / 'mock-acknowledged').exists()
    # Exact duplicates exercise the real queue's echo suppression, not a presenter stub.
    print(json.dumps({'ok': [entries[0], entries[0]] if ready else []}))
else:
    raise AssertionError('unexpected host operation')
