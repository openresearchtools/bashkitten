#!/usr/bin/env python3
"""Explicit live compaction check; no credentials are read by this script."""
import hashlib,json,os,pathlib,runpy,socket,subprocess,sys,tempfile,time
state_path=pathlib.Path(sys.argv[1]);binary=sys.argv[2]
s=json.loads(state_path.read_text());phase=next(p for p in s['phases'] if p['name']=='real-compaction-and-resume');sid=phase['session']
d=pathlib.Path(os.environ['BASHKITTEN_DATA_DIR'])/'sessions'/sid
config_path=pathlib.Path(os.environ['BASHKITTEN_CONFIG_DIR'])/'config.json'
def write_config(value):
 fd,name=tempfile.mkstemp(dir=config_path.parent,prefix='.live-settings-')
 with os.fdopen(fd,'w') as f:json.dump(value,f);f.flush();os.fsync(f.fileno())
 os.replace(name,config_path)
c=json.loads(config_path.read_text());original=c.get('compaction');c['compaction']={'enabled':True,'reserveTokens':16384,'keepRecentTokens':1000};write_config(c)
phase['beforeRealCompactionSha256']=hashlib.sha256((d/'000001.jsonl').read_bytes()).hexdigest()
try:
 subprocess.run([binary,'session','compact',sid],check=True)
finally:
 c=json.loads(config_path.read_text())
 if original is None:c.pop('compaction',None)
 else:c['compaction']=original
 write_config(c)
fd=os.open(d,os.O_RDONLY|os.O_DIRECTORY)
connection=socket.socket(socket.AF_UNIX);connection.settimeout(120);connection.connect(f'/proc/self/fd/{fd}/control.sock');os.close(fd)
connection.sendall(b'{"type":"subscribe"}\n')
# Match the actual serde tag used by BashKitten's control protocol.
stream=connection.makefile('r');events=[];done=False
for line in stream:
 value=json.loads(line)
 batch=value.get('data',{}).get('events',[]) if value.get('message')=='subscribed' else [value]
 for event in batch:
  events.append(event)
  if event.get('type')=='compaction_end':done=True
 if done:break
phase['events']=events
phase['segments']=[p.name for p in sorted(d.glob('*.jsonl'))]
phase['oldSegmentUnchanged']=hashlib.sha256((d/'000001.jsonl').read_bytes()).hexdigest()==phase['beforeRealCompactionSha256']
end=next((e for e in reversed(events) if e.get('type')=='compaction_end'),{})
phase['compactionPassed']=bool(end.get('result')) and len(phase['segments'])==2 and phase['oldSegmentUnchanged']
phase['result']=f"Live summary/rotation passed: {phase['compactionPassed']}. Segments: {phase['segments']}. Old segment unchanged: {phase['oldSegmentUnchanged']}. Error: {end.get('errorMessage')}. Used Pi's configurable keepRecentTokens=1000 for this small task and restored the test configuration afterward."
runpy.run_path('scripts/live-model-tasks.py')['save'](state_path,s)
print(json.dumps({'passed':phase['compactionPassed'],'end':end,'segments':phase['segments'],'oldSegmentUnchanged':phase['oldSegmentUnchanged']},indent=2))
