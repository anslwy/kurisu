#!/usr/bin/env python3
"""Two real stale-frontend deploys, unchanged-token auth and native SSE reconnect.
Usage: KURISU_TEST_TMPDIR=<disk temp directory> python3 test-web-recycle.py NEW_KU OLD_UI_KU
The old fixture must embed a different asset set. No live ports or credentials.
"""
import hashlib,json,os,re,shlex,shutil,signal,socket,subprocess,sys,tempfile,time,uuid
from pathlib import Path
new=Path(sys.argv[1]).resolve();old=Path(sys.argv[2]).resolve()
base=Path(os.environ['KURISU_TEST_TMPDIR']).resolve()
assert str(base)!='/tmp','use the disk-backed test directory'
fixture=Path(__file__).resolve().parent/'fixtures/opencode-acp.py'
expected=json.loads(subprocess.check_output([new,'--web-build'],text=True))
reports=[]
def until(predicate,description,timeout=30):
 deadline=time.monotonic()+timeout
 while time.monotonic()<deadline:
  try:
   value=predicate()
   if value:return value
  except (OSError,ValueError,KeyError):pass
  time.sleep(.01)
 raise AssertionError(description)
def assets(body):return sorted(set(re.findall(r'assets/[^\s\'"<>]+\.(?:js|css)',body)))
import urllib.request
for run in range(1,3):
 root=Path(tempfile.mkdtemp(prefix=f'web-recycle-{run}-',dir=base));print('artifacts:',root,flush=True)
 bins=root/'bin';bins.mkdir();(bins/'opencode').symlink_to(fixture)
 target=root/'installed-ku';shutil.copy2(old,target)
 token=uuid.uuid4().hex
 env=dict(os.environ,KURISU_HOME=str(root),KURISU_WEB_TOKEN=token,TMPDIR=str(base),KURISU_TEST_TMPDIR=str(base),KURISU_OPENCODE_BIN=str(fixture),PATH=str(bins)+':'+os.environ['PATH'])
 env.pop('CARGO_TARGET_DIR',None)
 daemon=subprocess.Popen([str(target),'serve'],env=env,stdout=(root/'daemon.log').open('w'),stderr=subprocess.STDOUT)
 front=None;observer=None;duplicate=None;newpid=None
 def req(op,**fields):
  with socket.socket(socket.AF_UNIX) as stream:
   stream.settimeout(100);stream.connect(str(root/'kurisu.sock'))
   stream.sendall((json.dumps(dict(op=op,**fields))+'\n').encode())
   reply=json.loads(stream.makefile().readline())
   if 'error' in reply:raise RuntimeError(reply['error'])
   return reply['ok']
 def get(path):
  request=urllib.request.Request(origin+path,headers={'Authorization':'Bearer '+token})
  with urllib.request.urlopen(request,timeout=5) as response:return response.read().decode()
 def deploy(candidate=new):
  return subprocess.run([new,'source','install','--from',candidate,'--target',target,'--yes'],env=env,stdout=subprocess.PIPE,stderr=subprocess.STDOUT,text=True,timeout=120)
 def metrics():
  try:return json.loads((root/'sse.json').read_text())
  except (OSError,ValueError):return {}
 try:
  until(lambda:(root/'kurisu.sock').exists(),'daemon socket')
  sid=req('create',name='retained',harness='opencode',model='fixture/free',cwd=str(root/'work'))['id']
  assert req('send',name=sid,text='history before web recycle')['reply']=='PONG'
  front=subprocess.Popen([str(target),'web','--serve','--port','0'],env=env,stdout=(root/'old-web.log').open('w'),stderr=subprocess.STDOUT)
  until(lambda:(root/'web.json').exists(),'web discovery')
  configuration=json.loads((root/'web.json').read_text());port=int(configuration['listen'].rsplit(':',1)[1])
  assert port not in (8791,8798,8793)
  origin=f'http://127.0.0.1:{port}'
  before=assets(until(lambda:get('/web'),'old UI 200'))
  assert before!=expected['assets'],'old fixture already has candidate assets; supply an older UI binary'
  stale=subprocess.run([new,'doctor','--json'],env=env,text=True,stdout=subprocess.PIPE)
  checks=json.loads(stale.stdout);line=next(x for x in checks if x['check']=='/WEB BUILD')
  assert line['status'] in ('WARN','FAIL'),line
  # Multiple frontends must be rejected before the daemon/binary changes.
  if run==1:
   duplicate=subprocess.Popen([str(target),'web','--serve','--port','0'],env=env,stdout=(root/'duplicate.log').open('w'),stderr=subprocess.STDOUT)
   until(lambda:json.loads((root/'web.json').read_text())['listen']!=configuration['listen'],'second frontend')
   digest=hashlib.sha256(target.read_bytes()).hexdigest();pid=req('ping')['pid']
   rejected=deploy();(root/'multiple-refused.log').write_text(rejected.stdout)
   assert rejected.returncode!=0 and 'multiple standalone /web processes' in rejected.stdout,rejected.stdout
   assert str(front.pid) in rejected.stdout and str(duplicate.pid) in rejected.stdout
   assert req('ping')['pid']==pid and hashlib.sha256(target.read_bytes()).hexdigest()==digest
   duplicate.terminate();duplicate.wait(timeout=5);duplicate=None
   (root/'web.json').write_text(json.dumps(configuration))
  script=root/'observe-sse.cjs'
  script.write_text("""const fs=require('node:fs');
const result={opens:0,hellos:0,errors:0};
const save=()=>fs.writeFileSync(process.env.SSE_METRICS,JSON.stringify(result));
const source=new EventSource(process.env.SSE_URL);
source.onopen=()=>{result.opens++;result.last_open_ms=Date.now();if(result.opens===2)result.reconnect_ms=Date.now()-result.disconnected_ms;save();};
source.onerror=()=>{result.errors++;result.disconnected_ms??=Date.now();save();};
source.addEventListener('hello',()=>{result.hellos++;save();if(result.opens>=2&&result.hellos>=2){source.close();clearTimeout(timer);process.exit(0);}});
const timer=setTimeout(()=>{save();source.close();process.exit(1);},25000);
""")
  observer=subprocess.Popen(['node','--experimental-eventsource',script],env=dict(env,SSE_URL=origin+'/api/v1/stream?token='+token,SSE_METRICS=str(root/'sse.json')),stdout=(root/'sse.log').open('w'),stderr=subprocess.STDOUT)
  until(lambda:metrics().get('hellos',0)>=1,'native EventSource initial hello')
  result=deploy();(root/'deploy.log').write_text(result.stdout)
  assert result.returncode==0,result.stdout
  front.wait(timeout=5)
  observer.wait(timeout=20);assert observer.returncode==0,metrics()
  record=json.loads((root/'web-recycle.json').read_text());newpid=record['new_pid']
  after=assets(get('/web'))
  assert record['old_pid']==front.pid and record['new_pid']!=front.pid
  assert record['asset_hash_swap'] and after==expected['assets'] and after!=before
  assert record['token_preserved'] and isinstance(json.loads(get('/api/v1/sessions'))['ok'],list)
  assert json.loads(get('/_ku/web-build'))['pid']==newpid
  promoted=int(re.search(r'post-swap probe: ok \(pid (\d+)',result.stdout)[1])
  assert req('ping')['pid']==promoted,'UI recycle must not restart promoted daemon'
  assert req('send',name=sid,text='history after web recycle')['reply']=='PONG'
  history=req('history',name=sid,tail=100)
  assert any('history before web recycle' in row.get('text','') for row in history)
  doctor=subprocess.run([new,'doctor'],env=env,text=True,stdout=subprocess.PIPE)
  doctor_line=next(x for x in doctor.stdout.splitlines() if x.startswith('/WEB BUILD'))
  assert re.match(r'/WEB BUILD\s+OK',doctor_line),doctor_line
  report={'run':run,'home':str(root),'port':port,'old_pid':front.pid,'new_pid':newpid,'ui_gap_ms':record['ui_gap_ms'],'asset_hash_swap':True,'token_preserved':True,'sse_auto_reconnected':metrics()['opens']>=2,'sse_reconnect_ms':metrics()['reconnect_ms'],'doctor':doctor_line,'before':before,'after':after}
  reports.append(report);(root/'acceptance.json').write_text(json.dumps(report,indent=2))
  print('WEB RECYCLE '+json.dumps(report),flush=True)
 finally:
  if observer is not None and observer.poll() is None:observer.kill();observer.wait()
  if duplicate is not None and duplicate.poll() is None:duplicate.terminate();duplicate.wait()
  if front is not None and front.poll() is None:front.terminate();front.wait()
  if newpid is not None:
   try:os.kill(newpid,signal.SIGTERM)
   except ProcessLookupError:pass
  try:req('shutdown')
  except (OSError,RuntimeError,ValueError):pass
  try:daemon.wait(timeout=5)
  except subprocess.TimeoutExpired:daemon.kill();daemon.wait()
# Failure decisions must leave the installed path unchanged and daemon alive.
root=Path(tempfile.mkdtemp(prefix='web-recycle-refusal-',dir=base));target=root/'installed-ku';shutil.copy2(new,target)
token=uuid.uuid4().hex
env=dict(os.environ,KURISU_HOME=str(root),KURISU_WEB_TOKEN=token,TMPDIR=str(base),KURISU_TEST_TMPDIR=str(base))
daemon=subprocess.Popen([str(target),'serve'],env=env,stdout=(root/'daemon.log').open('w'),stderr=subprocess.STDOUT)
front=None
try:
 until(lambda:(root/'kurisu.sock').exists(),'refusal fixture daemon')
 absent=subprocess.run([new,'source','install','--from',new,'--target',target,'--yes'],env=env,stdout=subprocess.PIPE,stderr=subprocess.STDOUT,text=True,timeout=120)
 (root/'absent.log').write_text(absent.stdout)
 assert absent.returncode==0 and 'no frontend spawned' in absent.stdout,absent.stdout
 front=subprocess.Popen([str(old),'web','--serve','--port','0'],env=env,stdout=(root/'old-web.log').open('w'),stderr=subprocess.STDOUT)
 until(lambda:(root/'web.json').exists(),'refusal fixture frontend')
 origin='http://127.0.0.1:'+str(json.loads((root/'web.json').read_text())['listen'].rsplit(':',1)[1])
 until(lambda:get('/web'),'refusal fixture UI')
 wrapper=root/'stale-candidate'
 wrapper.write_text('#!/bin/sh\nif [ "$1" = "web" ]; then exec '+shlex.quote(str(old))+' "$@"; fi\nexec '+shlex.quote(str(new))+' "$@"\n');wrapper.chmod(0o755)
 digest=hashlib.sha256(target.read_bytes()).hexdigest()
 failed=subprocess.run([new,'source','install','--from',wrapper,'--target',target,'--yes'],env=env,stdout=subprocess.PIPE,stderr=subprocess.STDOUT,text=True,timeout=120)
 (root/'unchanged-assets-refused.log').write_text(failed.stdout)
 assert failed.returncode!=0 and 'asset verification failed' in failed.stdout,failed.stdout
 assert hashlib.sha256(target.read_bytes()).hexdigest()==digest,'failed UI proof must not publish candidate binary'
 until(lambda:get('/api/v1/sessions'),'previous UI restored with preserved token')
 stale=subprocess.run([new,'doctor','--json'],env=env,text=True,stdout=subprocess.PIPE)
 assert next(x for x in json.loads(stale.stdout) if x['check']=='/WEB BUILD')['status']=='FAIL'
 print('PASS absent frontend skipped; unchanged stale assets refused; installed binary unchanged; previous UI/token restored; doctor detects stale UI beside current daemon',flush=True)
finally:
 # The restored foreground process was spawned by the installer, not this test.
 for entry in Path('/proc').iterdir():
  if not entry.name.isdigit():continue
  try:
   args=(entry/'cmdline').read_bytes().split(b'\0')
   environment=(entry/'environ').read_bytes().split(b'\0')
   if len(args)>1 and args[1]==b'web' and ('KURISU_HOME='+str(root)).encode() in environment:os.kill(int(entry.name),signal.SIGTERM)
  except (OSError,ValueError):pass
 try:req('shutdown')
 except (OSError,RuntimeError,ValueError):pass
 if front is not None:
  try:front.wait(timeout=5)
  except subprocess.TimeoutExpired:front.kill();front.wait()
 try:daemon.wait(timeout=5)
 except subprocess.TimeoutExpired:daemon.kill();daemon.wait()
(base/'web-recycle-acceptance.json').write_text(json.dumps(reports,indent=2))
print('PASS 2 asset swaps, preserved token/port/home, stale doctor detection, native EventSource auto reconnect',flush=True)
