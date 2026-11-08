#!/usr/bin/env python3
"""Deterministic ACP agent fixture; never accesses a provider or credentials."""
import json,os,sys,uuid,threading,time
from pathlib import Path
if '--version' in sys.argv:
 print('opencode fixture');sys.exit()
root=Path(os.environ.get('KURISU_FIXTURE_HOME') or os.environ['KURISU_HOME']);state=root/'fixture-sessions.json'
with (root/'fixture-pids').open('a') as f:f.write(str(os.getpid())+'\n')
sessions=json.loads(state.read_text()) if state.exists() else {}
active={};approvals={}
emit_lock=threading.Lock()
def emit(v):
 with emit_lock:print(json.dumps(dict(jsonrpc='2.0',**v)),flush=True)
def result(i,r):emit(dict(id=i,result=r))
def err(i,m):emit(dict(id=i,error=dict(code=-32602,message=m)))
def update(s,u):emit(dict(method='session/update',params=dict(sessionId=s,update=u)))
def options(s):
 model=sessions[s].get('model','fixture/free')
 return [{'id':'model','name':'Model','category':'model','type':'select','currentValue':model,'options':[{'value':'fixture/free','name':'Fixture Free'},{'value':'fixture/no-effort','name':'No effort'}]},*([{'id':'effort','name':'Effort','category':'thought_level','type':'select','currentValue':'default','options':[{'value':'default','name':'Default'},{'value':'high','name':'High'}]}] if model!='fixture/no-effort' else [])]
def save():
 existing=json.loads(state.read_text()) if state.exists() else {}
 existing.update(sessions);state.write_text(json.dumps(existing))
def finish(s,i,text='PONG',reason='end_turn'):
 active.pop(s,None)
 update(s,dict(sessionUpdate='agent_thought_chunk',content=dict(type='text',text='Thinking')))
 update(s,dict(sessionUpdate='tool_call',toolCallId='read-1',title='Read fixture',kind='read',status='in_progress',rawInput={'file_path':'AGENTS.md'}))
 update(s,dict(sessionUpdate='tool_call_update',toolCallId='read-1',status='completed',rawOutput={'text':'fixture'}))
 update(s,dict(sessionUpdate='agent_message_chunk',content=dict(type='text',text=text)))
 result(i,dict(stopReason=reason))
def wait_for_release(s,i):
 def release():
  marker=root/('fixture-release-'+s)
  while not marker.exists() and active.get(s)==i:time.sleep(.01)
  if active.get(s)!=i:return
  if marker.read_text()=='error':
   active.pop(s,None);err(i,'fixture turn failed')
  else:finish(s,i)
 threading.Thread(target=release,daemon=True).start()
for line in sys.stdin:
 q=json.loads(line);m=q.get('method');p=q.get('params',{});i=q.get('id');s=p.get('sessionId')
 with (root/'fixture-requests.jsonl').open('a') as f:f.write(json.dumps(q)+'\n')
 if not m:
  entry=approvals.pop(i,None)
  if entry:
   s,prompt_id=entry
   choice=q.get('result',{}).get('outcome',{})
   finish(s,prompt_id,'APPROVED' if choice.get('optionId') in ('once','always') else 'DENIED')
  continue
 if m=='initialize':result(i,{'protocolVersion':1,'agentCapabilities':{'loadSession':True}})
 elif m in ('session/new','session/load'):
  if m=='session/new':
   s='fixture-'+str(uuid.uuid4());sessions[s]={'cwd':p['cwd'],'model':'fixture/free'}
  elif s not in sessions:
   sessions.update(json.loads(state.read_text()))
   if s not in sessions:err(i,'missing persisted session');continue
  if sessions[s]['cwd']!=p['cwd']:err(i,'wrong cwd');continue
  assert any(x['name']=='kurisu-mcp' and any(e['name']=='KURISU_SESSION' for e in x['env']) for x in p['mcpServers']) or not p['mcpServers']
  sessions[s]['mcp']=p['mcpServers'];save()
  update(s,{'sessionUpdate':'available_commands_update','availableCommands':[{'name':'compact','description':'Compact'}]})
  result(i,dict(configOptions=options(s),**({'sessionId':s} if m=='session/new' else {})))
 elif m=='session/set_config_option':
  if p['configId']=='model' and p['value'] not in ('fixture/free','fixture/no-effort'):err(i,'unknown model');continue
  if p['configId']=='effort' and p['value'] not in ('default','high'):err(i,'unknown effort');continue
  sessions[s][p['configId']]=p['value'];save();result(i,{'configOptions':options(s)})
 elif m=='session/set_mode':result(i,{})
 elif m=='session/delete':sessions.pop(s,None);save();result(i,{})
 elif m=='session/close':
  if s in active:finish(s,active[s],reason='cancelled')
  result(i,{})
 elif m=='session/cancel':
  if os.environ.get('FIXTURE_IGNORE_CANCEL') != '1' and s in active:finish(s,active[s],reason='cancelled')
 elif m=='session/prompt':
  text='\n'.join(x.get('text','') for x in p['prompt'])
  if 'FIXTURE_EOF' in text:sys.exit()
  if 'FIXTURE_ERROR' in text:err(i,'fixture provider failure');continue
  active[s]=i
  if 'FIXTURE_RELEASE_WAIT' in text:
   wait_for_release(s,i)
   continue
  if 'FIXTURE_COMMAND' in text:
   update(s,dict(sessionUpdate='tool_call',toolCallId='command-1',title='shell',kind='execute',status='in_progress',rawInput={'command':'cargo test --all\n  --locked -- --nocapture'}))
   wait_for_release(s,i)
   continue
  if 'FIXTURE_WAIT' in text:
   update(s,dict(sessionUpdate='tool_call',toolCallId='waiting',title='Waiting fixture',kind='execute',status='in_progress'))
   continue
  if 'FIXTURE_APPROVAL' in text:
   rid='approval-'+s;approvals[rid]=(s,i)
   emit(dict(id=rid,method='session/request_permission',params={'sessionId':s,'toolCall':{'toolCallId':rid,'title':'Run fixture command','name':'Bash','rawInput':{'command':'echo fixture'}},'options':[{'optionId':'once','name':'Allow','kind':'allow_once'},{'optionId':'always','name':'Always','kind':'allow_always'},{'optionId':'reject','name':'Deny','kind':'reject_once'}]}));continue
  if text=='/compact':
   update(s,{'sessionUpdate':'session_info_update','_meta':{'opencode/compaction':{'status':'completed','messageId':'compact-fixture','reason':'manual'}}})
   result(i,{'stopReason':'end_turn'});active.pop(s,None);continue
  finish(s,i)
 else:err(i,'unsupported method '+str(m))
