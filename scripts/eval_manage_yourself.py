#!/usr/bin/env python3
"""Run curated manage_yourself CLI scenarios in isolated temporary profiles.

The dataset records user requests and reference tool commands. This runner
checks that the reference commands still work as the CLI evolves. It does not
claim to measure an LLM's command-selection accuracy.
"""

from __future__ import annotations

import argparse
import json
import os
from pathlib import Path
import re
import shlex
import subprocess
import tempfile
import threading
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer


def run(binary: Path, command: str, env: dict[str, str], cwd: Path) -> subprocess.CompletedProcess[str]:
    argv = [str(binary), *shlex.split(command)]
    return subprocess.run(argv, cwd=cwd, env=env, text=True, capture_output=True, timeout=45)


class ModelApiHandler(BaseHTTPRequestHandler):
    def do_GET(self) -> None:
        if self.path != "/models":
            self.send_error(404)
            return
        self.send_response(200)
        self.send_header("Content-Type", "application/json")
        self.end_headers()
        self.wfile.write(b'{"data":[]}')

    def log_message(self, *_args: object) -> None:
        pass


def apply_file_edits(item: dict, values: dict[str, str]) -> None:
    for edit in item.get("file_edits", []):
        path = Path(edit["path"].format_map(values))
        if "copy_from" in edit:
            source = Path(edit["copy_from"].format_map(values))
            path.parent.mkdir(parents=True, exist_ok=True)
            path.write_text(source.read_text())
        source = path.read_text()
        for key, value in edit["set"].items():
            rendered = str(value).format_map(values)
            source, count = re.subn(rf"(?m)^{re.escape(key)}:.*$", f"{key}: {rendered}", source)
            if count != 1:
                raise ValueError(f"{path}: expected exactly one {key} field")
        path.write_text(source)


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--dataset", type=Path, default=Path(__file__).with_name("manage_yourself.jsonl"))
    parser.add_argument("--binary", type=Path, default=Path("target/debug/remi-cat"))
    parser.add_argument("--case", action="append", default=[], help="Run only this case ID; repeatable")
    parser.add_argument("--output", type=Path, help="Write per-case JSONL results")
    args = parser.parse_args()
    binary = args.binary.resolve()
    records = [json.loads(line) for line in args.dataset.read_text().splitlines() if line.strip()]
    if len({item["id"] for item in records}) != len(records):
        parser.error("duplicate case ID")
    selected = [item for item in records if not args.case or item["id"] in args.case]
    if not selected:
        parser.error("no matching cases")
    results = []
    for item in selected:
        if item.get("policy") == "do_not_execute":
            results.append({"id": item["id"], "status": "policy_only"})
            continue
        with tempfile.TemporaryDirectory(prefix="remi-self-eval-") as temp:
            root = Path(temp)
            env = os.environ.copy()
            for key in list(env):
                if key.endswith("_API_KEY") or key.endswith("_TOKEN"):
                    env.pop(key)
            env["OPENAI_API_KEY"] = "manage-self-eval-placeholder"
            env["HOME"] = str(root / "home")
            env["REMI_DATA_DIR"] = str(root / "data")
            env["REMI_PROFILE_REGISTRY_ROOT"] = str(root / "registry")
            env["RUST_LOG"] = "error"
            Path(env["HOME"]).mkdir()
            profile_dir = root / "profile"
            values = {"profile_dir": str(profile_dir), "profile": str(profile_dir / "profile.yaml"),
                      "model_file": str(profile_dir / "models" / "default.yaml"),
                      "custom_model_file": str(profile_dir / "models" / "eval-model.yaml"),
                      "runtime_file": str(profile_dir / "runtime.yaml")}
            server = None
            if item.get("mock_model_api"):
                server = ThreadingHTTPServer(("127.0.0.1", 0), ModelApiHandler)
                threading.Thread(target=server.serve_forever, daemon=True).start()
                values["mock_url"] = f"http://127.0.0.1:{server.server_port}"
            setup = item.get("setup", ["profile init {profile_dir} --id eval.profile --name Eval --with-runtime"])
            commands = setup + item["commands"] + item.get("verify", [])
            error = None
            for index, template in enumerate(commands):
                if index == len(setup):
                    try:
                        apply_file_edits(item, values)
                    except (OSError, ValueError) as exc:
                        error = f"file edit failed: {exc}"
                        break
                command = template.format_map(values)
                try:
                    process = run(binary, command, env, root)
                except subprocess.TimeoutExpired:
                    error = f"step {index + 1} timed out: {command}"
                    break
                expected = item.get("expect_failure", False) and index == len(commands) - 1
                if (process.returncode != 0) != expected:
                    error = f"step {index + 1} exit {process.returncode}: {command}: {(process.stdout + process.stderr)[-500:]}"
                    break
                for needle in item.get("contains", []) if index == len(commands) - 1 else []:
                    if needle not in process.stdout + process.stderr:
                        error = f"step {index + 1} missing {needle!r}: {command}"
                        break
                if error:
                    break
            if server:
                server.shutdown()
                server.server_close()
            if not error:
                for check in item.get("file_contains", []):
                    path = Path(check["path"].format_map(values))
                    if check["text"].format_map(values) not in path.read_text():
                        error = f"{path} missing {check['text']!r}"
                        break
            if not error:
                for check in item.get("file_not_contains", []):
                    path = Path(check["path"].format_map(values))
                    if check["text"].format_map(values) in path.read_text():
                        error = f"{path} unexpectedly contains {check['text']!r}"
                        break
            results.append({"id": item["id"], "status": "fail" if error else "pass", **({"error": error} if error else {})})
    if args.output:
        args.output.parent.mkdir(parents=True, exist_ok=True)
        args.output.write_text("".join(json.dumps(item, ensure_ascii=False) + "\n" for item in results))
    for result in results:
        print(f"{result['status']:11} {result['id']}" + (f" — {result['error']}" if "error" in result else ""))
    passed = sum(item["status"] == "pass" for item in results)
    failed = sum(item["status"] == "fail" for item in results)
    print(f"CLI references: {passed} passed, {failed} failed; {len(results) - passed - failed} policy-only")
    return 1 if failed else 0


if __name__ == "__main__":
    raise SystemExit(main())
