#!/usr/bin/env python3
"""Opt-in installed Claude steering smoke test (uses its configured provider).
Fresh /tmp KURISU_HOME and private tmux server; no web listener or browser.
Run: python3 scripts/test-claude-steering.py target/debug/ku
"""
import json,os,socket,subprocess,sys,tempfile,time
from pathlib import Path
binary=str(Path(sys.argv[1]).resolve())
with tempfile.TemporaryDirectory(prefix='ku-claude-steer-',dir='/tmp') as tmp:
 root=Path(tmp);env=dict(os.environ,KURISU_HOME=tmp,KURISU_MCP_TIMEOUT='10')
 def req(op,**fields):
  with socket.socket(socket.AF_UNIX) as s:
   s.settimeout(90);s.connect(str(root/'kurisu.sock'));s.sendall((json.dumps(dict(op=op,**fields))+'\n').encode());r=json.loads(s.makefile().readline())
   if 'error' in r:raise RuntimeError(r['error'])
   return r['ok']
 h=5381
 for b in tmp.encode():h=((h*33)^b)&0xffffffff
 tmux=['tmux','-L',f'kurisu-{h:08x}']
 def trust_own_folder():
  panes=subprocess.run(tmux+['list-panes','-a','-F','#{pane_id}'],capture_output=True,text=True).stdout.splitlines()
  for pane in panes:
   screen=subprocess.run(tmux+['capture-pane','-p','-t',pane],capture_output=True,text=True).stdout
   if 'Yes, I trust this folder' in screen:subprocess.run(tmux+['send-keys','-t',pane,'Down','Enter'],check=True)
 with (root/'server.log').open('w') as log:
  p=subprocess.Popen([binary,'serve'],env=env,stdout=log,stderr=log)
  try:
   for _ in range(100):
    if (root/'kurisu.sock').exists():break
    time.sleep(.05)
   sid=req('create',name='steer',harness='claude',model='claude-haiku-5-5',cwd=str(root/'work'))['id']
   req('send',name=sid,text='Use Bash to run sleep 15, then reply with ORIGINAL. Do not do anything else.',wait=False)
   for _ in range(150):
    trust_own_folder();st=req('status',name=sid)
    if (st.get('activity') or {}).get('state')=='running':break
    assert st['status']!='error',(root/'server.log').read_text()
    time.sleep(.5)
   else:raise AssertionError('Claude did not start its tool')
   text='After the command finishes, reply with STEERING_PROBE instead of ORIGINAL.'
   r=req('send',name=sid,text=text,wait=False,mode='steer')
   assert r['steered'] and not r['queued'],r
   for _ in range(180):
    if req('status',name=sid)['status'] in ('idle','stopped','error'):break
    time.sleep(.5)
   else:raise AssertionError('steered turn did not finish')
   rows=req('history_page',name=sid)['messages']
   sent=[m for m in rows if m['seq']==r['seq']]
   assert len(sent)==1 and sent[0]['data']['delivery']=='steered',sent
   assert any('STEERING_PROBE' in m['text'] for m in rows if m['role']=='agent'),rows
   assert req('send',name=sid,text='Reply with just PONG.')['reply']=='PONG'
   print('PASS installed Claude: active tool steered, one persisted steering message, requested reply, subsequent send')
  finally:
   try:req('shutdown')
   except Exception:pass
   try:p.wait(timeout=5)
   except subprocess.TimeoutExpired:p.kill();p.wait()
