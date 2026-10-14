#!/usr/bin/env python3
"""Package a built native artifact with locked provenance; never publish or use a service."""
import argparse
import gzip
import hashlib
import io
import json
import pathlib
import platform
import subprocess
import tarfile

parser = argparse.ArgumentParser()
parser.add_argument('--binary', required=True)
parser.add_argument('--output', required=True)
parser.add_argument('--revision', required=True)
parser.add_argument('--profile', choices=['local', 'hosted'], default='local')
args = parser.parse_args()
root = pathlib.Path(__file__).resolve().parents[1]
output = pathlib.Path(args.output)
output.mkdir(parents=True, exist_ok=True)
rust = subprocess.check_output(['rustc', '+1.97.1', '-vV'], text=True)
target = next(line.split(': ', 1)[1] for line in rust.splitlines() if line.startswith('host: '))
metadata = json.loads(subprocess.check_output([
    'cargo', '+1.97.1', 'metadata', '--locked', '--offline', '--format-version', '1',
    '--filter-platform', target,
], cwd=root))
nodes = {node['id']: node for node in metadata['resolve']['nodes']}
remaining = [metadata['resolve']['root']]
reachable = set()
while remaining:
    package_id = remaining.pop()
    if package_id not in reachable:
        reachable.add(package_id)
        remaining.extend(dep['pkg'] for dep in nodes[package_id]['deps'])
packages = []
for package in metadata['packages']:
    if package['id'] not in reachable or package['name'] == 'editac-cli':
        continue
    license_dir = root / 'licenses' / (package['name'] + '-' + package['version'])
    if not license_dir.is_dir() or not any(license_dir.iterdir()):
        raise SystemExit('Missing license text for ' + package['name'])
    packages.append({key: package[key] for key in ('name', 'version', 'license')})
files = {
    'bin/editac': pathlib.Path(args.binary).read_bytes(),
    'README.md': (root / 'README.md').read_bytes(),
    'LICENSE': (root / 'LICENSE').read_bytes(),
    'THIRD_PARTY_NOTICES.md': (root / 'THIRD_PARTY_NOTICES.md').read_bytes(),
    'provenance/core.json': (root / 'vendor/core/provenance.json').read_bytes(),
    'provenance/Cargo.lock': (root / 'Cargo.lock').read_bytes(),
}
for path in sorted((root / 'licenses').rglob('*')):
    if path.is_file():
        files[str(path.relative_to(root))] = path.read_bytes()
manifest = {
    'kind': args.profile + '-workflow development artifact; no publication',
    'capabilities': ['local-edit', 'hosted-edit', 'offline-cache', 'sync', 'status', 'recover', 'diagnostics'],
    'sourceRevision': args.revision,
    'buildHost': target,
    'rustc': rust,
    'platform': platform.platform(),
    'architecture': platform.machine(),
    'packages': sorted(packages, key=lambda item: (item['name'], item['version'])),
    'files': {name: hashlib.sha256(data).hexdigest() for name, data in files.items()},
}
files['manifest.json'] = (json.dumps(manifest, indent=2) + '\n').encode()
archive = output / ('editac-' + args.profile + '.tar.gz')
with archive.open('wb') as stream:
    with gzip.GzipFile(fileobj=stream, mode='wb', mtime=0, filename='') as zipped:
        with tarfile.open(fileobj=zipped, mode='w') as tar:
            for name, data in sorted(files.items()):
                entry = tarfile.TarInfo(name)
                entry.size = len(data)
                entry.mode = 0o755 if name == 'bin/editac' else 0o644
                entry.mtime = 0
                tar.addfile(entry, io.BytesIO(data))
(output / 'package.json').write_text(json.dumps({
    'archive': archive.name,
    'sha256': hashlib.sha256(archive.read_bytes()).hexdigest(),
    'manifest': manifest,
}, indent=2) + '\n')
print(archive)
