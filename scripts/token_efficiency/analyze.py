#!/usr/bin/env python3
"""Aggregate authoritative API usage and separately labelled component estimates."""
import argparse
from collections import Counter, defaultdict
import json
from pathlib import Path
import statistics
import tiktoken

ENC = tiktoken.get_encoding('cl100k_base')


def output_parts(path):
    texts=defaultdict(str)
    if not path.exists(): return {}
    for line in path.read_text().splitlines():
        if line.startswith('data: '): line=line[6:]
        try: event=json.loads(line)
        except ValueError: continue
        for choice in event.get('choices',[]):
            delta=choice.get('delta',choice.get('message',{}))
            texts['visible_text']+=delta.get('content') or ''
            texts['reasoning_text']+=delta.get('reasoning_content') or ''
            for tool in delta.get('tool_calls',[]):
                fun=tool.get('function',{});i=tool.get('index',0)
                texts[f'arguments_{i}']+=fun.get('arguments') or ''
                texts[f'name_{i}']+=fun.get('name') or ''
    out=Counter()
    for key,text in texts.items():
        category='tool_arguments' if key.startswith('arguments_') else 'tool_names' if key.startswith('name_') else key
        out[category]+=len(ENC.encode(text,disallowed_special=()))
    return dict(out)


def analyze(root):
    result=json.loads((root/'result.json').read_text())
    totals=Counter(); parts=Counter(); tool_calls=Counter(); errors=[]; calls=[]; outputs=Counter()
    unique={}; transmitted=Counter(); auxiliary=Counter(); todo_followups=Counter()
    for path in sorted((root/'requests').glob('*.meta.json')):
        meta=json.loads(path.read_text()); body=json.loads(path.with_name(path.name.replace('.meta.json','.sent.json')).read_text())
        usage=meta.get('usage')
        totals['requests']+=1
        if meta.get('status')!=200: errors.append({'request':meta['request'],'status':meta.get('status'),'error':meta.get('error')})
        if not usage:
            totals['missing_usage']+=1
            continue
        for key in ['prompt_tokens','completion_tokens','total_tokens']:
            totals[key]+=usage.get(key,0)
        cached=usage.get('cached_tokens',usage.get('prompt_tokens_details',{}).get('cached_tokens',0))
        totals['cached_tokens']+=cached
        totals['uncached_prompt_tokens']+=max(0,usage.get('prompt_tokens',0)-cached)
        totals['reasoning_tokens']+=usage.get('completion_tokens_details',{}).get('reasoning_tokens',0)
        parts.update(meta['components_cl100k_estimate'])
        outputs.update(output_parts(path.with_name(path.name.replace('.meta.json','.response.txt'))))
        messages=body.get('messages',[])
        text=json.dumps(messages,ensure_ascii=False)
        is_aux='title' in text.lower() and not body.get('tools')
        if is_aux:
            auxiliary.update(prompt_tokens=usage.get('prompt_tokens',0),completion_tokens=usage.get('completion_tokens',0),requests=1)
        latest=next((m for m in reversed(messages) if m.get('role')=='assistant' and m.get('tool_calls')),None)
        names=[t.get('function',{}).get('name','') for t in latest['tool_calls']] if latest else []
        if names and all(n.startswith('todo') for n in names):
            todo_followups.update(requests=1,prompt_tokens=usage.get('prompt_tokens',0),completion_tokens=usage.get('completion_tokens',0),cached_tokens=cached)
        for m in messages:
            identity=json.dumps(m,ensure_ascii=False,sort_keys=True)
            transmitted[m.get('role')]+=len(identity)
            unique[identity]=m.get('role')
        calls.append(dict(request=meta['request'],usage=usage,components=meta['components_cl100k_estimate'],auxiliary_candidate=is_aux))
    # The last full request contains all preceding assistant tool calls on these
    # short, noncompacting runs. Take max per id to avoid counting replayed history.
    seen={}
    for path in sorted((root/'requests').glob('*.sent.json')):
        d=json.loads(path.read_text())
        for m in d.get('messages',[]):
            for t in m.get('tool_calls',[]): seen[t['id']]=t.get('function',{}).get('name','?')
    tool_calls.update(seen.values())
    result.update(usage=dict(totals),components_cl100k_estimate=dict(parts),output_cl100k_estimate=dict(outputs),tool_calls=dict(tool_calls),auxiliary_candidates=dict(auxiliary),todo_only_followups=dict(todo_followups),errors=errors,calls=calls)
    result['workspace_tests']=[str(p.relative_to(root/'workspace')) for p in (root/'workspace').rglob('*test*.py')]
    original=[p for p in (root/'requests').glob('*.original.json')]
    result['request_files']=len(original)
    result['unfinished_requests']=len(original)-totals['requests']
    return result


def main():
    p=argparse.ArgumentParser(description=__doc__); p.add_argument('roots',nargs='+',type=Path); p.add_argument('--output',required=True,type=Path); args=p.parse_args()
    rows=[]
    for root in args.roots:
        for path in sorted(root.glob('*/result.json')): rows.append(analyze(path.parent))
    groups=defaultdict(list)
    for row in rows: groups[row['harness']].append(row)
    aggregates={}
    for name,rs in groups.items():
        usage=Counter(); parts=Counter(); tools=Counter(); outputs=Counter()
        for r in rs: usage.update(r['usage']);parts.update(r['components_cl100k_estimate']);tools.update(r['tool_calls']);outputs.update(r['output_cl100k_estimate'])
        aggregates[name]=dict(runs=len(rs),passed=sum(r['passed'] for r in rs),usage=dict(usage),components_cl100k_estimate=dict(parts),output_cl100k_estimate=dict(outputs),tool_calls=dict(tools),median_seconds=statistics.median(r['elapsed'] for r in rs))
    args.output.parent.mkdir(exist_ok=True,parents=True)
    args.output.write_text(json.dumps(dict(aggregates=aggregates,runs=rows),ensure_ascii=False,indent=2))
    print('| Harness | Pass | API input | cached | uncached | API output | calls | median s |')
    print('|---|---:|---:|---:|---:|---:|---:|---:|')
    for name,g in aggregates.items():
        u=g['usage'];print(f"| {name} | {g['passed']}/{g['runs']} | {u.get('prompt_tokens',0)} | {u.get('cached_tokens',0)} | {u.get('uncached_prompt_tokens',0)} | {u.get('completion_tokens',0)} | {u.get('requests',0)} | {g['median_seconds']:.1f} |")
    for r in rows: print(r['id'],r['passed'],r['usage'],r['tool_calls'])


if __name__=='__main__':main()
