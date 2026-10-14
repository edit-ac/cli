#!/usr/bin/env python3
"""Probe an extracted native package using the canonical prepared hosted workflow.
The Rust harness supplies real engine checkpoints/updates; all service IPC is local.
"""
import argparse
import hashlib
import json
import os
from pathlib import Path, PurePosixPath
import re
import subprocess
import tarfile
import tempfile

p = argparse.ArgumentParser()
p.add_argument('--archive', required=True)
p.add_argument('--receipt', required=True)
a = p.parse_args()
receipt_path = Path(a.receipt).resolve()
receipt_path.parent.mkdir(parents=True, exist_ok=True)
log_path = receipt_path.with_suffix('.log')
receipt = {'archiveSha256': hashlib.sha256(Path(a.archive).read_bytes()).hexdigest(),
           'executed': 0, 'passed': 0, 'failed': 0, 'skipped': 0, 'cases': [],
           'provenance': 'os-host; extracted native binary; prepared executable service/auth/receipts; no publication'}
try:
    with tempfile.TemporaryDirectory(prefix='editac-hosted-package-') as temporary:
        root = Path(temporary)
        with tarfile.open(a.archive) as tar:
            members = tar.getmembers()
            names = set()
            for member in members:
                path = PurePosixPath(member.name)
                assert not path.is_absolute() and '..' not in path.parts
                assert member.isfile() and member.name not in names
                names.add(member.name)
            tar.extractall(root, members=members)
        manifest = json.loads((root / 'manifest.json').read_text())
        for path, digest in manifest['files'].items():
            assert hashlib.sha256((root / path).read_bytes()).hexdigest() == digest, path
        assert 'hosted-edit' in manifest['capabilities']
        env = dict(os.environ, EDITAC_TEST_BINARY=str(root / 'bin/editac'))
        with log_path.open('w') as log:
            run = subprocess.run(['cargo', '+1.97.1', 'test', '--test', 'hosted_workflows',
                                  '--locked', '--offline', '--', '--test-threads=1'],
                                 cwd=Path(__file__).resolve().parents[1], env=env,
                                 stdout=log, stderr=subprocess.STDOUT)
        output = log_path.read_text()
        receipt['cases'] = [{'name': name, 'state': state} for name, state in
                            re.findall(r'^test (\S+) \.\.\. (ok|FAILED|ignored)', output, re.M)]
        receipt['executed'] = len(receipt['cases'])
        receipt['passed'] = sum(c['state'] == 'ok' for c in receipt['cases'])
        receipt['failed'] = sum(c['state'] == 'FAILED' for c in receipt['cases'])
        receipt['skipped'] = sum(c['state'] == 'ignored' for c in receipt['cases'])
        receipt['testExit'] = run.returncode
        assert run.returncode == 0 and receipt['executed'] >= 12
        assert receipt['passed'] == receipt['executed'] and not receipt['failed'] and not receipt['skipped']
except Exception as error:
    receipt['error'] = str(error)
    receipt['failed'] = max(1, receipt['failed'])
finally:
    receipt['log'] = log_path.name
    receipt_path.write_text(json.dumps(receipt, indent=2) + '\n')
print(json.dumps({k: receipt[k] for k in ['executed', 'passed', 'failed', 'skipped']}))
raise SystemExit(bool(receipt['failed'] or receipt['skipped']))
