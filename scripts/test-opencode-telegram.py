#!/usr/bin/env python3
"""Telegram routing against loopback mocks and fresh /tmp homes; no Telegram traffic."""
import http.server,json,os,queue,socket,subprocess,sys,tempfile,threading,time
from pathlib import Path
binary=str(Path(sys.argv[1]).resolve());fixture=Path(__file__).parent/'fixtures/opencode-acp.py'
with tempfile.TemporaryDirectory(prefix='ku-opencode-tg-',dir='/tmp') as tmp:
 root=Path(tmp);bins=root/'bin';bins.mkdir();(bins/'opencode').symlink_to(fixture.resolve());(bins/'codex').symlink_to('/bin/true')
 updates=queue.Queue();messages=[];uid=0
 class Telegram(http.server.BaseHTTPRequestHandler):
  def log_message(self,*_):pass
  def do_GET(self):
   if '/file/' in self.path:
    self.send_response(200);self.end_headers();self.wfile.write(b'local attachment fixture');return
   self.do_POST()
  def do_POST(self):
   data=json.loads(self.rfile.read(int(self.headers.get('Content-Length',0))) or '{}');method=self.path.rsplit('/',1)[-1];result=True
   if method=='getUpdates':
    result=[]
    if data.get('offset',-1)>=0:
     try:result=[updates.get(timeout=.1)]
     except queue.Empty:pass
   elif method=='getMe':result={'id':7,'username':'local_test_bot','is_bot':True}
   elif method in ('sendMessage','editMessageText'):
    messages.append(data);result={'message_id':len(messages),'chat':{'id':42},'text':data.get('text','')}
   elif method=='getFile':result={'file_id':'attachment','file_path':'doc.txt','file_size':24}
   body=json.dumps({'ok':True,'result':result}).encode();self.send_response(200);self.end_headers()
   try:self.wfile.write(body)
   except BrokenPipeError:pass
 api=http.server.ThreadingHTTPServer(('127.0.0.1',0),Telegram);assert api.server_port!=8791
 threading.Thread(target=api.serve_forever,daemon=True).start()
 (root/'telegram.json').write_text(json.dumps({'token':'local-fake','chat_id':'42'}))
 env=dict(os.environ,KURISU_HOME=tmp,PATH=str(bins)+':/usr/bin:/bin',KURISU_OPENCODE_BIN=str(bins/'opencode'),KURISU_OPENCODE_TIMEOUT_SECS='10',KURISU_TELEGRAM_API=f'http://127.0.0.1:{api.server_port}')
 server=subprocess.Popen([binary,'serve'],env=env,stdout=open(root/'server.log','w'),stderr=subprocess.STDOUT)
 def req(op,**fields):
  with socket.socket(socket.AF_UNIX) as s:
   s.settimeout(15);s.connect(str(root/'kurisu.sock'));s.sendall((json.dumps(dict(op=op,**fields))+'\n').encode());r=json.loads(s.makefile().readline())
   assert 'error' not in r,r
   return r['ok']
 def until(pred,what):
  for _ in range(400):
   if pred():return
   assert server.poll() is None,'server exited'
   time.sleep(.025)
  raise AssertionError((what,messages[-8:],(root/'server.log').read_text()[-2000:]))
 def put(text=None,callback=None,attachment=None,chat=42,thread=None):
  global uid
  uid+=1
  if callback:u={'callback_query':{'id':str(uid),'from':{'id':42},'message':{'message_id':uid,'chat':{'id':42,'type':'private'}},'data':callback}}
  else:
   msg={'message_id':uid,'date':int(time.time()),'chat':{'id':chat,'type':'private' if chat==42 else 'supergroup'},'from':{'id':42}}
   if thread is not None:msg['message_thread_id']=thread
   if attachment:msg.update(attachment)
   else:msg['text']=text
   u={'message':msg}
  updates.put(dict(update_id=uid,**u))
 def command(text,expected=None):
  n=len(messages);put(text)
  until(lambda:any(expected.lower() in json.dumps(m).lower() for m in messages[n:]) if expected else len(messages)>n,text)
  return messages[-1]
 def buttons(msg):return [b for row in msg.get('reply_markup',{}).get('inline_keyboard',[]) for b in row]
 def click(data,expected=None):
  n=len(messages);put(callback=data);until(lambda:any(expected.lower() in json.dumps(m).lower() for m in messages[n:]) if expected else len(messages)>n,data);return messages[-1]
 try:
  until(lambda:(root/'kurisu.sock').exists(),'socket')
  s=req('create',name='open',harness='opencode',model='fixture/free',cwd=str(root/'work'));sid=s['id']
  command('/use '+sid)
  command('/start','Kurisu');command('/help','Kurisu');command('/id',sid)
  command('/status','opencode');command('/list','open');command('/use '+sid[:8])
  report=command('/usage','OpenCode: provider quota unavailable');assert '0%' not in report['text']
  card=command('/model','pick a harness')
  card=click(next(b for b in buttons(card) if '(opencode)' in b['text'])['callback_data'],'Fixture Free')
  card=click(next(b for b in buttons(card) if b['text'].endswith('Fixture Free'))['callback_data'],'effort')
  assert [b['text'].lstrip('✓ ') for b in buttons(card)]==['default','high'],card
  click(next(b for b in buttons(card) if b['text'].endswith('high'))['callback_data'],'effort')
  until(lambda:req('status',name=sid)['effort']=='high','selected effort')
  command('/model fixture/no-effort','model');command('/model fixture/free','model')
  command('/mode','OpenCode permissions');command('/mode plan','now plan')
  command('/mode auto','unavailable');command('/mode default','now default')
  command('/mcp','kurisu');command('/schedule','wake');command('/wakes','wake')
  for route in ('/browser','/web','/terminal'):command(route)
  command('hello','PONG');time.sleep(3.5)
  command('/compact this','Compaction complete')
  compact=command('/compact','compact')
  click(next(b for b in buttons(compact) if b['callback_data'].startswith('cmp:a:'))['callback_data'],'status')
  command('/compact','compact');command('/catch-up','PONG');command('/catch_up','PONG')
  n=len(messages);put(attachment={'document':{'file_id':'attachment','file_name':'doc.txt','file_size':24},'caption':'Read local fixture'})
  until(lambda:any('PONG' in m.get('text','') for m in messages[n:]),'document route')
  assert list((root/'work'/'.kurisu-uploads').glob('*doc.txt'))
  put('FIXTURE_WAIT');until(lambda:req('status',name=sid)['status']=='working','active prompt')
  command('/stop')
  until(lambda:req('status',name=sid)['status'] in ('stopped','idle'),'stop')
  command('/send-now');command('/send_now')
  time.sleep(3.5)
  for action,answer in [('a','APPROVED'),('d','DENIED')]:
   n=len(messages);put('FIXTURE_APPROVAL')
   until(lambda:any(any(b['callback_data'].startswith('approval:') for b in buttons(m)) for m in messages[n:]),'approval card')
   card=next(m for m in messages[n:] if any(b['callback_data'].startswith('approval:') for b in buttons(m)))
   click(next(b for b in buttons(card) if b['callback_data'].endswith(':'+action))['callback_data'])
   until(lambda:any(answer in m.get('text','') for m in messages[n:]),'approval result')
   time.sleep(3.5)
  command('/new','Started a new session')
  peers=req('peers');new=next(x for x in peers if x['name'].startswith('telegram-'))
  assert new['harness']=='opencode' and new['model']=='fixture/free',new
  command('/reset','Started a new session')
  assert all(x['harness']=='opencode' for x in req('peers') if x['name'].startswith('telegram-'))
  cross=req('create',name='cross',harness='codex',cwd=str(root/'cross'))
  command('/use '+cross['id'])
  card=command('/model','pick a harness')
  card=click(next(b for b in buttons(card) if '(opencode)' in b['text'])['callback_data'],'Fixture Free')
  card=click(next(b for b in buttons(card) if b['text'].endswith('Fixture Free'))['callback_data'],'effort')
  card=click(next(b for b in buttons(card) if b['text'].endswith('high'))['callback_data'],'Switch')
  click(next(b for b in buttons(card) if b['callback_data'].startswith('mx:') and b['callback_data'].endswith(':y'))['callback_data'],'Switched')
  replaced=next(x for x in req('peers') if x['name']=='cross')
  assert replaced['harness']=='opencode' and replaced['effort']=='high',replaced
  command('/status','opencode')
  n=len(messages);put('/use '+replaced['id'],chat=-100,thread=99)
  until(lambda:any(m.get('chat_id')==-100 and 'Now using' in m.get('text','') for m in messages[n:]),'topic selection')
  n=len(messages);put('topic hello',chat=-100,thread=99)
  until(lambda:any(m.get('chat_id')==-100 and m.get('message_thread_id')==99 and 'PONG' in m.get('text','') for m in messages[n:]),'topic reply')
  print('PASS Telegram approvals/cross-harness handoff/group-topic routing; help/start/use/id/status/list/usage/model+harness+effort callbacks/mode/mcp/wakes/remote links/plain text/compact/catchup/document/stop/send-now/new/reset')
 finally:
  server.terminate()
  try:server.wait(timeout=10)
  except subprocess.TimeoutExpired:server.kill();server.wait()
  api.shutdown();api.server_close()
