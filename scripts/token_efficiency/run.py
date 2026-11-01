#!/usr/bin/env python3
"""Run real harnesses through a metering proxy. Never logs authentication headers.

Requires tiktoken (component counts are cl100k estimates, NOT Kimi billing).
API usage is saved verbatim. Credentials are read into memory from one dotenv key.
"""
import argparse
from collections import Counter
from concurrent.futures import ThreadPoolExecutor
import hashlib
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
import json
import os
import random
from pathlib import Path
import signal
import subprocess
import sys
import threading
import time
import urllib.error
import urllib.request

import tiktoken
from tasks import TASKS, CHECKS, setup

REPO = Path(__file__).resolve().parents[2]
ENC = tiktoken.get_encoding("cl100k_base")
LOCK = threading.Lock()
COUNTS = Counter()
ROOT = None
KEY = None
MODEL = "kimi-k2.6"
SETUP_TASK = None


def dump(path, value):
    path.write_text(json.dumps(value, ensure_ascii=False, indent=2))


def estimate(value):
    text = value if isinstance(value, str) else json.dumps(value, ensure_ascii=False, separators=(',', ':'))
    return len(ENC.encode(text, disallowed_special=()))


def components(body):
    counts = Counter(tool_schema=estimate(body.get('tools', [])))
    tools = []
    for m in body.get('messages', []):
        role = m.get('role', 'unknown')
        counts[role] += estimate(m)
        if role == 'assistant':
            tools.extend(x.get('function', {}).get('name', '?') for x in m.get('tool_calls', []))
    return dict(counts), dict(Counter(tools))


class Proxy(BaseHTTPRequestHandler):
    protocol_version = 'HTTP/1.1'

    def log_message(self, *args):
        pass

    def do_GET(self):
        data = json.dumps({'data': [{'id': MODEL, 'object': 'model'}]}).encode()
        self.send_response(200); self.send_header('Content-Type','application/json'); self.send_header('Content-Length',str(len(data))); self.end_headers(); self.wfile.write(data)

    def do_POST(self):
        run_id = self.path.split('/')[1]
        if not run_id or any(c not in 'abcdefghijklmnopqrstuvwxyz0123456789-_' for c in run_id):
            self.send_error(400); return
        directory = ROOT / run_id / 'requests'
        directory.mkdir(parents=True, exist_ok=True)
        with LOCK:
            COUNTS[run_id] += 1
            number = COUNTS[run_id]
        stem = directory / f'{number:03d}'
        original = json.loads(self.rfile.read(int(self.headers['Content-Length'])))
        dump(stem.with_suffix('.original.json'), original)
        body = dict(original)
        # Controlled model settings for every harness, including auxiliary calls.
        body.update(model=MODEL, max_tokens=8192, temperature=0.6, thinking={'type': 'disabled'})
        body.pop('max_completion_tokens', None)
        body.pop('reasoning_effort', None)
        if body.get('stream'):
            body['stream_options'] = {'include_usage': True}
        dump(stem.with_suffix('.sent.json'), body)
        parts, tool_history = components(body)
        meta = dict(request=number, components_cl100k_estimate=parts, history_tool_calls=tool_history, usage=None, status=None)
        started = time.monotonic()
        request = urllib.request.Request('https://api.moonshot.cn/v1/chat/completions', json.dumps(body).encode(), {'Authorization': 'Bearer '+KEY, 'Content-Type':'application/json'})
        try:
            upstream = urllib.request.urlopen(request, timeout=180)
        except urllib.error.HTTPError as e:
            data = e.read()
            meta.update(status=e.code, error=data.decode(errors='replace'), elapsed=time.monotonic()-started)
            dump(stem.with_suffix('.meta.json'), meta)
            self.send_response(e.code); self.send_header('Content-Length',str(len(data))); self.end_headers(); self.wfile.write(data)
            return
        except Exception as e:
            meta.update(error=type(e).__name__+': '+str(e), elapsed=time.monotonic()-started)
            dump(stem.with_suffix('.meta.json'),meta)
            self.send_error(502); return
        meta['status'] = upstream.status
        self.send_response(upstream.status)
        self.send_header('Content-Type', upstream.headers.get('Content-Type','application/json'))
        self.send_header('Connection','close'); self.end_headers()
        self.close_connection = True
        response = bytearray()
        try:
            while True:
                chunk = upstream.readline() if body.get('stream') else upstream.read()
                if not chunk: break
                response.extend(chunk)
                try:
                    obj = json.loads(chunk[6:] if chunk.startswith(b'data: ') else chunk)
                    if obj.get('usage'): meta['usage'] = obj['usage']
                except (ValueError, TypeError): pass
                self.wfile.write(chunk); self.wfile.flush()
                if not body.get('stream'): break
        except (BrokenPipeError, ConnectionResetError) as e:
            meta['client_disconnect'] = type(e).__name__
        finally:
            upstream.close()
            stem.with_suffix('.response.txt').write_bytes(response)
            meta['elapsed'] = time.monotonic()-started
            dump(stem.with_suffix('.meta.json'),meta)


def clean_env(home):
    env = {k:v for k,v in os.environ.items() if not any(x in k.upper() for x in ['API_KEY','TOKEN','SECRET','PASSWORD']) and not k.startswith(('REMI_', 'AGENT_', 'OPENAI_', 'ANTHROPIC_', 'MOONSHOT_', 'DSH_', 'OPENCODE_'))}
    env.update(HOME=str(home), XDG_CONFIG_HOME=str(home/'config'), XDG_DATA_HOME=str(home/'data'), XDG_CACHE_HOME=str(home/'cache'), TMPDIR='/tmp', NO_PROXY='127.0.0.1,localhost', no_proxy='127.0.0.1,localhost', RUST_LOG='error', REMI_PROFILE_REGISTRY_ROOT=str(home/'registry'), TIKTOKEN_CACHE_DIR='/tmp/remi-token-bench/tiktoken-cache')
    return env


def invoke(command, cwd, env, timeout, out, err):
    with out.open('w') as stdout, err.open('w') as stderr:
        child = subprocess.Popen(command,cwd=cwd,env=env,stdout=stdout,stderr=stderr,start_new_session=True)
        try: return child.wait(timeout=timeout), False
        except subprocess.TimeoutExpired:
            os.killpg(child.pid, signal.SIGTERM)
            try: child.wait(timeout=5)
            except subprocess.TimeoutExpired: os.killpg(child.pid,signal.SIGKILL); child.wait()
            return child.returncode, True


def run_case(args, harness, task, repeat):
    rid = f'{harness}-{task}-{repeat}'
    root = ROOT/rid
    root.mkdir()
    workspace=root/'workspace'
    if SETUP_TASK is None: setup(workspace)
    else: SETUP_TASK(workspace,task)
    home=root/'home'; home.mkdir()
    env=clean_env(home)
    if args.suite=='extended':
        rustbin=Path('/home/skye/.rustup/toolchains/stable-x86_64-unknown-linux-gnu/bin')
        env['PATH']=str(args.node.parent)+os.pathsep+str(rustbin)+os.pathsep+env['PATH']
    endpoint=f'http://127.0.0.1:{args.port}/{rid}/v1'
    env['PWD']=str(workspace)
    prompt=TASKS[task]+f' Your workspace is {workspace}. Work only inside that directory. Finish autonomously without asking questions. Do not delegate to other agents.'
    if harness.startswith('remi'):
        profile=root/'profile'
        init=subprocess.run([str(args.remi),'profile','init',str(profile),'--id','bench','--name','Bench','--with-runtime'],cwd=workspace,env=env,capture_output=True,text=True,timeout=40)
        if init.returncode: raise RuntimeError(init.stderr[-1000:])
        manifest=profile/'profile.yaml'
        resolved=json.loads(subprocess.check_output([str(args.remi),'profile','show',str(manifest),'--resolved','--format','json'],cwd=workspace,env=env,text=True))
        modeldir=Path(resolved['resources']['models']); modeldir.mkdir(exist_ok=True,parents=True)
        (modeldir/'bench.yaml').write_text(f'id: bench\nname: Bench\nprovider: openai\nmodel: {MODEL}\nbase_url: {endpoint}\nmax_output_tokens: 8192\ncontext_tokens: 256000\nsupports_images: false\ncontext_compaction: hard\noverflow_bytes: 24000\n')
        import yaml
        manifest_data=yaml.safe_load(manifest.read_text()); manifest_data['workspace']=str(workspace); manifest.write_text(yaml.safe_dump(manifest_data))
        config=Path(resolved['config']['runtime']); data=yaml.safe_load(config.read_text()); data.update(model_profile='bench',root_agent_id='default',sandbox={'kind':'no_sandbox'},shell={'mode':'disabled'}); config.write_text(yaml.safe_dump(data))
        if args.remi_agent_file:
            (Path(resolved['resources']['agents'])/'default.md').write_bytes(args.remi_agent_file.read_bytes())
        if harness=='remi-lean':
            agent=Path(resolved['resources']['agents'])/'default.md'
            agent.write_text('---\nid: default\nname: Remi\nmodel: bench\nhelper_model: bench\ntools: [bash, fs_read, fs_write, apply_patch, fs_ls, rg]\ndelegates: []\nmax_turns: 40\npersistent_sessions: true\n---\nYou are a coding assistant. Complete the requested task autonomously. Read relevant files, make focused changes, and verify them. Be concise.\n')
        elif harness=='remi-no-todo':
            agent=Path(resolved['resources']['agents'])/'default.md'
            _,front,body=agent.read_text().split('---',2)
            spec=yaml.safe_load(front)
            spec['tools']=[t for t in spec['tools'] if t!='todo' and not t.startswith('todo__')]
            body='\n'.join(line for line in body.splitlines() if not line.startswith('For multi-step tasks,'))
            agent.write_text('---\n'+yaml.safe_dump(spec)+'---\n'+body)
        env.update(OPENAI_API_KEY='metered-local-proxy',KIMI_API_KEY='metered-local-proxy',MOONSHOT_API_KEY='metered-local-proxy',OPENAI_BASE_URL=endpoint,REMI_MODEL_PROFILE='bench',REMI_A2A_PORT='0')
        command=[str(args.remi),'--profile',str(manifest),'prompt','--permissions','medium',prompt]
        if args.remi_wait_background_tasks:
            command.insert(-1, '--wait-background-tasks')
    elif harness=='opencode':
        configuration={'$schema':'https://opencode.ai/config.json','provider':{'bench':{'npm':'@ai-sdk/openai-compatible','name':'Bench','options':{'baseURL':endpoint,'apiKey':'metered-local-proxy'},'models':{MODEL:{'name':MODEL,'limit':{'context':256000,'output':8192}}}}},'model':'bench/'+MODEL,'permission':'allow'}
        cfg=root/'opencode.json'; dump(cfg,configuration)
        env.update(OPENCODE_CONFIG=str(cfg),OPENCODE_DISABLE_AUTOUPDATE='1')
        command=[str(args.opencode),'--standalone','run','--auto','--format','json','--model','bench/'+MODEL,prompt]
    else:
        patch=root/'bench.patch.yml'
        patch.write_text(f'''- id: agent-default-model
  config:
    provider: moonshotai
    model: {MODEL}
- id: llm-pi-ai
  config:
    providers:
      moonshotai:
        apiKeyEnv: MOONSHOT_API_KEY
        baseURL: {endpoint}
        models:
          - id: {MODEL}
            contextWindow: 256000
            maxTokens: 8192
''')
        env.update(DSH_HOME=str(home/'dsh'), MOONSHOT_API_KEY='metered-local-proxy')
        env['PATH']=str(args.node.parent)+os.pathsep+env['PATH']
        command=[str(args.node),str(args.dsh),'headless','--patch',str(patch),'--json',prompt]
    dump(root/'invocation.json',{'command':command,'model':MODEL,'task':task,'repeat':repeat,'harness':harness})
    started=time.monotonic()
    code,timed_out=invoke(command,workspace,env,args.timeout,root/'stdout.txt',root/'stderr.txt')
    elapsed=time.monotonic()-started
    check_env=dict(os.environ,BENCH_NODE=str(args.node))
    check=subprocess.run([sys.executable,'-c',CHECKS[task]],cwd=workspace,env=check_env,capture_output=True,text=True,timeout=180 if args.suite=='extended' else 30)
    (root/'acceptance.txt').write_text(check.stdout+check.stderr)
    result=dict(id=rid,harness=harness,task=task,repeat=repeat,exit_code=code,timed_out=timed_out,elapsed=elapsed,passed=check.returncode==0)
    dump(root/'result.json',result)
    print(json.dumps(result),flush=True)
    return result


def main():
    global ROOT,KEY,MODEL,TASKS,CHECKS,SETUP_TASK
    parser=argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--output',type=Path,required=True)
    parser.add_argument('--remi',type=Path,default=REPO/'target/debug/remi-cat')
    parser.add_argument('--remi-agent-file',type=Path,help='Override the isolated default agent file without rebuilding the frozen binary')
    parser.add_argument('--remi-wait-background-tasks',action='store_true',help='Use the existing CLI waiting/async mode; external timeout still applies')
    parser.add_argument('--opencode',type=Path,default=Path('/tmp/remi-token-bench/opencode-runtime/node_modules/@opencode/cli-linux-x64/bin/opencode'))
    parser.add_argument('--dsh',type=Path,default=Path('/tmp/remi-token-bench/runtime/node_modules/@deepseek-ai/dsh/lib/bin.js'))
    parser.add_argument('--node',type=Path,default=Path('/tmp/remi-token-bench/runtime/node_modules/node-linux-x64/bin/node'))
    parser.add_argument('--harness',nargs='+',default=['remi','opencode','dsh'])
    parser.add_argument('--suite',choices=['short','extended'],default='short')
    parser.add_argument('--task',nargs='+')
    parser.add_argument('--repeats',type=int,default=2)
    parser.add_argument('--timeout',type=int,default=300)
    parser.add_argument('--workers',type=int,default=2)
    parser.add_argument('--port',type=int,default=18473)
    parser.add_argument('--order-seed',type=int,help='Shuffle the complete job list reproducibly')
    args=parser.parse_args()
    if args.suite=='extended':
        from extended_tasks import TASKS as suite_tasks,CHECKS as suite_checks,setup as suite_setup
        TASKS,CHECKS,SETUP_TASK=suite_tasks,suite_checks,suite_setup
    args.task=args.task or list(TASKS)
    if any(task not in TASKS for task in args.task):parser.error('Unknown task for selected suite')
    ROOT=args.output.resolve()
    if ROOT == REPO or REPO in ROOT.parents:
        parser.error('Run workspaces outside this repository (for example /tmp/remi-token-bench/...) to prevent ancestor skill discovery; copy evidence back after completion.')
    ROOT.mkdir(parents=True,exist_ok=True)
    for line in (REPO/'.env').read_text().splitlines():
        if line.startswith('KIMI_API_KEY='): KEY=line.split('=',1)[1].strip().strip('\"\'')
    if not KEY: raise RuntimeError('KIMI_API_KEY unavailable')
    server=ThreadingHTTPServer(('127.0.0.1',args.port),Proxy)
    threading.Thread(target=server.serve_forever,daemon=True).start()
    jobs=[(h,t,r) for r in range(1,args.repeats+1) for t in args.task for h in args.harness]
    if args.order_seed is not None: random.Random(args.order_seed).shuffle(jobs)
    dump(ROOT/'experiment.json',{'suite':args.suite,'tasks':args.task,'timeout_seconds':args.timeout,'model':MODEL,'max_tokens':8192,'thinking':'disabled','temperature':0.6,'tokenizer':'cl100k_base estimates only','repeats':args.repeats,'workers':args.workers,'order_seed':args.order_seed,'job_order':jobs,'remi_sha256':hashlib.sha256(args.remi.read_bytes()).hexdigest(),'remi_head':subprocess.check_output(['git','rev-parse','HEAD'],cwd=REPO,text=True).strip(),'dirty':subprocess.check_output(['git','status','--short'],cwd=REPO,text=True)})
    results=[]
    with ThreadPoolExecutor(max_workers=args.workers) as pool:
        futures=[pool.submit(run_case,args,*job) for job in jobs]
        for future in futures:
            try: results.append(future.result())
            except Exception as e: print(type(e).__name__+': '+str(e),flush=True)
    dump(ROOT/'results.json',results)
    server.shutdown()


if __name__=='__main__': main()
