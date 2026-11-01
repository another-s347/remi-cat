"""Independent CLI/filesystem tests plus an external Rust public-API probe."""
import unittest,tempfile,subprocess,os
from pathlib import Path
ROOT=Path.cwd()
build=subprocess.run(['cargo','build','--offline'],capture_output=True,text=True,timeout=90)
if build.returncode:
 print(build.stdout,build.stderr);raise SystemExit(1)
BIN=ROOT/'target/debug/trail-index'
def esc(s):return s.replace('\\','\\\\').replace('\t','\\t').replace('\r','\\r').replace('\n','\\n')
def row(path,line,text):return f'{esc(path)}\t{line}\t{esc(text)}\n'
class Check(unittest.TestCase):
 def setUp(self):
  self.tmp=tempfile.TemporaryDirectory();self.base=Path(self.tmp.name);self.docs=self.base/'docs';self.docs.mkdir();self.index=self.base/'cache/index.idx'
 def tearDown(self):self.tmp.cleanup()
 def cli(self,*args):return subprocess.run([str(BIN),*map(str,args)],capture_output=True,text=True,timeout=5)
 def put(self,path,text):p=self.docs/path;p.parent.mkdir(parents=True,exist_ok=True);p.write_text(text);return p
 def built(self):
  r=self.cli('build',self.docs,self.index);self.assertEqual(r.returncode,0,r.stderr);return r.stdout
 def test_build_query_unicode_and_line_numbers(self):
  self.put('b.md','hello\n世界 x.y\n');self.put('a.txt','x.y\nxay\n');self.put('empty.txt','')
  self.assertEqual(self.built(),'files=3 lines=4\n')
  self.assertEqual(self.cli('query',self.index,'x.y').stdout,row('a.txt',1,'x.y')+row('b.md',2,'世界 x.y'))
  self.assertEqual(self.cli('query',self.index,'世界').stdout,row('b.md',2,'世界 x.y'))
  self.assertEqual(self.cli('query',self.index,'HELLO').stdout,'')
 def test_escaping_and_crlf(self):
  self.put('dir/tab\tname.txt','one\tback\\slash\r\ntwo\r\n')
  self.assertEqual(self.built(),'files=1 lines=2\n')
  expected='TRAIL1\n'+row('dir/tab\tname.txt',1,'one\tback\\slash')+row('dir/tab\tname.txt',2,'two')
  self.assertEqual(self.index.read_bytes(),expected.encode())
  self.assertEqual(self.cli('query',self.index,'back\\').stdout,row('dir/tab\tname.txt',1,'one\tback\\slash'))
 def test_ignore_rules_and_symlinks(self):
  self.put('ok.txt','yes');self.put('skip.py','yes');self.put('.secret.txt','yes')
  for directory in ['.hidden','target','node_modules']:self.put(directory+'/hidden.txt','yes')
  outside=self.base/'outside.txt';outside.write_text('yes');(self.docs/'link.txt').symlink_to(outside)
  (self.docs/'linked-dir').symlink_to(self.base,target_is_directory=True)
  self.assertEqual(self.built(),'files=1 lines=1\n')
  self.assertEqual(self.cli('query',self.index,'yes').stdout,row('ok.txt',1,'yes'))
 def test_refresh_determinism_and_removal(self):
  a=self.put('a.txt','old\n');self.put('z.md','other\n');self.built();first=self.index.read_bytes();self.built();self.assertEqual(first,self.index.read_bytes())
  a.unlink();self.put('b.txt','new\n');self.built();self.assertEqual(self.cli('query',self.index,'old').stdout,'')
  self.assertEqual(self.cli('query',self.index,'new').stdout,row('b.txt',1,'new'))
 def test_failed_utf8_build_preserves_snapshot(self):
  self.put('valid.txt','before');self.built();before=self.index.read_bytes();(self.docs/'bad.txt').write_bytes(b'\xff\xfe')
  r=self.cli('build',self.docs,self.index);self.assertNotEqual(r.returncode,0);self.assertTrue(r.stderr.strip());self.assertEqual(r.stdout.strip(),'');self.assertEqual(self.index.read_bytes(),before)
 def test_empty_corpus(self):
  self.assertEqual(self.built(),'files=0 lines=0\n');self.assertEqual(self.index.read_bytes(),b'TRAIL1\n');self.assertEqual(self.cli('query',self.index,'x').stdout,'')
 def test_missing_paths_empty_query_and_invalid_arguments(self):
  self.put('a.txt','a');self.built()
  for args in [('query',self.index,''),('query',self.base/'missing','a'),('build',self.base/'missing',self.index),('nonsense',),('build',)]:
   r=self.cli(*args);self.assertNotEqual(r.returncode,0);self.assertTrue(r.stderr.strip());self.assertEqual(r.stdout.strip(),'')
 def test_corrupt_storage_is_rejected(self):
  self.index.parent.mkdir(parents=True)
  for content in ['WRONG\na.txt\t1\tx\n','TRAIL1\na.txt\t0\tx\n','TRAIL1\na.txt\tbad\tx\n','TRAIL1\na.txt\t1\tx\\q\n','TRAIL1\na.txt\t1\tx\\\n','TRAIL1\nmissing-tabs\n']:
   self.index.write_text(content);r=self.cli('query',self.index,'x');self.assertNotEqual(r.returncode,0,repr(content));self.assertTrue(r.stderr.strip())
 def test_unsafe_paths_are_rejected(self):
  self.index.parent.mkdir(parents=True)
  for path in ['','/absolute.txt','../outside.txt','dir/../bad.txt','./a.txt','a//b.txt']:
   self.index.write_text('TRAIL1\n'+row(path,1,'x'));r=self.cli('query',self.index,'x');self.assertNotEqual(r.returncode,0,path)
 def test_unsorted_valid_storage_queries_are_sorted(self):
  self.index.parent.mkdir(parents=True);self.index.write_text('TRAIL1\n'+row('z.txt',2,'hit')+row('a.txt',4,'hit')+row('a.txt',1,'hit'))
  r=self.cli('query',self.index,'hit');self.assertEqual(r.returncode,0,r.stderr);self.assertEqual(r.stdout,row('a.txt',1,'hit')+row('a.txt',4,'hit')+row('z.txt',2,'hit'))
 def test_public_rust_api(self):
  self.put('a.txt','needle\nother\n')
  source=self.base/'probe.rs';source.write_text('''extern crate trail_index;
use std::path::Path;
fn main(){let a:Vec<String>=std::env::args().collect();let r:trail_index::BuildReport=trail_index::build(Path::new(&a[1]),Path::new(&a[2])).unwrap();let hits:Vec<trail_index::Hit>=trail_index::query(Path::new(&a[2]),"needle").unwrap();assert_eq!(hits[0].path,"a.txt");assert_eq!(hits[0].line,1);assert_eq!(hits[0].text,"needle");println!("{} {} {}",r.files,r.lines,hits.len());}
''')
  probe=self.base/'probe';c=subprocess.run(['rustc',str(source),'--extern',f'trail_index={ROOT}/target/debug/libtrail_index.rlib','-L',str(ROOT/'target/debug/deps'),'-o',str(probe)],capture_output=True,text=True,timeout=30)
  self.assertEqual(c.returncode,0,c.stderr);r=subprocess.run([str(probe),str(self.docs),str(self.index)],capture_output=True,text=True,timeout=5);self.assertEqual(r.returncode,0,r.stderr);self.assertEqual(r.stdout,'1 2 1\n')
unittest.main()
