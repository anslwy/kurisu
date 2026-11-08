#!/usr/bin/env python3
"""Web/Telegram shared queue regression; fresh /tmp homes, local ACP, no web port."""
import concurrent.futures,json,os,socket,subprocess,sys,tempfile,time
from pathlib import Path
binary=str(Path(sys.argv[1]).resolve())
fixture=Path(__file__).parent/'fixtures/opencode-acp.py'
for outcome in ('complete','error','interrupt','stuck_interrupt'):
 with tempfile.TemporaryDirectory(prefix='ku-web-queue-',dir='/tmp') as tmp:
  root=Path(tmp)
  bins=root/'bin';bins.mkdir();(bins/'opencode').symlink_to(fixture.resolve());(bins/'python3').symlink_to(sys.executable)
  env=dict(os.environ,KURISU_HOME=tmp,PATH=str(bins),KURISU_OPENCODE_BIN=str(fixture.resolve()),KURISU_OPENCODE_TIMEOUT_SECS='120',FIXTURE_IGNORE_CANCEL='1' if outcome=='stuck_interrupt' else '0')
  def req(op,**fields):
   with socket.socket(socket.AF_UNIX) as s:
    s.settimeout(15);s.connect(str(root/'kurisu.sock'));s.sendall((json.dumps(dict(op=op,**fields))+'\n').encode());r=json.loads(s.makefile().readline())
    if 'error' in r:raise RuntimeError(r['error'])
    return r['ok']
  def until(check):
   for _ in range(500):
    if check():return
    assert p.poll() is None,(root/'server.log').read_text()
    time.sleep(.02)
   raise AssertionError((outcome,'timed out',(root/'server.log').read_text()))
  def prompts(text):
   path=root/'fixture-requests.jsonl'
   if not path.exists():return []
   return [r for line in path.read_text().splitlines(keepends=True) if line.endswith('\n') for r in [json.loads(line)] if r.get('method')=='session/prompt' and r['params']['prompt'][-1]['text'].endswith(text)]
  def delivery(seq):
   return next(((m.get('data') or {}).get('delivery') for m in req('history_page',name=sid)['messages'] if m['seq']==seq),None)
  with (root/'server.log').open('w') as log:
   p=subprocess.Popen([binary,'serve'],env=env,stdout=log,stderr=log)
   try:
    until(lambda:(root/'kurisu.sock').exists())
    sid=req('create',name='queue',harness='opencode',model='fixture/free',cwd=str(root/'work'))['id']
    assert req('send',name=sid,text='hello')['reply']=='PONG'
    bound=req('status',name=sid)['harness_session_id']
    first='FIXTURE_RELEASE_WAIT first' if outcome in ('complete','error') else 'FIXTURE_WAIT first'
    req('send',name=sid,text=first,wait=False);until(lambda:len(prompts(first))==1)
    # Keep the queued turn busy: Stop must target the old turn, not the queue.
    queued='FIXTURE_RELEASE_WAIT queued' if 'interrupt' in outcome else 'queued follow-up'
    r=req('send',name=sid,text=queued,wait=False,mode='queue')
    assert r['queued'] and not r['steered'],r
    assert delivery(r['seq'])=='queued'
    assert not prompts(queued),'queued message reached the busy harness'
    during=None
    if outcome=='stuck_interrupt':
     with concurrent.futures.ThreadPoolExecutor() as pool:
      stopping=pool.submit(req,'interrupt',name=sid)
      until(lambda:'session/cancel' in (root/'fixture-requests.jsonl').read_text())
      during=req('send',name=sid,text='queued during interrupt',wait=False,mode='queue')
      assert during['queued'] and delivery(during['seq'])=='queued'
      assert stopping.result()['stopped']
    elif 'interrupt' in outcome:assert req('interrupt',name=sid)['stopped']
    else:(root/('fixture-release-'+bound)).write_text('error' if outcome=='error' else 'complete')
    until(lambda:len(prompts(queued))==1)
    assert delivery(r['seq'])=='delivered'
    if 'interrupt' in outcome:
     assert req('status',name=sid)['status']=='working','queued turn was stopped too'
     (root/('fixture-release-'+bound)).write_text('complete')
    until(lambda:req('status',name=sid)['status']=='idle')
    assert req('send',name=sid,text='after queue')['reply']=='PONG'
    assert len(prompts(queued))==1,'duplicate queued delivery'
    if during:
     assert delivery(during['seq'])=='delivered'
     assert len(prompts('queued during interrupt'))==1
    assert req('status',name=sid)['harness_session_id']==bound
    # Unsupported steering uses the same explicit queue fallback as Telegram.
    req('send',name=sid,text='FIXTURE_WAIT steer target',wait=False)
    until(lambda:bool(prompts('FIXTURE_WAIT steer target')))
    r=req('send',name=sid,text='steering fallback',wait=False,mode='steer')
    assert r['queued'] and not r['steered'],r
    assert req('interrupt',name=sid)['stopped']
    until(lambda:bool(prompts('steering fallback')))
    until(lambda:req('status',name=sid)['status']=='idle')
    assert delivery(r['seq'])=='delivered' and len(prompts('steering fallback'))==1
    print('PASS',outcome,'busy queue, exactly-once boundary delivery, steering fallback survives Stop',flush=True)
   finally:
    try:req('shutdown')
    except Exception:pass
    try:p.wait(timeout=5)
    except subprocess.TimeoutExpired:p.kill();p.wait()
