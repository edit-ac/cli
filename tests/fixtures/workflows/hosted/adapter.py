#!/usr/bin/python3
"""Prepared local host IPC fixture; no sockets, ambient credentials or providers."""
import hashlib
import json
import os
from pathlib import Path
import sys

root = Path(__file__).resolve().parent
r = json.load(sys.stdin)
c = json.loads((root / 'fixture.json').read_text())
op = r['operation']
with (root / 'operations.jsonl').open('a') as log:
    log.write(json.dumps({'operation': op}) + '\n')

def save(name, data):
    with (root / name).open('w') as f:
        json.dump(data, f)
        f.flush()
        os.fsync(f.fileno())

def reply(ok=None, error=None):
    print(json.dumps({'error': error} if error else {'ok': ok}))
    sys.exit(0)

if op == 'credential':
    reply('prepared-hosted-secret')
if op == 'resolve':
    if c.get('authorizationFailure'):
        reply(error='authorization')
    reply(c['identity'])
if op == 'bootstrap':
    reply(c['checkpoint'])
if op == 'connect':
    count = json.loads((root / 'connects.json').read_text()) if (root / 'connects.json').exists() else 0
    save('connects.json', count + 1)
    if count < c.get('disconnects', 0):
        reply(error='unavailable')
    capability = None if c.get('noReceipts') else 'editac-durable-receipt-v1'
    reply({'identity': c.get('negotiationIdentity', r['identity']), 'receiptProtocol': capability})
if op == 'exchange':
    for frame in r['frames']:
        assert all(isinstance(b, int) and 0 <= b < 256 for b in frame)
    if (root / 'editor-finished').exists() and c.get('remoteFrames') and not (root / 'received.json').exists():
        save('received.json', True)
        reply(c['remoteFrames'])
    reply([])
if op == 'upload':
    if c.get('revoked'):
        reply(error='revoked')
    e = r['entry']
    assert hashlib.sha256(bytes(e['bytes'])).hexdigest() == e['checksum']
    # Prepared service receipt follows its explicit fixture durability boundary.
    save('uploaded-' + str(e['writerId']) + '-' + str(e['sequence']) + '.json', e)
    if c.get('missingReceipt'):
        reply({'receipt': None})
    receipt = {'protocol': 'editac-durable-receipt-v1', 'state': 'durable', **{
        k: e[k] for k in ['identity', 'writerId', 'sequence', 'checksum', 'updateId']}}
    if c.get('wrongReceipt'):
        receipt['checksum'] = '0' * 64
    reply({'receipt': receipt})
if op == 'disconnect':
    reply(None)
reply(error='invalid-protocol')
