#!/usr/bin/env python3
"""Exercise deploys with real servers/ACP turns and a loopback Telegram API."""
import concurrent.futures, http.server, json, os, queue, shutil, socket, subprocess, sys, tempfile, threading, time
from pathlib import Path
binary=Path(sys.argv[1]).resolve()
fixture=Path(sys.argv[2]).resolve() if len(sys.argv)>2 else Path(__file__).resolve().parent/'fixtures/opencode-acp.py'
root=Path(tempfile.mkdtemp(prefix='ku-deploy-',dir='/tmp'))
print('artifacts:',root,flush=True)
updates=queue.Queue();messages=[];polls=[];samples=[];halt=threading.Event()
class Telegram(http.server.BaseHTTPRequestHandler):
 def log_message(self,*args):pass
 def do_POST(self):
  data=json.loads(self.rfile.read(int(self.headers.get('Content-Length',0))) or '{}');method=self.path.rsplit('/',1)[-1];result=True
  if method=='getUpdates':
   polls.append(time.monotonic());result=[]
   if data.get('offset',-1)>=0:
    try:result=[updates.get(timeout=.05)]
    except queue.Empty:pass
  elif method=='getMe':result={'id':7,'username':'local_test_bot','is_bot':True}
  elif method in ('sendMessage','editMessageText'):
   messages.append(data);result={'message_id':len(messages),'chat':{'id':42},'text':data.get('text','')}
  body=json.dumps({'ok':True,'result':result}).encode();self.send_response(200);self.end_headers()
  try:self.wfile.write(body)
  except BrokenPipeError:pass
api=http.server.ThreadingHTTPServer(('127.0.0.1',0),Telegram)
threading.Thread(target=api.serve_forever,daemon=True).start()
(root/'telegram.json').write_text(json.dumps({'token':'local-fake','chat_id':'42'}))
bins=root/'bin';bins.mkdir();(bins/'opencode').symlink_to(fixture);target=root/'installed-ku';shutil.copy2(binary,target)
env=dict(os.environ,KURISU_HOME=str(root),KURISU_OPENCODE_BIN=str(fixture),KURISU_OPENCODE_TIMEOUT_SECS='120',KURISU_TELEGRAM_API=f'http://127.0.0.1:{api.server_port}',PATH=str(bins)+':'+os.environ['PATH'])
def req(op,path=None,**fields):
 with socket.socket(socket.AF_UNIX) as s:
  s.settimeout(100);s.connect(str(path or root/'kurisu.sock'));s.sendall((json.dumps(dict(op=op,**fields))+'\n').encode());r=json.loads(s.makefile().readline())
  if 'error' in r:raise RuntimeError(r['error'])
  return r['ok']
def until(pred,desc,timeout=30):
 deadline=time.monotonic()+timeout
 while time.monotonic()<deadline:
  if pred():return
  time.sleep(.02)
 raise AssertionError(desc)
def deploy(candidate=binary,fault=None):
 e=env.copy()
 if fault:e['KURISU_DEPLOY_FAULT']=fault
 return subprocess.run([str(binary),'source','install','--from',str(candidate),'--target',str(target),'--yes'],env=e,text=True,stdout=subprocess.PIPE,stderr=subprocess.STDOUT,timeout=110)
def version():
 v=json.loads(subprocess.check_output([str(binary),'version','--json'],env=env,text=True))
 v.pop('uptime_seconds',None)
 return v
def sample():
 channels={}
 try:
  while not halt.wait(.05):
   states=[]
   for p in [root/'kurisu.sock',root/'kurisu.sock.a',root/'kurisu.sock.b']:
    try:
     # Keep a control connection to the old server after it stops accepting.
     # Otherwise a draining-but-still-polling process would disappear from samples.
     if p not in channels:
      sock=socket.socket(socket.AF_UNIX);sock.settimeout(.5);sock.connect(str(p))
      channels[p]=(sock,sock.makefile())
     sock,reader=channels[p]
     sock.sendall(b'{"op":"deploy_state"}\n')
     reply=json.loads(reader.readline())
     if 'error' in reply:raise RuntimeError(reply['error'])
     v=reply['ok']
     if not any(x['pid']==v['pid'] for x in states):states.append(v)
    except (OSError,RuntimeError,ValueError):
     if p in channels:
      sock,reader=channels.pop(p);reader.close();sock.close()
   samples.append({'at':time.monotonic(),'states':states})
 finally:
  for sock,reader in channels.values():reader.close();sock.close()
server=subprocess.Popen([str(target),'serve'],env=env,stdout=(root/'server.log').open('w'),stderr=subprocess.STDOUT)
try:
 until(lambda:(root/'kurisu.sock').exists(),'server socket')
 until(lambda:len(polls)>0,'Telegram polling')
 sid=req('create',name='mid-turn',harness='opencode',model='fixture/free',cwd=str(root/'work'))['id']
 assert req('send',name=sid,text='hello')['reply']=='PONG'
 before=version();pid=req('ping')['pid'];contents=target.read_bytes()
 broken=root/'broken-ku';broken.write_text('#!/bin/sh\nif [ "$1" = "--version" ]; then exec '+str(binary)+' --version; fi\necho deliberately broken startup >&2\nexit 71\n');broken.chmod(0o755)
 r=deploy(broken);(root/'broken.log').write_text(r.stdout)
 assert r.returncode!=0,r.stdout
 assert version()==before and req('ping')['pid']==pid and target.read_bytes()==contents
 print('PASS broken candidate: version, PID and installed bytes unchanged\n'+r.stdout,flush=True)
 r=deploy(fault='post_swap');(root/'rollback.log').write_text(r.stdout)
 assert r.returncode!=0 and 'rolled back' in r.stdout,r.stdout
 assert version()==before and req('ping')['pid']==pid and target.read_bytes()==contents
 assert req('send',name=sid,text='after rollback')['reply']=='PONG'
 print('PASS rollback: previous process and binary restored, next turn succeeds\n'+r.stdout,flush=True)
 thread=threading.Thread(target=sample,daemon=True);thread.start()
 with concurrent.futures.ThreadPoolExecutor() as pool:
  turn=pool.submit(req,'send',name=sid,text='FIXTURE_RELEASE_WAIT')
  until(lambda:any('FIXTURE_RELEASE_WAIT' in x for x in (root/'fixture-requests.jsonl').read_text().splitlines()),'mid-turn prompt')
  hid=req('status',name=sid)['harness_session_id']
  job=pool.submit(deploy)
  until(lambda:(root/'kurisu.sock.a').exists() and (root/'kurisu.sock.b').exists(),'candidate and old slots')
  updates.put({'update_id':1,'message':{'message_id':1,'date':int(time.time()),'chat':{'id':42,'type':'private'},'from':{'id':42},'text':'/id'}})
  time.sleep(.5);assert not turn.done(),'turn must still be running during switch'
  (root/('fixture-release-'+hid)).write_text('ok')
  assert turn.result(timeout=30)['reply']=='PONG'
  r=job.result(timeout=90);(root/'handover.log').write_text(r.stdout)
  assert r.returncode==0,r.stdout
 until(lambda:any('text' in x and 'id' in x['text'].lower() for x in messages),'queued Telegram response')
 assert req('ping')['pid']!=pid
 assert req('send',name=sid,text='after deploy')['reply']=='PONG'
 history=req('history',name=sid,tail=100)
 assert any('FIXTURE_RELEASE_WAIT' in x.get('text','') for x in history)
 print('PASS deploy during real ACP mid-turn: turn finished, history retained, next turn succeeds\n'+r.stdout,flush=True)
 print('PASS Telegram message queued during switch answered afterwards',flush=True)
 halt.set();thread.join(timeout=3)
 (root/'samples.json').write_text(json.dumps(samples,indent=2))
 primary=[sum(not s['standby'] for s in x['states']) for x in samples]
 poller=[sum(s['telegram']=='running' for s in x['states']) for x in samples]
 print('SAMPLED',len(samples),'public/slot snapshots; primary count range',min(primary),max(primary),'Telegram poller count range',min(poller),max(poller),flush=True)
 assert max(primary)<=1 and max(poller)<=1,'duplicate primary or Telegram poller'
 print('PASS no duplicate primary or poller; standby processes recorded separately',flush=True)
 if os.environ.get('KURISU_DEPLOY_REQUIRE_CONTINUOUS')=='1':
  assert min(primary)==min(poller)==1, 'acceptance (d) failed: an ownership sample was 0, expected exactly 1 continuously'
finally:
 halt.set()
 try:req('shutdown')
 except Exception:pass
 try:server.wait(timeout=5)
 except subprocess.TimeoutExpired:server.kill();server.wait()
 api.shutdown();api.server_close()
