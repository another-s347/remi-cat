#!/usr/bin/env python3
"""Re-evaluate finished artifacts without giving acceptance tests to the agents.

Version 2 removes an underspecified sleep(0) expectation: the original prompt
does not say that a zero-delay callback must run. Positive delay is tested instead.
Both original verdicts and acceptance output remain available for audit.
"""
import argparse
import json
from pathlib import Path
import subprocess
import sys
from tasks import CHECKS


def main():
    p=argparse.ArgumentParser(description=__doc__);p.add_argument('roots',nargs='+',type=Path);p.add_argument('--suite',choices=['short','extended'],default='short');args=p.parse_args()
    checks=CHECKS
    if args.suite=='extended':
        from extended_tasks import CHECKS as checks
    version='extended-v1' if args.suite=='extended' else 2
    for root in args.roots:
        for path in root.glob('*/result.json'):
            result=json.loads(path.read_text())
            if result.get('acceptance_version')==version: continue
            original=path.with_name('result.original.json')
            if not original.exists(): original.write_text(path.read_text())
            check=subprocess.run([sys.executable,'-c',checks[result['task']]],cwd=path.parent/'workspace',capture_output=True,text=True,timeout=180 if args.suite=='extended' else 30)
            (path.parent/'acceptance-v2.txt').write_text(check.stdout+check.stderr)
            result.update(passed=check.returncode==0,acceptance_version=version)
            path.write_text(json.dumps(result,indent=2))
            print(result['id'],result['passed'])


if __name__=='__main__': main()
