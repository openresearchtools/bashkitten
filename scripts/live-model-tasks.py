#!/usr/bin/env python3
"""Explicit, opt-in live model checks. Uses the caller's BashKitten configuration.

Creates disposable task folders and independently evaluates results. Never reads
or copies credentials. This script is not part of the installed application.
"""
import argparse, hashlib, json, os, pathlib, subprocess, tempfile, time

CASES = [
    {
        'name': 'csv-inventory-repair', 'model': 'openai-codex/gpt-5.6-luna',
        'files': {
            'inventory.py': '''# TODO: repeated categories must accumulate, including negative amounts.
def totals(rows):
    result = {}
    for row in rows:
        result[row["category"]] = row["amount"]
    return result
''',
            'data/records.csv': 'category,amount\nHardware,12\nSoftware,9\nHardware,-3\n',
            'tests/test_inventory.py': '''import unittest
from inventory import totals
class InventoryTests(unittest.TestCase):
    def test_accumulate(self):
        self.assertEqual(totals([{"category":" A ","amount":"12"},{"category":"A","amount":"-3"}]),{"A":9})
    def test_empty(self): self.assertEqual(totals([]),{})
    def test_invalid(self):
        with self.assertRaises(ValueError): totals([{"category":"A","amount":"oops"}])
'''
        },
        'oracle': '''import unittest
from inventory import totals
class IndependentTests(unittest.TestCase):
    def test_separate_buckets_and_signed_integers(self):
        self.assertEqual(totals([{"category":"x ","amount":"-10"},{"category":" y","amount":"0"},{"category":" x","amount":"+23"}]),{"x":13,"y":0})
    def test_generator(self):
        self.assertEqual(totals({"category":"a","amount":str(n)} for n in range(20)),{"a":190})
    def test_bad_value(self):
        with self.assertRaises(ValueError): totals([{"category":"a","amount":"1.5"}])
''',
        'prompt': 'Repair inventory.totals: strip surrounding whitespace from category names, parse each amount as a signed base-10 integer, accumulate duplicate categories, return an empty dict for no rows, and propagate ValueError on invalid amounts. Inspect the project with ls, find, grep and read; use edit for the code change, write a short README.md explaining usage, and use bash to run the supplied tests. Use all seven built-in tools at least once. Do not change the supplied tests. Work only in this working folder. Report actual test results.',
        'required': ['README.md'], 'all_tools': True,
    },
    {
        'name': 'config-parser-refactor', 'model': 'openai-codex/gpt-5.6-sol',
        'files': {
            'parser.py': '''def parse_config(text):
    return dict(line.split("=") for line in text.splitlines())
''',
            'tests/test_parser.py': r'''import unittest
from parser import parse_config
class ParserTests(unittest.TestCase):
    def test_basic(self): self.assertEqual(parse_config("a = one\nb=two"),{"a":"one","b":"two"})
    def test_empty(self): self.assertEqual(parse_config(""),{})
    def test_comments(self): self.assertEqual(parse_config("# comment\n\na=3"),{"a":"3"})
'''
        },
        'oracle': '''import unittest
from parser import parse_config
class IndependentTests(unittest.TestCase):
    def test_bom_crlf_duplicate_equals(self):
        self.assertEqual(parse_config("\\ufeff# heading\\r\\nx = first\\r\\nx = a=b=c\\r\\nempty=\\r\\n"),{"x":"a=b=c","empty":""})
    def test_comments_and_value(self):
        self.assertEqual(parse_config("  # comment\\na=# keep me\\n"),{"a":"# keep me"})
    def test_error_line(self):
        with self.assertRaisesRegex(ValueError,"line 3"):
            parse_config("# comment\\na=1\\nbroken")
    def test_empty_key(self):
        with self.assertRaisesRegex(ValueError,"line 2"):
            parse_config("a=1\\n = value")
''',
        'prompt': 'Refactor parse_config in parser.py into a small reliable dependency-free key=value parser. Strip one initial UTF-8 BOM, support LF/CRLF, skip blank lines and lines whose first non-whitespace character is #. Split only at the first =; trim key/value outer whitespace, retain # and further equals inside values, allow empty values, and let the last duplicate key win. For a missing = or empty key raise ValueError mentioning the original 1-based line number, including skipped lines. Do not modify supplied tests. Add your own edge-case tests separately, document the behavior in README.md, and run the tests with bash. Work only in this folder.',
        'required': ['README.md'],
    },
]

def save(path, value):
    path.write_text(json.dumps(value, indent=2)+'\n')
    lines=['# Real model run results','', 'These are actual BashKitten session processes using the configured OpenAI subscription. A task passes only after independent checks. This is not a claim of complete Pi parity.','', '| Task | Model | Status | Session |','| --- | --- | --- | --- |']
    for task in value.get('tasks',[]):
        lines.append(f"| {task['name']} | {task['model']} | {task['status']} | `{task.get('session') or 'not started'}` |")
    for task in value.get('tasks',[]):
        lines += ['',f"## {task['name']}",'',f"Working folder: `{task['folder']}`"]
        if 'tools' in task: lines += ['', 'Tools observed: '+', '.join(task['tools'])+'.',f"Supplied tests unchanged: {task['testsUnchanged']}."]
        for check in task.get('checks',[]): lines += ['',f"{check['name']} checks (exit {check['exitCode']}):",'```text',check['output'].strip(),'```']
        for error in task.get('errors',[]): lines += ['',f'Provider failure: {error}']
    for phase in value.get('phases',[]): lines += ['',f"## {phase['name']}",'',phase.get('result','Pending')]
    for failure in value.get('failures',[]): lines += ['',f"## Observed failure: {failure['name']}",'',failure['observed'],'',failure['repair'],'',failure['verification']]
    path.with_suffix('.md').write_text('\n'.join(lines)+'\n')

def run(args):
    env = dict(os.environ, BASHKITTEN_AGENT_BIN=str(pathlib.Path(args.agent_binary).resolve()))
    if args.action == 'start':
        root=pathlib.Path(tempfile.mkdtemp(prefix='bashkitten-live-'))
        state={'started':time.strftime('%Y-%m-%dT%H:%M:%SZ',time.gmtime()),'root':str(root),'tasks':[]}
        for case in CASES:
            folder=root/case['name'];folder.mkdir()
            hashes={}
            for name,content in case['files'].items():
                p=folder/name;p.parent.mkdir(parents=True,exist_ok=True);p.write_text(content)
                if name.startswith('tests/'): hashes[name]=hashlib.sha256(p.read_bytes()).hexdigest()
            oracle=root/'oracles'/case['name'];oracle.mkdir(parents=True)
            (oracle/'test_independent.py').write_text(case['oracle'])
            (folder/'TASK.md').write_text(case['prompt']+'\n')
            task={'name':case['name'],'model':case['model'],'thinking':'medium','folder':str(folder),'oracle':str(oracle),'testHashes':hashes,'required':case['required'],'allToolsRequired':case.get('all_tools',False),'status':'starting'}
            state['tasks'].append(task);save(args.state,state)
            result=subprocess.run([args.binary,'session','start','--cwd',str(folder),'--model',case['model'],'--thinking','medium','--prompt',case['prompt']],env=env,text=True,capture_output=True)
            task.update({'session':result.stdout.strip().splitlines()[-1] if result.returncode==0 else None,'status':'running' if result.returncode==0 else 'launch_failed','launchError':result.stderr.strip() if result.returncode else ''})
            save(args.state,state)
            print(json.dumps({k:task[k] for k in ['name','model','folder','session','status','launchError']}),flush=True)
        return
    state=json.loads(args.state.read_text())
    listing=subprocess.run([args.binary,'session','list','--json'],env=env,text=True,capture_output=True,check=True)
    running={x['id']:x['running'] for x in json.loads(listing.stdout)}
    data=pathlib.Path(os.environ['BASHKITTEN_DATA_DIR'])/'sessions'
    for task in state['tasks']:
        sid=task['session']
        if not sid: continue
        if running.get(sid): print(json.dumps({'name':task['name'],'status':'running'}));continue
        folder=pathlib.Path(task['folder']);tools=[];errors=[];cache=[];responses=[]
        seen=set()
        for p in sorted((data/sid).glob('*.jsonl')):
            for line in p.read_text().splitlines():
                entry=json.loads(line)
                if entry.get('id') in seen: continue
                if entry.get('id'): seen.add(entry['id'])
                m=entry.get('message',{})
                if m.get('role')=='assistant':
                    content=m.get('content',[])
                    tools.extend(x['name'] for x in content if x.get('type')=='toolCall')
                    responses.extend(x.get('text','') for x in content if x.get('type')=='text')
                    if m.get('errorMessage'): errors.append(m['errorMessage'])
                    cache.append(m.get('usage',{}))
        untouched=all(hashlib.sha256((folder/name).read_bytes()).hexdigest()==digest for name,digest in task['testHashes'].items())
        checkenv=dict(env,PYTHONPATH=str(folder))
        checks=[]
        for label,source in [('supplied',str(folder/'tests')),('independent',task['oracle'])]:
            result=subprocess.run(['python3','-m','unittest','discover','-s',source,'-v'],cwd=folder,env=checkenv,text=True,capture_output=True,timeout=15)
            checks.append({'name':label,'exitCode':result.returncode,'output':result.stdout+result.stderr})
        missing=[name for name in task['required'] if not (folder/name).is_file()]
        alltools=(not task['allToolsRequired']) or set(tools)>={'bash','read','edit','write','grep','find','ls'}
        task.update({'status':'passed' if not errors and untouched and not missing and alltools and all(x['exitCode']==0 for x in checks) else 'failed','tools':tools,'errors':errors,'checks':checks,'testsUnchanged':untouched,'missingFiles':missing,'allToolsUsed':alltools,'usage':cache,'finalText':responses[-1] if responses else ''})
        print(json.dumps({'name':task['name'],'model':task['model'],'status':task['status'],'tools':tools,'errors':errors,'checks':[{k:c[k] for k in ['name','exitCode']} for c in checks]}),flush=True)
    state['checked']=time.strftime('%Y-%m-%dT%H:%M:%SZ',time.gmtime());save(args.state,state)

if __name__=='__main__':
    p=argparse.ArgumentParser();p.add_argument('action',choices=['start','check']);p.add_argument('--binary',required=True);p.add_argument('--agent-binary',required=True);p.add_argument('--state',type=pathlib.Path,required=True);run(p.parse_args())
