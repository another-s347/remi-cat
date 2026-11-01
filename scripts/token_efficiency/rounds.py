#!/usr/bin/env python3
"""Reconstruct output actions per actual request; audit the six retry traces.

Labels are explicit manual annotations of request IDs, not model judgments.
Input messages contain earlier actions; response files contain the current action.
"""
import argparse
from collections import Counter
import json
from pathlib import Path

ANNOTATIONS = {
    'remi-retry-1': {'control': [1,9,12,16,19], 'discovery': [2,3,4,5,6,7,8], 'mutation': [10,11,15], 'parent_recovery': [13,14], 'verification': [17,18], 'final': [20]},
    'remi-retry-2': {'control': [1,7,10,14,16], 'discovery': [2,3,4,5,6], 'mutation': [8,9,13], 'parent_recovery': [11,12], 'verification': [15], 'final': [17]},
    'opencode-retry-1': {'auxiliary': [1], 'discovery': [2,3,4,5,6,7], 'mutation': [8,9], 'verification': [10], 'final': [11]},
    'opencode-retry-2': {'auxiliary': [1], 'discovery': [2,3,4,5,6], 'mutation': [7,8], 'verification': [9], 'final': [10]},
    'dsh-retry-1': {'auxiliary': [2], 'discovery': [1,3,4,5,6,7,8], 'control': [9,11,13,15,17], 'mutation': [10,12,14], 'verification': [16], 'final': [18]},
    'dsh-retry-2': {'auxiliary': [2], 'discovery': [1,3,4,5], 'control': [6,8,10,12,14], 'mutation': [7,9,11], 'verification': [13], 'final': [15]},
}


def response(path):
    calls={}; text=''
    for line in path.read_text().splitlines():
        if not line.startswith('data: '): continue
        try: event=json.loads(line[6:])
        except ValueError: continue
        for choice in event.get('choices',[]):
            delta=choice.get('delta',{})
            text+=delta.get('content') or ''
            for tool in delta.get('tool_calls',[]):
                call=calls.setdefault(tool.get('index',0),dict(id='',name='',arguments=''))
                call['id']+=tool.get('id') or ''
                for key in ['name','arguments']: call[key]+=tool.get('function',{}).get(key) or ''
    return list(calls.values()),text


def main():
    parser=argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--root',type=Path,default=Path('target/token-efficiency-2026-10-31'))
    parser.add_argument('--output',type=Path,required=True)
    args=parser.parse_args(); result={}; aggregates={}
    for rid,annotations in ANNOTATIONS.items():
        harness=rid.split('-')[0]; group='dsh-main' if harness=='dsh' else 'main'
        root=args.root/group/rid/'requests'
        labels={number:label for label,numbers in annotations.items() for number in numbers}
        rows=[]; returned={}
        for path in sorted(root.glob('*.sent.json')):
            for message in json.loads(path.read_text()).get('messages',[]):
                if message.get('role')=='tool': returned[message['tool_call_id']]=message.get('content')
        for path in sorted(root.glob('*.meta.json')):
            meta=json.loads(path.read_text()); n=meta['request']
            calls,text=response(root/f'{n:03d}.response.txt')
            sent=json.loads((root/f'{n:03d}.sent.json').read_text())
            assert bool(sent.get('tools')) == (labels[n]!='auxiliary'),(rid,n)
            if labels[n]=='control': assert calls and all(c['name'].startswith('todo') for c in calls),(rid,n)
            if labels[n]=='final': assert not calls and text,(rid,n)
            for call in calls: call['result']=returned.get(call['id'])
            rows.append(dict(request=n,category=labels[n],calls=calls,text=text,usage=meta['usage']))
        assert len(rows)==len(labels),rid
        counts=Counter(row['category'] for row in rows)
        result[rid]=dict(counts=dict(counts),rounds=rows)
        agg=aggregates.setdefault(harness,dict(counts=Counter(),usage={}))
        agg['counts'].update(counts)
        for row in rows:
            u=agg['usage'].setdefault(row['category'],Counter())
            u.update(prompt_tokens=row['usage']['prompt_tokens'],completion_tokens=row['usage']['completion_tokens'])
        print(rid,dict(counts))
    args.output.parent.mkdir(parents=True,exist_ok=True)
    args.output.write_text(json.dumps(dict(aggregates=aggregates,runs=result),ensure_ascii=False,indent=2))
    for name,agg in aggregates.items(): print(name,json.dumps(agg,ensure_ascii=False))


if __name__=='__main__':main()
