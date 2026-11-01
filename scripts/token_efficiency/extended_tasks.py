"""Extended, cross-file workloads. Specifications and external checks freeze before runs."""
from pathlib import Path
from textwrap import dedent

TASKS = {
    'scheduler': 'Complete the asynchronous task-graph package in this workspace. Implement every behavior in SPEC.md across the library and JSON CLI, preserve the public API and existing tests, add regression tests and run them. Use only Node.js built-ins; do not install dependencies. Finish the implementation and CLI integration, not just a design.',
    'stockroom': 'Complete this SQLite stock-reservation package according to SPEC.md: migration, transactional inventory/reservation lifecycle, concurrent safety, and persistent JSON CLI. Preserve existing APIs and data. Add regression tests and run them. Python standard library only; no dependency installation. Finish all library and CLI requirements.',
    'trailindex': 'Complete the Rust persistent text-index project according to SPEC.md, including filesystem scanning, escaped storage, atomic rebuild, query and CLI. Preserve the public API and existing tests. Add regression tests and run cargo test --offline. Use only the Rust standard library; do not add dependencies. Finish all integration and failure-handling requirements.',
}

SPECS = {
'scheduler': '''# Task graph scheduler contract
Runtime: Node.js 24, ES modules, built-ins only. Run tests with node --test.
Public entry: src/index.mjs exports async runGraph(tasks, options={}).
A task is {id, deps=[], run}; id is a nonempty string, deps are unique task IDs,
and run is a function. Reject duplicate IDs, unknown/repeated dependencies,
cycles, malformed tasks or non-positive/non-integer concurrency before invoking
any task. Default concurrency is 2. Empty graph returns {results:{}}.

runGraph returns {results}, keyed by every ID. Each record is either
{status:'fulfilled', value}, {status:'rejected', error:MESSAGE_STRING}, or
{status:'skipped', reason:'dependency'|'aborted'}.
Invoke run({signal, results}) where results maps that task's direct dependency IDs
to their fulfilled values. Ready tasks start in input order, up to concurrency.
A task waits for all dependencies. One failed task must not stop independent
branches. Every dependent descendant of a rejected/skipped task is skipped
without invocation. Catch synchronous throws and asynchronous rejections.

options.signal is an AbortSignal: if already aborted, no task starts; after
abort, start no further tasks, await running tasks and preserve their outcomes,
and mark all never-started tasks skipped/aborted (abort reason has priority).
options.onEvent receives {type,id,status?}: start before invocation; finish once
for each invoked task, with status fulfilled/rejected; skip once for each skipped
task. No duplicate starts/terminal events. Tests use nonthrowing callbacks.

CLI: node bin/run.mjs PLAN.json [--concurrency N]. Plan is {tasks:[...]}; each
serialized task has id, optional deps, and kind: value (return value), delay
(nonnegative integer ms then return value), fail (throw message, default 'failure'),
or sum (sum numeric direct dependency values, rejecting nonnumeric values).
Validate unknown kind and malformed delay before executing the graph. Print one
JSON summary on stdout. Exit 0 iff every task fulfilled, 1 for a valid graph with
rejections/skips, 2 for invalid CLI/plan/graph; invalid cases print JSON {error:...}
on stderr. CLI errors must not print a successful summary or an uncaught stack.
Maintain the module boundaries where practical; add tests for scheduling,
validation-before-side-effects, failure propagation, cancellation and CLI.
''',
'stockroom': '''# Stockroom contract
Python 3.10+, standard library only. Run python3 -m unittest discover -v.
stockroom.db.open_db(path) returns sqlite3.Connection, with foreign keys enabled.
Fresh/empty database becomes schema version 2. Version 1 has items(sku TEXT
PRIMARY KEY, quantity INTEGER NOT NULL); migrate it without losing rows.
Version 2 keeps items and adds reservations(token TEXT PRIMARY KEY, sku TEXT NOT
NULL REFERENCES items(sku), quantity INTEGER NOT NULL, status TEXT NOT NULL).
Statuses are held, committed, cancelled. Set PRAGMA user_version=2 only on success.
Reject future versions with ValueError, leaving their data/version unchanged.

stockroom.service exports add_stock, get_stock, reserve, commit, cancel.
add_stock(conn,sku,quantity) adds physical stock, creating SKU if needed.
get_stock(conn,sku) -> {sku,on_hand,held,available}; on_hand is items.quantity,
held is sum of held reservations, available=on_hand-held. Unknown SKU raises
ValueError. Quantities must be positive ints, never bool; blank SKU/token invalid.
reserve(conn,token,sku,quantity) returns {token,sku,quantity,status}; sufficient
available stock is required, and reserving does not reduce physical stock.
Repeated token with identical SKU/quantity returns the existing record, including
its current status, without another hold. Reuse with different arguments fails.
commit(conn,token): held -> committed and physical stock decreases once; repeated
commit is idempotent. cancel(conn,token): held -> cancelled, releases hold;
repeated cancel is idempotent. Cannot commit cancelled or cancel committed.
Unknown tokens and all invalid operations raise ValueError and change nothing.
Reservation operations must be atomic and persist, leave no partial updates,
and not overbook when independent database connections reserve concurrently.
Two simultaneous reservations of 4 out of 5 units must yield exactly one success;
failures for insufficient stock must be ValueError, not leaked lock errors.

CLI: python3 -m stockroom --db PATH COMMAND ...
add SKU QTY prints get_stock; reserve TOKEN SKU QTY prints reservation;
commit TOKEN and cancel TOKEN print reservation; show SKU prints get_stock.
One JSON object on stdout for success, exit 0. Failures exit nonzero and print
JSON {error:MESSAGE} on stderr, with no traceback/success object. Invocations use
the same persistent database. Do not modify SQLite file by deleting/recreating it.
Extend the existing modules/tests; test migration, lifecycle, rollback,
idempotency, concurrency and CLI round-trips.
''',
 'trailindex': '''# Trail index contract
Rust stable, standard library only; cargo test --offline. Package/binary trail-index.
Public library exports Hit {path:String,line:usize,text:String}, BuildReport
{files:usize,lines:usize}, build(root:&Path,index:&Path)->Result<BuildReport,String>,
query(index:&Path,term:&str)->Result<Vec<Hit>,String>.

build recursively scans regular .txt and .md files (case-sensitive extensions).
Ignore symbolic links and dot-prefixed entries, plus target and node_modules
directories. Paths are relative to root with '/' separators. Root must be an
existing directory. Read UTF-8 strictly; any read/UTF-8 error fails the build.
Use text.lines() semantics (one-based line numbers, CRLF stripped, no extra empty
line for final newline). Empty files count as files but have zero lines.
Every rebuild reflects current files: removed/renamed files leave no stale hits.
Deterministic ordering is path ascending, then line ascending.

Storage is exactly UTF-8 header TRAIL1 followed by newline, then one row per line:
escaped_path TAB decimal_line TAB escaped_text NEWLINE. Escape backslash as \\\\,
tab as \\t, CR as \\r, LF as \\n. Reject unknown/dangling escapes, malformed rows,
zero/noninteger line numbers, invalid header or unsafe paths (absolute, empty,
'.'/'..' components) when reading an index. Empty file corpus stores only header.
Serialize deterministically. Write a complete replacement then atomically replace
the index; failed builds leave a previous valid index byte-for-byte unchanged.
Create a missing index parent directory. Do not write partial index contents.

query loads/validates index and returns all rows whose text contains term as a
case-sensitive literal substring, including Unicode/punctuation; empty term is
an error. Sort results by path then line. Missing/corrupt index returns Err.
CLI: trail-index build ROOT INDEX prints 'files=N lines=N' plus newline.
trail-index query INDEX TERM prints matching rows in the same escaped TSV format
(without header). Errors/invalid arguments exit nonzero, explain on stderr and
do not emit a success result. No network/dependencies. Exercise storage escaping,
Unicode, deterministic refresh, corrupt input, failed-build preservation and CLI.
''',
}

def put(root, files):
    for name, content in files.items():
        path = root / name
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_text(dedent(content).lstrip())


def setup(root: Path, task: str):
    root.mkdir(parents=True, exist_ok=True)
    put(root, {'SPEC.md': SPECS[task], 'README.md': 'Implement SPEC.md. All runtime dependencies are already installed; no network is needed.\n'})
    if task == 'scheduler':
        put(root, {
        'package.json': '{"name":"task-graph","type":"module","scripts":{"test":"node --test"},"private":true}\n',
        'src/index.mjs': "export { runGraph } from './runner.mjs';\n",
        'src/graph.mjs': 'export function validateGraph(tasks) { return tasks; }\n',
        'src/runner.mjs': '''import {validateGraph} from './graph.mjs';
export async function runGraph(tasks, options={}) {
  validateGraph(tasks); const results={};
  for (const task of tasks) results[task.id]={status:'fulfilled',value:await task.run({results})};
  return {results};
}
''',
        'src/handlers.mjs': '''export function fromPlan(plan) {
 return plan.tasks.map(t=>({...t,run:async()=>t.value}));
}
''',
        'bin/run.mjs': '''import fs from 'node:fs/promises';
import {runGraph} from '../src/index.mjs';
import {fromPlan} from '../src/handlers.mjs';
const plan=JSON.parse(await fs.readFile(process.argv[2],'utf8'));
console.log(JSON.stringify(await runGraph(fromPlan(plan))));
''',
        'test/basic.test.mjs': '''import test from 'node:test';
import assert from 'node:assert/strict';
import {runGraph} from '../src/index.mjs';
test('single value',async()=>assert.equal((await runGraph([{id:'one',run:async()=>7}])).results.one.value,7));
''',
        })
    elif task == 'stockroom':
        put(root, {
        'stockroom/__init__.py': 'from .db import open_db\nfrom .service import add_stock, get_stock, reserve, commit, cancel\n',
        'stockroom/db.py': '''import sqlite3
def open_db(path):
    conn=sqlite3.connect(path)
    conn.execute('CREATE TABLE IF NOT EXISTS items(sku TEXT PRIMARY KEY, quantity INTEGER NOT NULL)')
    conn.execute('PRAGMA user_version=1')
    conn.commit()
    return conn
''',
        'stockroom/service.py': '''def add_stock(conn,sku,quantity):
    conn.execute('INSERT INTO items VALUES(?,?) ON CONFLICT(sku) DO UPDATE SET quantity=quantity+excluded.quantity',(sku,quantity))
    conn.commit()
def get_stock(conn,sku):
    row=conn.execute('SELECT quantity FROM items WHERE sku=?',(sku,)).fetchone()
    if row is None: raise ValueError('unknown sku')
    return dict(sku=sku,on_hand=row[0],held=0,available=row[0])
def reserve(conn,token,sku,quantity):
    raise NotImplementedError('reservations')
def commit(conn,token):
    raise NotImplementedError('commit')
def cancel(conn,token):
    raise NotImplementedError('cancel')
''',
        'stockroom/__main__.py': '''import argparse,json
from . import open_db,get_stock,add_stock
p=argparse.ArgumentParser();p.add_argument('--db',required=True);p.add_argument('command');p.add_argument('args',nargs='*');a=p.parse_args()
c=open_db(a.db)
if a.command=='add': add_stock(c,a.args[0],int(a.args[1]))
print(json.dumps(get_stock(c,a.args[0])))
''',
        'tests/__init__.py': '',
        'tests/test_basic.py': '''import unittest
from stockroom import open_db,add_stock,get_stock
class Basic(unittest.TestCase):
    def test_stock(self):
        c=open_db(':memory:');add_stock(c,'book',3)
        self.assertEqual(get_stock(c,'book')['on_hand'],3);c.close()
''',
        })
    elif task == 'trailindex':
        put(root, {
        'Cargo.toml': '[package]\nname="trail-index"\nversion="0.1.0"\nedition="2021"\n',
        'src/lib.rs': '''pub mod codec;
pub mod scan;
pub mod store;
pub mod query;
#[derive(Debug,Clone,PartialEq,Eq)]
pub struct Hit { pub path:String, pub line:usize, pub text:String }
#[derive(Debug,Clone,PartialEq,Eq)]
pub struct BuildReport { pub files:usize, pub lines:usize }
pub use scan::build;
pub use query::query;
''',
        'src/codec.rs': '''pub fn escape(text:&str)->String { text.to_string() }
pub fn unescape(text:&str)->Result<String,String> { Ok(text.to_string()) }
''',
        'src/store.rs': '''use crate::Hit;
use std::path::Path;
pub fn save(_index:&Path,_hits:&[Hit])->Result<(),String> { Err("storage not implemented".into()) }
pub fn load(_index:&Path)->Result<Vec<Hit>,String> { Err("storage not implemented".into()) }
''',
        'src/scan.rs': '''use crate::BuildReport;
use std::path::Path;
pub fn build(_root:&Path,_index:&Path)->Result<BuildReport,String> { Err("scan not implemented".into()) }
''',
        'src/query.rs': '''use crate::Hit;
use std::path::Path;
pub fn query(_index:&Path,_term:&str)->Result<Vec<Hit>,String> { Err("query not implemented".into()) }
''',
        'src/main.rs': 'fn main() { eprintln!("CLI not implemented"); std::process::exit(1); }\n',
        'tests/basic.rs': '#[test]\nfn public_hit() { let h=trail_index::Hit{path:"a.txt".into(),line:1,text:"hi".into()};assert_eq!(h.line,1); }\n',
        })
    else:
        raise ValueError(task)

CHECKS = {task: (Path(__file__).parent / 'extended_checks' / (task + '.py')).read_text() for task in TASKS}
