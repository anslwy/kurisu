#!/usr/bin/env python3
"""Exercise deploys with real servers/ACP turns and a loopback Telegram API."""
import concurrent.futures, http.server, json, os, queue, shutil, socket, subprocess, sys, tempfile, threading, time, urllib.request
from pathlib import Path
binary=Path(sys.argv[1]).resolve()
fixture=Path(sys.argv[2]).resolve() if len(sys.argv)>2 else Path(__file__).resolve().parent/'fixtures/opencode-acp.py'
root=Path(tempfile.mkdtemp(prefix='ku-deploy-',dir='/tmp'))
print('artifacts:',root,flush=True)
updates=queue.Queue();messages=[];polls=[];samples=[];http_samples=[];halt=threading.Event();web=None
token='isolated-deploy-load-token-123456'
phone_cookie=''
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
env=dict(os.environ,KURISU_HOME=str(root),KURISU_OPENCODE_BIN=str(fixture),KURISU_OPENCODE_TIMEOUT_SECS='120',KURISU_TELEGRAM_API=f'http://127.0.0.1:{api.server_port}',PATH=str(bins)+':'+os.environ['PATH'],KURISU_WEB_TOKEN=token)
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
def http_load(route,base):
 deadline=time.monotonic()
 while not halt.is_set():
  started=time.monotonic();status=None;error=None
  try:
   request=urllib.request.Request(base+route,headers={'Authorization':'Bearer '+token, 'Cookie':phone_cookie})
   with urllib.request.urlopen(request,timeout=10) as response:
    status=response.status;body=response.read()
    if '/api/' in route:
     payload=json.loads(body)
     if isinstance(payload,dict) and 'ok' in payload:payload=payload['ok']
     assert isinstance(payload,list) or isinstance(payload.get('sessions'),list),'API did not return sessions'
    else:assert b'html' in body.lower(),'web did not return HTML'
   assert status==200,status
  except Exception as e:error=repr(e)
  http_samples.append({'route':route,'started':started,'status':status,'error':error,'duration_ms':(time.monotonic()-started)*1000})
  deadline+=.03
  halt.wait(max(0,deadline-time.monotonic()))
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
 # Match the live instance's separate web frontend, on an OS-assigned test port.
 web=subprocess.Popen([str(binary),'web','--serve','--port','0'],env=env,stdout=(root/'web.log').open('w'),stderr=subprocess.STDOUT)
 until(lambda:(root/'web.json').exists(),'web discovery')
 listen=json.loads((root/'web.json').read_text())['listen']
 port=int(listen.rsplit(':',1)[1]);assert port not in (8791,8798,8793),port
 base='http://127.0.0.1:'+str(port)
 with socket.socket() as allocation:
  allocation.bind(('127.0.0.1',0));phone_port=allocation.getsockname()[1]
 assert phone_port not in (8791,8798,8793)
 (root/'weblink.json').write_text(json.dumps({'enabled':True,'listen':f'127.0.0.1:{phone_port}'}))
 req('remote_reload',kind='web')
 link=req('remote_link',kind='web')
 with urllib.request.urlopen(link['url'],timeout=10) as response:
  phone_cookie=response.headers['Set-Cookie'].split(';',1)[0];response.read()
 callbacks=[button['callback_data'] for message in messages for row in message.get('reply_markup',{}).get('inline_keyboard',[]) for button in row]
 attempt=next(value.split(':')[1] for value in reversed(callbacks) if value.startswith('pair:'))
 req('remote_decide',attempt=attempt,approve=True)
 phone_base=f'http://127.0.0.1:{phone_port}'
 loads=[threading.Thread(target=http_load,args=(route,origin),daemon=True) for origin,route in [(base,'/web'),(base,'/api/v1/sessions'),(phone_base,'/_web/'),(phone_base,'/_web/api/sessions')]]
 for load in loads:load.start()
 until(lambda:len(http_samples)>=10,'continuous HTTP load')
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
 for handover in range(2,6):
  r=deploy();(root/f'handover-{handover}.log').write_text(r.stdout)
  assert r.returncode==0,r.stdout
  time.sleep(.3)
  assert req('send',name=sid,text=f'after handover {handover}')['reply']=='PONG'
 time.sleep(.3)
 halt.set();thread.join(timeout=3)
 for load in loads:load.join(timeout=12)
 (root/'samples.json').write_text(json.dumps(samples,indent=2))
 primary=[sum(not s['standby'] for s in x['states']) for x in samples]
 poller=[sum(s['telegram']=='running' for s in x['states']) for x in samples]
 print('SAMPLED',len(samples),'public/slot snapshots; primary count range',min(primary),max(primary),'Telegram poller count range',min(poller),max(poller),flush=True)
 assert max(primary)<=1 and max(poller)<=1,'duplicate primary or Telegram poller'
 print('PASS no duplicate primary or poller; standby processes recorded separately',flush=True)
 events=[json.loads(line) for line in (root/'deploy-events.jsonl').read_text().splitlines()]
 gaps=[];running_gaps=[];stopped=None;requested=None
 for event in events:
  if event['event']=='poller_stopped':stopped=event['at_unix_ns']
  elif event['event']=='promotion_requested' and stopped is not None:requested=event['at_unix_ns'];gaps.append((requested-stopped)/1e6)
  elif event['event']=='poller_started' and stopped is not None:running_gaps.append((event['at_unix_ns']-stopped)/1e6);stopped=None
 assert len(gaps)==len(running_gaps)==5,(gaps,running_gaps)
 failures=[x for x in http_samples if x['error'] is not None or x['status']!=200]
 report={'endpoint':base,'phone_endpoint':phone_base,'interval_ms_per_route':30,'request_count':len(http_samples),'failures':len(failures),'routes':{route:sum(x['route']==route for x in http_samples) for route in ['/web','/api/v1/sessions','/_web/','/_web/api/sessions']},'owner_samples':len(samples),'max_primary':max(primary),'max_pollers':max(poller),'poller_stop_to_promotion_request_ms':gaps,'poller_stop_to_running_ms':running_gaps,'max_handover_window_ms':max(gaps),'max_http_duration_ms':max(x['duration_ms'] for x in http_samples)}
 (root/'http-samples.json').write_text(json.dumps(http_samples,indent=2))
 (root/'acceptance.json').write_text(json.dumps(report,indent=2))
 print('RELEASE ACCEPTANCE '+json.dumps(report),flush=True)
 assert not failures,failures[:10]
 print('PASS five consecutive handovers: every HTTP request 200; never two primary owners or Telegram pollers',flush=True)
finally:
 halt.set()
 if web is not None:
  web.terminate()
  try:web.wait(timeout=5)
  except subprocess.TimeoutExpired:web.kill();web.wait()
 try:req('shutdown')
 except Exception:pass
 try:server.wait(timeout=5)
 except subprocess.TimeoutExpired:server.kill();server.wait()
 api.shutdown();api.server_close()
