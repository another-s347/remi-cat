#!/usr/bin/env python3
"""Compare paired runs; separate model decisions from transport retries."""
from collections import Counter
import argparse
import json
from pathlib import Path
from rounds import response


def summarize(root):
    runs=[]
    for path in sorted(root.glob('*/result.json')):
        run=json.loads(path.read_text()); usage=Counter(); errors=Counter(); calls={}; returned={}
        for p in sorted((path.parent/'requests').glob('*.meta.json')):
            m=json.loads(p.read_text());usage['attempts']+=1
            if m.get('usage'):
                u=m['usage'];usage['model_rounds']+=1
                for k in ['prompt_tokens','completion_tokens']:usage[k]+=u.get(k,0)
                usage['cached_tokens']+=u.get('cached_tokens',u.get('prompt_tokens_details',{}).get('cached_tokens',0))
            else: errors[str(m.get('status'))]+=1
            raw=p.with_name(p.name.replace('.meta.json','.response.txt'))
            if raw.exists():
                current,_=response(raw)
                for c in current:calls[c['id']]=c
                if current and all(c['name'].startswith('todo') for c in current):usage['todo_only_rounds']+=1
        for p in (path.parent/'requests').glob('*.sent.json'):
            for m in json.loads(p.read_text()).get('messages',[]):
                if m['role']=='tool': returned[m['tool_call_id']]=str(m.get('content',''))
        usage['uncached_prompt_tokens']=usage['prompt_tokens']-usage['cached_tokens']
        mechanisms=Counter()
        for cid,text in returned.items():
            if calls.get(cid,{}).get('name')=='fs_read':
                mechanisms['read_result_bytes']+=len(text.encode())
                if '[Large file preview;' in text:mechanisms['initial_previews']+=1
            if calls.get(cid,{}).get('name')=='fs_write' and 'No such file or directory' in text:
                mechanisms['missing_parent_writes']+=1
            if 'fs_read output too large' in text:mechanisms['discarded_reads']+=1
        for c in calls.values():
            if c['name']=='todo__complete' or (c['name']=='todo' and json.loads(c['arguments']).get('action')=='complete'):
                a=json.loads(c['arguments'])
                if len(a.get('ids',[]))>1:mechanisms['batch_completions']+=1
        run.update(usage=dict(usage),transport_errors=dict(errors),mechanisms=dict(mechanisms))
        runs.append(run)
    total=Counter();errors=Counter();mechanisms=Counter()
    for r in runs:total.update(r['usage']);errors.update(r['transport_errors']);mechanisms.update(r['mechanisms'])
    return dict(runs=runs,totals=dict(total),transport_errors=dict(errors),mechanisms=dict(mechanisms),passed=sum(r['passed'] for r in runs))


def main():
    p=argparse.ArgumentParser(description=__doc__);p.add_argument('root',type=Path);args=p.parse_args()
    result={name:summarize(args.root/name) for name in ['before','after']}
    (args.root/'comparison.json').write_text(json.dumps(result,ensure_ascii=False,indent=2))
    for name,g in result.items():
        print(name,g['passed'],'/',len(g['runs']),g['totals'],g['transport_errors'],g['mechanisms'])
        for r in g['runs']:print(r['id'],r['passed'],r['usage'],r['mechanisms'])


if __name__=='__main__':main()
