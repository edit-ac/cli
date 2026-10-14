#!/usr/bin/env python3
"""Named deterministic packaged-binary probes, no credentials or service transport."""
import argparse, hashlib, importlib.util, json, pathlib, shutil, subprocess, tarfile, tempfile
p=argparse.ArgumentParser();p.add_argument('--archive',required=True);p.add_argument('--fixture',required=True);p.add_argument('--receipt',required=True);a=p.parse_args()
root=pathlib.Path(tempfile.mkdtemp(prefix='editac-packaged-probes-'));cases=[];pty_results=[]
evidence=pathlib.Path(a.receipt).resolve().with_suffix('').with_name(pathlib.Path(a.receipt).stem+'-artifacts')
evidence.mkdir(parents=True,exist_ok=False);evidence.chmod(0o700)
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
 def run(args):return subprocess.run([str(binary),*map(str,args)],cwd=root,env={},capture_output=True,timeout=20)
 def ok(o):
  assert o.returncode==0,(o.returncode,o.stderr.decode(errors='replace'));return json.loads(o.stdout)
 def manifest():
  m=json.loads((package/'manifest.json').read_text())
  for path,h in m['files'].items():assert hashlib.sha256((package/path).read_bytes()).hexdigest()==h,path
  assert (package/'licenses/yrs-0.28.0/LICENSE').exists()
  assert 'tui' in m['capabilities']
  assert m['terminalGuide']=='docs/tui.md'
  guide=(package/m['terminalGuide']).read_text()
  for required in ['Ctrl-S','Ctrl-Q','--catalog','--offline','Unqualified','Accessibility','macOS 26.4.1']:
   assert required in guide,required
 case('archive-manifest-and-apache-third-party-notices',manifest)
 def help():
  o=run(['--help']);assert o.returncode==0;assert b'edit <path>' in o.stdout;assert b'edit <hosted-url>' in o.stdout;assert b'--offline' in o.stdout
 case('truthful-local-help',help)
 case('version-probe',lambda: (_ for _ in ()).throw(AssertionError()) if run(['--version']).returncode else None)
 def tui_help():
  o=run(['tui','--help']);assert o.returncode==0;assert not o.stderr
  for text in [b'--store <absolute-db>',b'--offline | --adapter',b'Ctrl-S',b'Ctrl-Q',b'docs/tui.md',b'unqualified']:
   assert text in o.stdout,text
 case('terminal-help-without-source-or-credentials',tui_help)
 def tui_session():
  work=root/'terminal-report';work.mkdir(mode=0o700);target=work/'source.data';target.write_bytes(b'# **literal**\r\n')
  args=['tui',target,'--format','text','--store',work/'cache.sqlite']
  first=ok(run(args));second=ok(run(args))
  assert first['phase']=='session-acquired';assert 'requires a TTY' in first['presentation']
  assert first['identity']==second['identity'];assert first['sourceBytes']==len(target.read_bytes())
  assert target.read_bytes()==b'# **literal**\r\n'
 case('terminal-noninteractive-literal-and-offline-reopen',tui_session)
 # Reuse the qualified PTY controller; only the extracted binary is executed in
 # the isolated consumer. Python/controller source is test glue, not runtime input.
 spec=importlib.util.spec_from_file_location('editac_tui_pty',pathlib.Path(__file__).with_name('tui-pty.py'))
 driver=importlib.util.module_from_spec(spec);spec.loader.exec_module(driver)
 def tui_workflow(name,ext,base,draft,create=False):
  work=root/name;work.mkdir(mode=0o700);target=work/('source.'+ext);store=work/'cache.sqlite'
  if not create:target.write_bytes(base)
  args=['tui',str(target),'--store',str(store)]
  if create:args+=['--create']
  actions=[{'expect':'write capability' if create else base.decode().splitlines()[0]}]
  actions += [{'keys':'\x01'},{'paste':draft.decode()},{'keys':'\x13'},
              {'expect':'Save: committed locally'},{'keys':'\x11'}]
  output=evidence/name
  try:
   driver.run(binary,work,output,{'size':[240,24],'args':args,'actions':actions})
  finally:
   result=json.loads((output/'result.json').read_text());pty_results.append({'name':name,**result})
  assert target.read_bytes()==draft
  report=ok(run(['tui',target,'--store',store]))
  assert report['phase']=='session-acquired';assert report['sourceBytes']==len(draft)
  # Verify restored source through the actual terminal, not only a byte count.
  output=evidence/(name+'-reopen')
  try:
   driver.run(binary,work,output,{'size':[240,24],'args':['tui',str(target),'--store',str(store)],
    'actions':[{'expect':draft.decode().splitlines()[0]},{'keys':'\x11'}]})
  finally:
   result=json.loads((output/'result.json').read_text());pty_results.append({'name':name+'-reopen',**result})
 case('terminal-markdown-save-and-offline-reopen',lambda:tui_workflow('tui-markdown','md',b'# Old\n','# New 🦀\n'.encode()))
 case('terminal-literal-paste-save-and-offline-reopen',lambda:tui_workflow('tui-text','txt',b'old\r\n','**literal** 🦀\r\n'.encode()))
 case('terminal-explicit-create-save-and-reopen',lambda:tui_workflow('tui-create','txt',b'',b'created\n',True))
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
 shutil.rmtree(root)
 receipt={'archiveSha256':hashlib.sha256(pathlib.Path(a.archive).read_bytes()).hexdigest(),'executed':len(cases),'passed':sum(c['state']=='passed' for c in cases),'failed':sum(c['state']=='failed' for c in cases),'skipped':0,'cases':cases,'ptyChildren':pty_results,'cleanup':{'consumerRemoved':not root.exists(),'childrenReaped':all(r['reaped'] for r in pty_results)},'limits':'native packaged binary and OS PTY; deterministic fake editor; no terminal application/accessibility/other-OS/publication claim'}
 pathlib.Path(a.receipt).write_text(json.dumps(receipt,indent=2)+'\n')
print(json.dumps({k:receipt[k] for k in ['executed','passed','failed','skipped']}))
raise SystemExit(bool(receipt['failed']))
