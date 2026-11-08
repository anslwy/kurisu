#!/usr/bin/env python3
"""Installed-CLI subset startup/default/menu regression, using fake CLIs and mock Telegram.

No model turns, real Telegram requests, CLI installs or production ports. Every
case has a fresh /tmp KURISU_HOME and an isolated PATH. Pass a built ku binary.
"""
import http.server,itertools,json,os,queue,socket,subprocess,sys,tempfile,threading,time
from pathlib import Path
BINARY=str(Path(sys.argv[1]).resolve())
HARNESS=('claude','codex','opencode')
FIXTURE=Path(__file__).parent/'fixtures/opencode-acp.py'

def check(installed):
 with tempfile.TemporaryDirectory(prefix='ku-cli-matrix-',dir='/tmp') as tmp:
  root=Path(tmp);bins=root/'bin';bins.mkdir();(bins/'python3').symlink_to(sys.executable)
  for h in installed:
   path=bins/h
   if h=='opencode':path.symlink_to(FIXTURE.resolve());continue
   path.write_text(f'''#!{sys.executable}
import json,sys,pathlib,os
with (pathlib.Path(os.environ['KURISU_HOME'])/'cli-calls').open('a') as f:f.write({h!r}+' '+json.dumps(sys.argv[1:])+'\\n')
if 'app-server' in sys.argv:sys.exit(99)
if '--version' in sys.argv:print({h!r}+' fixture')
elif 'auth' in sys.argv:print('{{"loggedIn":true}}')
elif 'login' in sys.argv:print('Logged in using fixture')
elif '--help' in sys.argv:print('--permission-mode choices: "default", "auto", "plan"')
''');path.chmod(0o755)
  updates=queue.Queue();messages=[]
  class Telegram(http.server.BaseHTTPRequestHandler):
   def log_message(self,*_):pass
   def do_POST(self):
    data=json.loads(self.rfile.read(int(self.headers.get('Content-Length',0))) or '{}');method=self.path.rsplit('/',1)[-1];result=True
    if method=='getUpdates':
     result=[]
     if data.get('offset',-1)>=0:
      try:result=[updates.get(timeout=.05)]
      except queue.Empty:pass
    elif method=='getMe':result={'id':7,'username':'matrix_test_bot','is_bot':True}
    elif method in ('sendMessage','editMessageText'):
     messages.append(data);result={'message_id':len(messages)}
    body=json.dumps({'ok':True,'result':result}).encode();self.send_response(200);self.end_headers()
    try:self.wfile.write(body)
    except BrokenPipeError:pass
  api=http.server.ThreadingHTTPServer(('127.0.0.1',0),Telegram);assert api.server_port!=8791
  threading.Thread(target=api.serve_forever,daemon=True).start()
  (root/'telegram.json').write_text(json.dumps({'token':'local-fake','chat_id':'42'}))
  env=dict(os.environ,KURISU_HOME=tmp,PATH=str(bins),KURISU_TELEGRAM_API=f'http://127.0.0.1:{api.server_port}')
  for name in ('KURISU_DEFAULT_HARNESS','KURISU_CLAUDE_BIN','KURISU_OPENCODE_BIN'):env.pop(name,None)
  log=(root/'server.log').open('w');server=subprocess.Popen([BINARY,'serve'],env=env,stdout=log,stderr=log)
  def until(pred):
   for _ in range(300):
    if pred():return
    assert server.poll() is None,(root/'server.log').read_text()
    time.sleep(.02)
   raise AssertionError((installed,messages,(root/'server.log').read_text()))
  def req(op,**args):
   with socket.socket(socket.AF_UNIX) as s:
    s.settimeout(10);s.connect(str(root/'kurisu.sock'));s.sendall((json.dumps(dict(op=op,**args))+'\n').encode());return json.loads(s.makefile().readline())
  def command(uid,text):
   n=len(messages);updates.put({'update_id':uid,'message':{'message_id':uid,'date':int(time.time()),'chat':{'id':42,'type':'private'},'from':{'id':42},'text':text}});until(lambda:len(messages)>n);return messages[-1]
  try:
   until(lambda:(root/'kurisu.sock').exists())
   assert req('ping')['ok']['pid']
   assert not (root/'cli-calls').exists(),'server eagerly invoked a CLI'
   assert not (root/'fixture-requests.jsonl').exists(),'server eagerly started ACP'
   default=req('create',name='default-peer')
   if installed:
    expected=next(h for h in HARNESS if h in installed)
    assert default['ok']['harness']==expected,default
    for h in HARNESS:
     r=req('create',name=h+'-peer',harness=h)
     if h in installed:assert r['ok']['harness']==h,r
     else:assert 'not found on PATH' in r['error'] and h in r['error'],r
    command(1,'/use '+default['ok']['id'])
    menu=command(2,'/model')
    buttons=[b for row in menu.get('reply_markup',{}).get('inline_keyboard',[]) for b in row]
    if len(installed)>1:
     assert {h for h in HARNESS if any('('+h+')' in b['text'] for b in buttons)}==set(installed),menu
    else:
     callbacks=[b.get('callback_data','') for b in buttons]
     assert any(c.startswith('model:') for c in callbacks),menu
     assert all(c.startswith(('model:','mp:','ms:')) for c in callbacks),menu
     assert 'Page 1/' in menu.get('text',''),menu
     assert callbacks[-1].startswith('ms:'),'model page must end with Search: %r'%menu
     if installed[0] in ('claude','codex'):
      assert any(c.startswith('mp:') for c in callbacks),'model page must offer Next: %r'%menu
    # Catalog discovery may start installed OpenCode ACP; Codex/Claude agents
    # must remain unloaded until an actual turn is requested.
    calls=(root/'cli-calls').read_text() if (root/'cli-calls').exists() else ''
    assert 'app-server' not in calls,calls
   else:
    assert 'no supported CLI found on PATH' in default['error'],default
    for h in HARNESS:assert 'not found on PATH' in req('create',name=h+'-peer',harness=h)['error']
   assert req('ping')['ok']['pid'],'server lost health after selection errors'
   print('PASS',','.join(installed) or 'none','healthy startup; default/installed/missing selection; lazy agents; model menu' if installed else 'healthy empty server; actionable create errors',flush=True)
  finally:
   server.terminate()
   try:server.wait(timeout=5)
   except subprocess.TimeoutExpired:server.kill();server.wait()
   log.close();api.shutdown();api.server_close()

for count in range(4):
 for installed in itertools.combinations(HARNESS,count):check(installed)
