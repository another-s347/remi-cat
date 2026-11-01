"""Independent transactional/CLI acceptance using fresh external databases."""
import unittest,tempfile,sqlite3,json,subprocess,sys,threading
from concurrent.futures import ThreadPoolExecutor
from pathlib import Path
from stockroom import open_db,add_stock,get_stock,reserve,commit,cancel
class Check(unittest.TestCase):
 def setUp(self):
  self.tmp=tempfile.TemporaryDirectory();self.path=Path(self.tmp.name)/'stock.db';self.c=open_db(self.path)
 def tearDown(self):self.c.close();self.tmp.cleanup()
 def test_schema(self):
  self.assertEqual(self.c.execute('PRAGMA user_version').fetchone()[0],2)
  self.assertEqual(self.c.execute('PRAGMA foreign_keys').fetchone()[0],1)
  columns={r[1] for r in self.c.execute('PRAGMA table_info(reservations)')};self.assertTrue({'token','sku','quantity','status'}<=columns)
 def test_v1_migration(self):
  p=Path(self.tmp.name)/'v1.db';c=sqlite3.connect(p);c.executescript("CREATE TABLE items(sku TEXT PRIMARY KEY,quantity INTEGER NOT NULL);INSERT INTO items VALUES('old',9);PRAGMA user_version=1;");c.close()
  c=open_db(p)
  try:self.assertEqual(get_stock(c,'old')['available'],9);self.assertEqual(c.execute('PRAGMA user_version').fetchone()[0],2);reserve(c,'old-hold','old',2)
  finally:c.close()
 def test_future_version_unchanged(self):
  p=Path(self.tmp.name)/'future.db';c=sqlite3.connect(p);c.executescript('CREATE TABLE future_data(x);INSERT INTO future_data VALUES(7);PRAGMA user_version=9;');c.close()
  with self.assertRaises(ValueError):open_db(p)
  c=sqlite3.connect(p)
  try:self.assertEqual(c.execute('PRAGMA user_version').fetchone()[0],9);self.assertEqual(c.execute('SELECT x FROM future_data').fetchone()[0],7)
  finally:c.close()
 def test_lifecycle_and_persistence(self):
  add_stock(self.c,'book',10);r=reserve(self.c,'t1','book',4)
  self.assertEqual(r,dict(token='t1',sku='book',quantity=4,status='held'))
  self.assertEqual(get_stock(self.c,'book'),dict(sku='book',on_hand=10,held=4,available=6))
  self.assertEqual(commit(self.c,'t1')['status'],'committed');commit(self.c,'t1')
  self.assertEqual(get_stock(self.c,'book'),dict(sku='book',on_hand=6,held=0,available=6))
  c=open_db(self.path)
  try:self.assertEqual(reserve(c,'t1','book',4)['status'],'committed');self.assertEqual(get_stock(c,'book')['on_hand'],6)
  finally:c.close()
 def test_idempotent_hold_and_cancel(self):
  add_stock(self.c,'x',5);reserve(self.c,'t','x',3);reserve(self.c,'t','x',3)
  self.assertEqual(get_stock(self.c,'x')['held'],3);cancel(self.c,'t');cancel(self.c,'t')
  self.assertEqual(get_stock(self.c,'x')['available'],5);self.assertEqual(reserve(self.c,'t','x',3)['status'],'cancelled')
 def test_failures_leave_state_unchanged(self):
  add_stock(self.c,'x',5);reserve(self.c,'t','x',4)
  for operation in [lambda:reserve(self.c,'new','x',2),lambda:reserve(self.c,'t','x',1),lambda:reserve(self.c,'u','missing',1),lambda:commit(self.c,'missing'),lambda:cancel(self.c,'missing')]:
   with self.assertRaises(ValueError):operation()
   self.assertEqual(get_stock(self.c,'x')['available'],1)
  cancel(self.c,'t')
  with self.assertRaises(ValueError):commit(self.c,'t')
  reserve(self.c,'z','x',1);commit(self.c,'z')
  with self.assertRaises(ValueError):cancel(self.c,'z')
 def test_input_validation(self):
  for q in [0,-1,True,False,1.5,'2']:
   with self.subTest(q=q):
    with self.assertRaises(ValueError):add_stock(self.c,'x',q)
  add_stock(self.c,'x',5)
  for q in [0,-1,True,1.5]:
   with self.assertRaises(ValueError):reserve(self.c,'t','x',q)
  for value in ['', '   ']:
   with self.assertRaises(ValueError):add_stock(self.c,value,1)
   with self.assertRaises(ValueError):reserve(self.c,value,'x',1)
 def test_multiple_holds_and_stock_addition(self):
  add_stock(self.c,'x',7);reserve(self.c,'a','x',2);reserve(self.c,'b','x',3);cancel(self.c,'a');add_stock(self.c,'x',4)
  self.assertEqual(get_stock(self.c,'x'),dict(sku='x',on_hand=11,held=3,available=8));commit(self.c,'b')
  self.assertEqual(get_stock(self.c,'x')['on_hand'],8)
 def test_concurrent_reservations_do_not_overbook(self):
  add_stock(self.c,'x',5);barrier=threading.Barrier(2)
  def attempt(token):
   c=open_db(self.path)
   try:
    barrier.wait(timeout=4)
    try:reserve(c,token,'x',4);return 'ok'
    except ValueError:return 'insufficient'
   finally:c.close()
  with ThreadPoolExecutor(max_workers=2) as pool:results=list(pool.map(attempt,['a','b']))
  self.assertEqual(sorted(results),['insufficient','ok']);self.assertEqual(get_stock(self.c,'x')['held'],4)
 def cli(self,*args):return subprocess.run([sys.executable,'-m','stockroom','--db',str(self.path),*args],capture_output=True,text=True,timeout=5)
 def test_cli_lifecycle_and_json_errors(self):
  for args,status in [(('add','cli','8'),None),(('reserve','token','cli','3'),'held'),(('commit','token'),'committed'),(('commit','token'),'committed')]:
   r=self.cli(*args);self.assertEqual(r.returncode,0,r.stderr);body=json.loads(r.stdout)
   if status:self.assertEqual(body['status'],status)
  self.assertEqual(json.loads(self.cli('show','cli').stdout)['on_hand'],5)
  for args in [('reserve','too','cli','9'),('show','absent'),('add','cli','0'),('cancel','token')]:
   r=self.cli(*args);self.assertNotEqual(r.returncode,0);self.assertTrue(json.loads(r.stderr)['error']);self.assertEqual(r.stdout.strip(),'')
unittest.main()
