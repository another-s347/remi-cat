"""Deterministic workspaces and independent acceptance checks for harness comparison."""
import json
from pathlib import Path

TASKS = {
    "csv": "Fix parse_rows in project/csvio.py to parse standard CSV correctly, including quoted commas, escaped quotes, multiline fields, CRLF, empty input and empty fields. Preserve its list-of-lists API. Add regression tests and run them. Do not change unrelated code.",
    "retry": "Implement retry_call(fn, attempts=3, retry_on=(OSError,), delay=0, sleep=time.sleep) in project/retry.py and export it from project/__init__.py. Return the callable result; attempts includes the first call, must be a positive integer (bool is invalid); retry only retry_on, re-raise the last failure, and sleep(delay) only between retries. Add regression tests and run them. Do not change unrelated code.",
    "logs": "Find the actual error behind job invoice-731 in data/events.jsonl. Write diagnosis.json with job, root_cause_code, failed_stage and retryable (boolean). Cross-check data/error_codes.json. Do not alter the input data. Explain the evidence briefly.",
}


def setup(root: Path):
    (root / "project").mkdir(parents=True)
    (root / "project/__init__.py").write_text('from .csvio import parse_rows\n')
    (root / "project/csvio.py").write_text('def parse_rows(text):\n    return [line.split(",") for line in text.splitlines()]\n')
    (root / "project/retry.py").write_text('import time\n\n# Retry utility is not yet implemented.\n')
    (root / "README.md").write_text('Small Python utility project. Python 3 standard library only. Tests may be run with python3 -m unittest discover.\n')
    (root / "data").mkdir()
    events = [dict(job=f"invoice-{i % 900}", stage="poll", level="info", message="heartbeat: still running", sequence=i) for i in range(3600)]
    events[2847] = dict(job="invoice-731", stage="upload", level="error", code="E_AUTH_EXPIRED", retryable=False, sequence=2847)
    events[2848] = dict(job="invoice-731", stage="finalize", level="error", code="E_UPSTREAM", caused_by=2847, sequence=2848)
    (root / "data/events.jsonl").write_text(''.join(json.dumps(x)+'\n' for x in events))
    (root / "data/error_codes.json").write_text(json.dumps({"E_AUTH_EXPIRED": {"meaning": "Credential expired; refresh before retry", "retryable": False}, "E_UPSTREAM": {"meaning": "Downstream propagation; inspect caused_by", "retryable": True}}))


CHECKS = {
    "csv": '''import unittest
from project.csvio import parse_rows
class Check(unittest.TestCase):
 def test_quotes(self): self.assertEqual(parse_rows('a,"b,c","d""e"\\r\\n'), [['a','b,c','d"e']])
 def test_multiline(self): self.assertEqual(parse_rows('"a\\nb",c\\n'), [['a\\nb','c']])
 def test_empty(self): self.assertEqual(parse_rows(''), [])
 def test_fields(self): self.assertEqual(parse_rows(',a,\\n'), [['','a','']])
 def test_plain(self): self.assertEqual(parse_rows('a,b\\nc,d'), [['a','b'],['c','d']])
unittest.main()
''',
    "retry": '''import unittest
from project import retry_call
class Check(unittest.TestCase):
 def test_success(self): self.assertEqual(retry_call(lambda: 7),7)
 def test_retry(self):
  calls=[]; sleeps=[]
  def f():
   calls.append(1)
   if len(calls)<3: raise OSError('transient')
   return 9
  self.assertEqual(retry_call(f,delay=0.2,sleep=sleeps.append),9)
  self.assertEqual((len(calls),sleeps),(3,[0.2,0.2]))
 def test_exhausted(self):
  calls=[]; sleeps=[]; err=OSError('last')
  def f(): calls.append(1); raise err
  with self.assertRaises(OSError) as cm: retry_call(f,attempts=2,delay=0.2,sleep=sleeps.append)
  self.assertIs(cm.exception,err); self.assertEqual(len(calls),2); self.assertEqual(sleeps,[0.2])
 def test_no_retry(self):
  calls=[]
  def f(): calls.append(1); raise ValueError('no')
  with self.assertRaises(ValueError): retry_call(f)
  self.assertEqual(calls,[1])
 def test_invalid(self):
  for attempts in [0,-1,1.5,True]:
   with self.subTest(attempts=attempts):
    with self.assertRaises((TypeError,ValueError)): retry_call(lambda: 1,attempts=attempts)
 def test_custom(self):
  calls=[]
  def f():
   calls.append(1)
   if len(calls)==1: raise ValueError('retry')
   return 'ok'
  self.assertEqual(retry_call(f,retry_on=(ValueError,)),'ok')
unittest.main()
''',
    "logs": '''import json,unittest
class Check(unittest.TestCase):
 def test_diagnosis(self):
  with open('diagnosis.json') as f: d=json.load(f)
  for k,v in dict(job='invoice-731',root_cause_code='E_AUTH_EXPIRED',failed_stage='upload',retryable=False).items():
   self.assertEqual(d[k],v)
unittest.main()
''',
}
