"""Independent Node runtime acceptance, outside the agent workspace."""
import os,subprocess
NODE=os.environ.get('BENCH_NODE','/tmp/remi-token-bench/runtime/node_modules/node-linux-x64/bin/node')
CODE=r'''
import test from 'node:test';
import assert from 'node:assert/strict';
import {pathToFileURL} from 'node:url';
import {resolve,join} from 'node:path';
import {mkdtempSync,writeFileSync,rmSync} from 'node:fs';
import {tmpdir} from 'node:os';
import {spawnSync} from 'node:child_process';
const {runGraph}=await import(pathToFileURL(resolve('src/index.mjs')));
const wait=ms=>new Promise(r=>setTimeout(r,ms));
test('empty and direct dependency values',async()=>{
 assert.deepEqual((await runGraph([])).results,{});
 const s=await runGraph([{id:'sum',deps:['a','b'],run:({results})=>results.a+results.b},{id:'a',run:()=>2},{id:'b',run:async()=>3}]);
 assert.equal(s.results.sum.value,5);assert.equal(s.results.sum.status,'fulfilled');
});
test('bounded parallelism and ready input order',async()=>{
 let active=0,peak=0,order=[];
 const s=await runGraph(['a','b','c','d'].map(id=>({id,run:async()=>{order.push(id);peak=Math.max(peak,++active);await wait(15);--active;return id;}})),{concurrency:2});
 assert.equal(peak,2);assert.deepEqual(order,['a','b','c','d']);assert.equal(Object.keys(s.results).length,4);
});
test('validate complete graph before side effects',async()=>{
 for(const kind of ['duplicate','unknown','cycle','repeated','badfn','badid']){
  let starts=0;const run=()=>++starts;let tasks=[{id:'ok',run}];
  if(kind==='duplicate')tasks.push({id:'ok',run});
  if(kind==='unknown')tasks.push({id:'bad',deps:['missing'],run});
  if(kind==='cycle')tasks.push({id:'x',deps:['y'],run},{id:'y',deps:['x'],run});
  if(kind==='repeated')tasks.push({id:'x',deps:['ok','ok'],run});
  if(kind==='badfn')tasks.push({id:'x',run:1});
  if(kind==='badid')tasks.push({id:'',run});
  await assert.rejects(()=>runGraph(tasks));assert.equal(starts,0,kind);
 }
 for(const concurrency of [0,-1,1.5,NaN])await assert.rejects(()=>runGraph([],{concurrency}));
});
test('branch failure skips descendants but independent work continues',async()=>{
 const calls=[];
 const s=await runGraph([{id:'bad',run:()=>{throw Error('broken');}},{id:'child',deps:['bad'],run:()=>calls.push('child')},{id:'grandchild',deps:['child'],run:()=>calls.push('grandchild')},{id:'good',run:()=>7}]);
 assert.equal(s.results.bad.error,'broken');assert.equal(s.results.bad.status,'rejected');
 for(const id of ['child','grandchild'])assert.deepEqual(s.results[id],{status:'skipped',reason:'dependency'});
 assert.equal(s.results.good.value,7);assert.deepEqual(calls,[]);
});
test('async failure and non-Error rejection become string errors',async()=>{
 const s=await runGraph([{id:'a',run:async()=>{throw Error('async');}},{id:'b',run:()=>{throw 17;}}]);
 assert.equal(s.results.a.error,'async');assert.equal(s.results.b.error,'17');
});
test('events match starts and terminal outcomes exactly',async()=>{
 const events=[];
 await runGraph([{id:'a',run:()=>1},{id:'b',deps:['a'],run:()=>{throw Error('x');}},{id:'c',deps:['b'],run:()=>3}],{onEvent:e=>events.push(e)});
 for(const id of ['a','b']){
  assert.equal(events.filter(e=>e.id===id&&e.type==='start').length,1);
  assert.equal(events.filter(e=>e.id===id&&e.type==='finish').length,1);
 }
 assert.equal(events.find(e=>e.id==='a'&&e.type==='finish').status,'fulfilled');
 assert.equal(events.find(e=>e.id==='b'&&e.type==='finish').status,'rejected');
 assert.equal(events.filter(e=>e.id==='c'&&e.type==='skip').length,1);
 assert.equal(events.filter(e=>e.id==='c'&&e.type==='start').length,0);
});
test('pre-abort and mid-run abort do not start further work',async()=>{
 const pre=new AbortController();pre.abort();let n=0;
 const a=await runGraph([{id:'x',run:()=>++n}],{signal:pre.signal});assert.equal(n,0);assert.equal(a.results.x.reason,'aborted');
 const ctrl=new AbortController();let finished=false;
 const b=await runGraph([{id:'a',run:async({signal})=>{assert.equal(signal,ctrl.signal);ctrl.abort();await wait(10);finished=true;return 8;}},{id:'b',run:()=>++n},{id:'c',deps:['a'],run:()=>++n}],{concurrency:1,signal:ctrl.signal});
 assert.equal(finished,true);assert.equal(b.results.a.value,8);assert.equal(n,0);
 for(const id of ['b','c'])assert.equal(b.results[id].reason,'aborted');
});
function cli(plan,args=[]){const dir=mkdtempSync(join(tmpdir(),'graph-check-'));try{const path=join(dir,'plan.json');writeFileSync(path,JSON.stringify(plan));return spawnSync(process.execPath,['bin/run.mjs',path,...args],{encoding:'utf8',timeout:4000});}finally{rmSync(dir,{recursive:true,force:true});}}
test('CLI executes value delay and fan-in sum',()=>{
 const r=cli({tasks:[{id:'a',kind:'value',value:4},{id:'b',kind:'delay',ms:1,value:5},{id:'c',kind:'sum',deps:['a','b']}]},['--concurrency','2']);
 assert.equal(r.status,0,r.stderr);assert.equal(JSON.parse(r.stdout).results.c.value,9);
});
test('CLI distinguishes task failures from invalid plans',()=>{
 const a=cli({tasks:[{id:'a',kind:'fail',message:'no'},{id:'b',kind:'value',deps:['a'],value:1}]});
 assert.equal(a.status,1,a.stderr);assert.equal(JSON.parse(a.stdout).results.b.status,'skipped');
 for(const plan of [{tasks:[{id:'a',kind:'unknown'}]},{tasks:[{id:'a',kind:'delay',ms:-1}]},{tasks:[{id:'a',kind:'value',deps:['missing']}]}]){
  const r=cli(plan);assert.equal(r.status,2,r.stderr);assert.ok(JSON.parse(r.stderr).error);assert.equal(r.stdout.trim(),'');
 }
 const c=cli({tasks:[]},['--concurrency','0']);assert.equal(c.status,2);assert.ok(JSON.parse(c.stderr).error);
});
'''
r=subprocess.run([NODE,'--input-type=module','-e',CODE],text=True,timeout=90)
raise SystemExit(r.returncode)
