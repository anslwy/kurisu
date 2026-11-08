#!/usr/bin/env python3
"""OpenCode ACP lifecycle integration. Local mock only; fresh /tmp home, no Telegram."""
import concurrent.futures,json,os,socket,subprocess,sys,tempfile,time
from pathlib import Path
binary=str(Path(sys.argv[1]).resolve());fixture=Path(__file__).parent/'fixtures/opencode-acp.py'
with tempfile.TemporaryDirectory(prefix='ku-opencode-',dir='/tmp') as tmp:
 root=Path(tmp);bins=root/'bin';bins.mkdir();(bins/'opencode').symlink_to(fixture.resolve())
 (bins/'codex').symlink_to('/bin/true')
 env=dict(os.environ,KURISU_HOME=tmp,PATH=str(bins)+':/usr/bin:/bin',KURISU_OPENCODE_BIN=str(bins/'opencode'),KURISU_OPENCODE_TIMEOUT_SECS='1')
 def start():
  p=subprocess.Popen([binary,'serve'],env=env,stdout=open(root/'server.log','a'),stderr=subprocess.STDOUT)
  for _ in range(100):
   if (root/'kurisu.sock').exists():return p
   assert p.poll() is None
   time.sleep(.05)
  raise AssertionError('no socket')
 def req(op,**fields):
  with socket.socket(socket.AF_UNIX) as s:
   s.settimeout(15);s.connect(str(root/'kurisu.sock'));s.sendall((json.dumps(dict(op=op,**fields))+'\n').encode());r=json.loads(s.makefile().readline())
   if 'error' in r:raise RuntimeError(r['error'])
   return r['ok']
 def fails(op,**fields):
  try:req(op,**fields)
  except RuntimeError as e:return str(e)
  raise AssertionError('expected error')
 p=start()
 try:
  s=req('create',name='open',harness='opencode',model='fixture/free',cwd=str(root/'work'));sid=s['id']
  assert req('send',name=sid,text='hello')['reply']=='PONG'
  bound=req('status',name=sid)['harness_session_id'];assert bound
  assert req('models',harness='opencode',model='fixture/no-effort')['config_options'][0]['id']=='model'
  req('update',name=sid,fields={'effort':'high'})
  assert 'does not advertise' in fails('update',name=sid,fields={'effort':'medium'})
  assert 'unavailable' in fails('update',name=sid,fields={'permission_mode':'auto'})
  req('update',name=sid,fields={'permission_mode':'plan'})
  assert req('send',name=sid,text='after changes')['reply']=='PONG'
  assert req('compact',name=sid)['summary']==''
  history=req('history_page',name=sid,tools=True)['messages'];assert any(x['role']=='tool' for x in history)
  with concurrent.futures.ThreadPoolExecutor() as pool:
   for allow in (True,False):
    f=pool.submit(req,'send',name=sid,text='FIXTURE_APPROVAL')
    for _ in range(100):
     approvals=req('approvals',name=sid)
     if approvals:break
     time.sleep(.02)
    assert approvals
    req('approve' if allow else 'deny',id=approvals[0]['id'])
    assert f.result()['reply']==('APPROVED' if allow else 'DENIED')
   f=pool.submit(req,'send',name=sid,text='FIXTURE_WAIT')
   time.sleep(.2);req('interrupt',name=sid);assert f.result()['stop_reason']=='Cancelled'
  assert 'fixture provider failure' in fails('send',name=sid,text='FIXTURE_ERROR')
  assert 'timed out' in fails('send',name=sid,text='FIXTURE_WAIT')
  assert req('send',name=sid,text='recover timeout')['reply']=='PONG'
  assert 'disconnected' in fails('send',name=sid,text='FIXTURE_EOF')
  assert req('send',name=sid,text='recover EOF')['reply']=='PONG'
  req('stop',name=sid);assert req('send',name=sid,text='after stop')['reply']=='PONG'
  req('shutdown');p.wait(timeout=10);p=start()
  assert req('send',name=sid,text='after restart')['reply']=='PONG'
  assert req('status',name=sid)['harness_session_id']==bound
  # A second same-folder peer must receive a separate ACP process/MCP identity.
  other=req('create',name='other',harness='opencode',model='fixture/free',cwd=str(root/'work'))
  assert req('send',name=other['id'],text='other')['reply']=='PONG'
  req('send',name=other['id'],text='callback proof',caller=sid,wait=False)
  for _ in range(100):
   h=req('history_page',name=sid,search='Kurisu callback')['messages']
   if h:break
   time.sleep(.02)
  assert h,'missing OpenCode peer completion callback'
  req('update',name=other['id'],fields={'effort':'high'})
  req('update',name=other['id'],fields={'model':'fixture/no-effort'})
  assert req('status',name=other['id'])['effort'] is None
  assert req('send',name=other['id'],text='without effort')['reply']=='PONG'
  old=req('reharness',name=sid,harness='codex',seed=False)['new']
  new=req('reharness',name=old['id'],harness='opencode',model='fixture/free',seed=False)['new']
  assert new['cwd']==s['cwd']
  assert req('handoff',name=new['id'])
  assert req('send',name=new['id'],text='replacement')['reply']=='PONG'
  report=req('usage_report');assert report['harnesses']['opencode']['confidence']=='unavailable'
  print('PASS ACP create/load/history/config/effort/stream/tool/approval/deny/cancel/error/timeout/EOF/recovery/stop/restart/handoff/usage')
 finally:
  try:req('shutdown')
  except Exception:pass
  try:p.wait(timeout=5)
  except subprocess.TimeoutExpired:p.kill();p.wait()
