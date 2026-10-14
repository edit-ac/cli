#!/usr/bin/env python3
"""Named deterministic packaged-binary probes, no credentials or service transport."""
import argparse, hashlib, json, pathlib, shutil, subprocess, tarfile, tempfile
p=argparse.ArgumentParser();p.add_argument('--archive',required=True);p.add_argument('--fixture',required=True);p.add_argument('--receipt',required=True);a=p.parse_args()
root=pathlib.Path(tempfile.mkdtemp(prefix='editac-packaged-probes-'));cases=[]
def case(name,fn):
 try: fn();cases.append({'name':name,'state':'passed'})
 except Exception as e: cases.append({'name':name,'state':'failed','error':str(e)})
try:
 package=root/'package'
 with tarfile.open(a.archive) as tar:
  members=tar.getmembers()
  for member in members:
   path=pathlib.PurePosixPath(member.name)
   if path.is_absolute() or '..' in path.parts or not (member.isfile() or member.isdir()):
    raise ValueError('Unsafe package archive member: '+member.name)
  tar.extractall(package,members=members)
 binary=package/'bin/editac';fixture=pathlib.Path(a.fixture).resolve()
 def run(args):return subprocess.run([str(binary),*map(str,args)],env={},capture_output=True)
 def ok(o):
  assert o.returncode==0,(o.returncode,o.stderr.decode(errors='replace'));return json.loads(o.stdout)
 def manifest():
  m=json.loads((package/'manifest.json').read_text())
  for path,h in m['files'].items():assert hashlib.sha256((package/path).read_bytes()).hexdigest()==h,path
  assert (package/'licenses/yrs-0.28.0/LICENSE').exists()
 case('archive-manifest-and-apache-third-party-notices',manifest)
 def help():
  o=run(['--help']);assert o.returncode==0;assert b'edit <path>' in o.stdout;assert b'Hosted document commands are unavailable' in o.stdout
 case('truthful-local-help',help)
 case('version-probe',lambda: (_ for _ in ()).throw(AssertionError()) if run(['--version']).returncode else None)
 def workflow(name,ext,base,draft,mode='changed',state='locally-saved',create=False):
  work=root/name;work.mkdir();target=work/('source.'+ext);payload=work/'payload';recovery=work/'recovery';payload.write_bytes(draft)
  if not create:target.write_bytes(base)
  command=f"/bin/sh '{fixture}' '{mode}' '{payload}' '{target}'"
  args=['edit',target,'--editor',command,'--recovery-dir',recovery]
  if create:args+=['--create']
  o=run(args)
  if state=='conflict':assert o.returncode==2;assert target.read_bytes()==b'external writer\n'
  elif state=='interrupted':assert o.returncode<0;assert target.read_bytes()==base
  else:assert ok(o)['state']==state;assert target.read_bytes()==(draft if state=='locally-saved' else base)
  if state!='unchanged':
   session=next(recovery.iterdir());output=work/'export';r=ok(run(['recover',session,'--output',output]));assert output.read_bytes()==draft;assert r['remoteDurability']=='not-applicable'
   assert run(['recover',session,'--output',target]).returncode==2
 case('markdown-unicode-source-and-recovery',lambda:workflow('markdown','md',b'# Old\r\n', '# New 🦀\r\n'.encode()))
 case('literal-text-avoids-markdown',lambda:workflow('text','txt',b'old',b'# *literal*\n'))
 case('noop-keeps-source-bytes',lambda:workflow('noop','md',b'# Odd  \r\n',b'ignored','noop','unchanged'))
 case('cancel-retains-draft-only',lambda:workflow('cancel','txt',b'old',b'cancelled','cancel','cancelled'))
 case('stale-save-and-committed-recovery',lambda:workflow('stale','txt',b'old',b'local draft','stale','conflict'))
 case('process-interruption-and-draft-recovery',lambda:workflow('interrupted','txt',b'old',b'interrupted','interrupt','interrupted'))
 case('explicit-offline-file-creation',lambda:workflow('create','txt',b'',b'created',create=True))
finally:
 receipt={'archiveSha256':hashlib.sha256(pathlib.Path(a.archive).read_bytes()).hexdigest(),'executed':len(cases),'passed':sum(c['state']=='passed' for c in cases),'failed':sum(c['state']=='failed' for c in cases),'skipped':0,'cases':cases,'limits':'native packaged binary, deterministic fake editor; no hosted/device/publication claim'}
 pathlib.Path(a.receipt).write_text(json.dumps(receipt,indent=2)+'\n');shutil.rmtree(root)
print(json.dumps({k:receipt[k] for k in ['executed','passed','failed','skipped']}))
raise SystemExit(bool(receipt['failed']))
