#!/usr/bin/env python3
"""Smoke-test real manage_yourself tool selection on read-only dataset cases.

Each case runs a fresh profile and HOME. A wrapper logs the exact CLI argv
issued by manage_yourself, so answers alone cannot pass the check.
"""

from __future__ import annotations

import argparse
import json
import os
from pathlib import Path
import re
import shlex
import shutil
import subprocess
import tempfile


READ_ONLY = {
    "discover_profiles": ("profile list", ()),
    "current_profile": ("profile current", ()),
    "inspect_manifest": ("profile show", ("--manifest",)),
    "inspect_resolved": ("profile show", ("--resolved",)),
    "inspect_sources": ("profile show", ("--sources",)),
    "validate_profile": ("profile check", ()),
    "list_resources": ("profile resource list", ()),
    "show_runtime_resource": ("profile resource show", ()),
    "list_channels": ("profile channel list", ()),
    "instance_status": ("profile status", ()),
    "all_instance_status": ("profile status", ("--all",)),
    "registry_info": ("profile registry info", ()),
}
MUTATING = {
    "change_context_window": "profile model set",
    "set_compaction_threshold": "config set auto_compress_context_percent=70",
    "upsert_feishu_event_hook": "profile channel upsert-feishu",
}
EXPECTED_REPLY = {
    "current_profile": "eval.profile",
    "inspect_sources": "manifest",
    "list_channels": "feishu.default",
    "change_context_window": "65536",
    "set_compaction_threshold": "70",
    "upsert_feishu_event_hook": "9876",
}


def credential_from_dotenv(path: Path, key: str) -> str:
    for line in path.read_text().splitlines():
        if line.startswith(f"{key}="):
            return line.split("=", 1)[1].strip().strip('"\'')
    raise ValueError(f"{key} is absent from {path}")


def command_words(command: str) -> list[str]:
    words = shlex.split(command)
    return words[2:] if len(words) >= 3 and words[0] == "--profile" else words


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--dataset", type=Path, default=Path(__file__).with_name("manage_yourself.jsonl"))
    parser.add_argument("--binary", type=Path, default=Path("target/debug/remi-cat"))
    parser.add_argument("--model-profile-file", type=Path, required=True)
    parser.add_argument("--model-profile-id", required=True)
    parser.add_argument("--credential-env", help="Credential environment variable required by the model")
    parser.add_argument("--dotenv", type=Path, help="Read only --credential-env from this file")
    parser.add_argument("--case", action="append", default=[])
    parser.add_argument("--allow-mutations", action="store_true", help="Allow selected writes inside fresh temporary profiles")
    parser.add_argument("--timeout", type=int, default=180)
    parser.add_argument("--rounds", type=int, default=1,
                        help="Run each case for this many consecutive turns in one persisted CLI channel")
    parser.add_argument("--fresh-followup-channel", action="store_true",
                        help="Diagnostic: use a new channel for follow-up turns to isolate session-history effects")
    parser.add_argument("--initial-threshold", type=int,
                        help="Diagnostic: set compaction percentage before the first model turn")
    parser.add_argument("--rust-log", default="error", help="RUST_LOG value for diagnostic runs")
    parser.add_argument("--trace-dir", type=Path, help="Save per-round stderr diagnostics to this directory")
    parser.add_argument("--output", type=Path)
    args = parser.parse_args()
    if args.rounds < 1:
        parser.error("--rounds must be positive")
    requested = set(args.case or ["discover_profiles", "current_profile"])
    allowed = READ_ONLY.keys() | (MUTATING.keys() if args.allow_mutations else set())
    unknown = requested - allowed
    if unknown:
        parser.error(f"not a supported live case: {', '.join(sorted(unknown))}")
    cases = [json.loads(line) for line in args.dataset.read_text().splitlines() if line.strip()]
    cases = [case for case in cases if case["id"] in requested]
    if len(cases) != len(requested):
        parser.error("a requested case is absent from the dataset")
    binary = args.binary.resolve()
    model_source = args.model_profile_file.resolve()
    results = []
    for case in cases:
        with tempfile.TemporaryDirectory(prefix="remi-self-live-") as raw:
            root = Path(raw)
            profile = root / "profile"
            env = {key: value for key, value in os.environ.items() if not (key.endswith("_API_KEY") or key.endswith("_TOKEN"))}
            env.update(HOME=str(root / "home"), REMI_DATA_DIR=str(root / "data"),
                       REMI_PROFILE_REGISTRY_ROOT=str(root / "registry"), RUST_LOG="error")
            Path(env["HOME"]).mkdir()
            if args.credential_env:
                value = credential_from_dotenv(args.dotenv, args.credential_env) if args.dotenv else os.environ.get(args.credential_env)
                if not value:
                    parser.error(f"{args.credential_env} is unavailable")
                env[args.credential_env] = value

            def invoke(argv: list[str], timeout: int = 45, diagnostic: bool = False) -> subprocess.CompletedProcess[str]:
                child_env = env.copy()
                if diagnostic:
                    child_env["RUST_LOG"] = args.rust_log
                return subprocess.run([str(binary), *argv], cwd=root, env=child_env, text=True,
                                      capture_output=True, timeout=timeout)

            init = invoke(["profile", "init", str(profile), "--id", "eval.profile",
                           "--name", "Eval", "--with-runtime"])
            if init.returncode:
                results.append({"id": case["id"], "status": "setup_failed", "error": init.stderr[-300:]})
                continue
            show = invoke(["profile", "show", str(profile / "profile.yaml"), "--resolved", "--format", "json"])
            if show.returncode:
                results.append({"id": case["id"], "status": "setup_failed", "error": show.stderr[-300:]})
                continue
            resolved = json.loads(show.stdout)
            models = Path(resolved["resources"]["models"])
            models.mkdir(parents=True, exist_ok=True)
            shutil.copyfile(model_source, models / model_source.name)
            mutating = case["id"] in MUTATING
            if mutating:
                agent_path = Path(resolved["resources"]["agents"]) / "default.md"
                agent = agent_path.read_text()
                agent, count = re.subn(
                    r"(?ms)^tools:\n.*?^max_turns:",
                    "tools:\n  - search\n  - skill__get\n  - manage_yourself\n  - fs_read\n  - fs_write\n  - fs_ls\n  - apply_patch\ndelegates: []\nmax_turns:",
                    agent,
                )
                if count != 1:
                    results.append({"id": case["id"], "status": "setup_failed", "error": "agent tool list missing"})
                    continue
                agent_path.write_text(agent)
            config_path = Path(resolved["config"]["runtime"])
            config = config_path.read_text()
            config, count = re.subn(r"(?m)^model_profile:.*$", f"model_profile: {args.model_profile_id}", config)
            if count != 1:
                results.append({"id": case["id"], "status": "setup_failed", "error": "runtime model_profile missing"})
                continue
            if args.initial_threshold is not None:
                config += f"\nauto_compress_context_percent: {args.initial_threshold}\n"
            config_path.write_text(config)

            log = root / "manage_calls.txt"
            wrapper = root / "remi-eval-cli"
            wrapper.write_text('#!/bin/sh\n"$REMI_EVAL_BINARY" "$@"\nstatus=$?\nprintf "%s\\t%s\\n" "$status" "$*" >> "$REMI_EVAL_CALL_LOG"\nexit "$status"\n')
            wrapper.chmod(0o700)
            env.update(REMI_CLI_EXE=str(wrapper), REMI_EVAL_CALL_LOG=str(log), REMI_EVAL_BINARY=str(binary))
            target = "" if case["id"] in {"discover_profiles", "current_profile", "all_instance_status", "registry_info"} else f" 目标 Profile：{profile / 'profile.yaml'}。"
            instruction = ("只修改这个临时 Profile 的相关文件或配置，并在完成后读取验证。"
                           "runtime.yaml 参数优先用 manage_yourself 的 config set 命令；"
                           "模型定义优先用 profile model set 命令。" if mutating
                           else "请实际使用 manage_yourself 查询，不要猜测；只做只读操作。")
            if case["id"] == "change_context_window":
                instruction += ("默认模型指 runtime.yaml 中 model_profile 选中的模型；"
                                "profile model set 省略模型 ID 可直接定位它。"
                                "如果调小 context_tokens，要同时确保 max_output_tokens 小于新值，"
                                "并通过 tools --json 加载校验。")
            if case["id"] == "set_compaction_threshold":
                instruction += "只需验证 runtime.yaml 中的阈值，无需列出所有工具。"
            prompt = f"{case['user']}{target} {instruction}"
            secret = env.get(args.credential_env, "") if args.credential_env else ""
            try:
                response = invoke(["--profile", str(profile / "profile.yaml"), "--cli-channel", "eval-rounds",
                                   "prompt", "--permissions", "medium" if mutating else "low",
                                   "--no-telemetry", prompt], args.timeout, diagnostic=True)
            except subprocess.TimeoutExpired:
                results.append({"id": case["id"], "status": "timeout"})
                continue
            if args.trace_dir:
                args.trace_dir.mkdir(parents=True, exist_ok=True)
                trace = response.stderr.replace(secret, "[REDACTED]") if secret else response.stderr
                (args.trace_dir / f"{case['id']}-round-1.stderr.log").write_text(trace)
                trace = response.stdout.replace(secret, "[REDACTED]") if secret else response.stdout
                (args.trace_dir / f"{case['id']}-round-1.stdout.log").write_text(trace)
            calls = log.read_text().splitlines() if log.exists() else []
            if mutating:
                required_command = MUTATING[case["id"]]
                matched = required_command is None or any(
                    status == "0" and required_command in command
                    for status, command in (call.split("\t", 1) for call in calls if "\t" in call)
                )
                for check in case.get("file_contains", []):
                    path = Path(check["path"].format_map({"profile_dir": str(profile),
                                                           "runtime_file": str(config_path),
                                                           "model_file": str(models / model_source.name)}))
                    if not path.exists() or check["text"] not in path.read_text():
                        matched = False
                if case["id"] == "change_context_window":
                    valid = invoke(["--profile", str(profile / "profile.yaml"), "tools", "--json"])
                    matched = matched and valid.returncode == 0
            else:
                prefix_text, required_flags = READ_ONLY[case["id"]]
                prefix = shlex.split(prefix_text)
                matched = any(
                    (lambda status, command: status == "0"
                     and command_words(command)[:len(prefix)] == prefix
                     and all(flag in command_words(command) for flag in required_flags))(*call.split("\t", 1))
                    for call in calls if "\t" in call
                )
            answer_excerpt = response.stdout[-600:]
            error_excerpt = response.stderr[-300:]
            if secret:
                answer_excerpt = answer_excerpt.replace(secret, "[REDACTED]")
                error_excerpt = error_excerpt.replace(secret, "[REDACTED]")
            expected_reply = EXPECTED_REPLY.get(case["id"])
            reply_complete = (response.returncode == 0 and bool(response.stdout.strip())
                              and (expected_reply is None or expected_reply in response.stdout))
            rounds = [{"round": 1, "exit_code": response.returncode,
                       "reply_nonempty": bool(response.stdout.strip()),
                       "reply_expected": expected_reply is None or expected_reply in response.stdout,
                       "successful_tool_calls": sum(call.startswith("0\t") for call in calls)}]
            for round_number in range(2, args.rounds + 1):
                before = len(calls)
                followup = (f"这是第 {round_number} 轮。目标 manifest 的绝对路径是 {profile / 'profile.yaml'}。"
                            "请使用这个绝对路径，不要猜测 @alias。用 manage_yourself 重新读取并独立验证上一轮结果；"
                            "完成工具调用后，请用一句简短的话给出实际值或状态。不要再次修改。")
                if case["id"] == "set_compaction_threshold":
                    followup = (f"这是第 {round_number} 轮。请用 manage_yourself 查询 Profile "
                                f"{profile / 'profile.yaml'} 的 config.runtime 文件路径，再读取该文件，"
                                "回答 auto_compress_context_percent 的实际数值。不要修改。")
                if case["id"] == "change_context_window":
                    followup = (f"这是第 {round_number} 轮。请用 manage_yourself 执行 "
                                f"profile model show {profile / 'profile.yaml'} --format json，"
                                "回答运行时选中模型的 context_tokens 与 max_output_tokens 实际值。不要修改。")
                if case["id"] == "upsert_feishu_event_hook":
                    followup = (f"这是第 {round_number} 轮。请用 manage_yourself 查看 Profile "
                                f"{profile / 'profile.yaml'} 的 Channel 列表，再读取 channels.yaml，"
                                "回答禁用状态、Event Hook 监听 host、port 和 path 的实际值。不要修改。")
                try:
                    followup_channel = "eval-rounds-fresh" if args.fresh_followup_channel else "eval-rounds"
                    next_response = invoke(["--profile", str(profile / "profile.yaml"), "--cli-channel", followup_channel,
                                            "prompt", "--permissions", "low", "--no-telemetry", followup], args.timeout, diagnostic=True)
                except subprocess.TimeoutExpired:
                    rounds.append({"round": round_number, "status": "timeout"})
                    reply_complete = False
                    break
                if args.trace_dir:
                    trace = next_response.stderr.replace(secret, "[REDACTED]") if secret else next_response.stderr
                    (args.trace_dir / f"{case['id']}-round-{round_number}.stderr.log").write_text(trace)
                    trace = next_response.stdout.replace(secret, "[REDACTED]") if secret else next_response.stdout
                    (args.trace_dir / f"{case['id']}-round-{round_number}.stdout.log").write_text(trace)
                calls = log.read_text().splitlines() if log.exists() else []
                new_calls = calls[before:]
                round_ok = (next_response.returncode == 0 and bool(next_response.stdout.strip())
                            and (expected_reply is None or expected_reply in next_response.stdout)
                            and any(call.startswith("0\t") for call in new_calls))
                rounds.append({"round": round_number, "exit_code": next_response.returncode,
                               "reply_nonempty": bool(next_response.stdout.strip()),
                               "reply_expected": expected_reply is None or expected_reply in next_response.stdout,
                               "successful_tool_calls": sum(call.startswith("0\t") for call in new_calls),
                               "error_excerpt": next_response.stderr[-300:].replace(secret, "[REDACTED]") if secret else next_response.stderr[-300:],
                               "answer_excerpt": next_response.stdout[-300:].replace(secret, "[REDACTED]") if secret else next_response.stdout[-300:]})
                reply_complete = reply_complete and round_ok
            if mutating:
                for check in case.get("file_contains", []):
                    path = Path(check["path"].format_map({"profile_dir": str(profile),
                                                           "runtime_file": str(config_path),
                                                           "model_file": str(models / model_source.name)}))
                    if not path.exists() or check["text"] not in path.read_text():
                        matched = False
            results.append({"id": case["id"], "status": "pass" if reply_complete and matched else "fail",
                            "exit_code": response.returncode, "reply_nonempty": bool(response.stdout.strip()), "calls": calls,
                            "rounds": rounds,
                            "answer_excerpt": answer_excerpt, "error_excerpt": error_excerpt})
    if args.output:
        args.output.parent.mkdir(parents=True, exist_ok=True)
        args.output.write_text("".join(json.dumps(row, ensure_ascii=False) + "\n" for row in results))
    for row in results:
        print(f"{row['status']:12} {row['id']}: {row.get('calls', [])}")
    print(f"live tool selection: {sum(row['status'] == 'pass' for row in results)}/{len(results)} passed")
    return 0 if all(row["status"] == "pass" for row in results) else 1


if __name__ == "__main__":
    raise SystemExit(main())
