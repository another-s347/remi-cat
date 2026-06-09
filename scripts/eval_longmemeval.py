#!/usr/bin/env python3
"""Run a small LongMemEval-style smoke eval through remi-cat CLI.

This is intentionally lightweight: it ingests each haystack session as a
historical transcript message, asks the benchmark question, and records a
simple substring/exact-match score for quick regressions. Use the official
LongMemEval grader for publishable numbers.
"""

from __future__ import annotations

import argparse
import json
import os
import re
import shlex
import shutil
import subprocess
import sys
import tempfile
import time
from pathlib import Path
from typing import Any


DEFAULT_COMMAND = "cargo run --quiet --"
DEFAULT_SOURCE_DATA_DIR = ".remi-cat"


def main() -> int:
    args = parse_args()
    records = load_records(args.dataset)
    selected = select_records(records, args)
    if not selected:
        print("no cases selected", file=sys.stderr)
        return 2

    repo = Path(args.repo).resolve()
    source_data_dir = (repo / args.source_data_dir).resolve()
    run_data_dir = prepare_data_dir(source_data_dir, args.data_dir)
    command = shlex.split(args.command)
    output_path = Path(args.output).resolve() if args.output else None
    if output_path:
        output_path.parent.mkdir(parents=True, exist_ok=True)
        if not args.append_output:
            output_path.write_text("", encoding="utf-8")

    env = os.environ.copy()
    env["REMI_DATA_DIR"] = str(run_data_dir)
    env["REMI_ADMIN_ENABLED"] = "false"
    env["REMI_EVAL_MODE"] = args.mode
    env.setdefault("RUST_LOG", "error")

    print(f"dataset: {args.dataset}")
    print(f"cases: {len(selected)}")
    print(f"data_dir: {run_data_dir}")
    print(f"command: {' '.join(command)}")
    print(f"mode: {args.mode}")

    results: list[dict[str, Any]] = []
    started = time.time()
    try:
        for index, record in enumerate(selected, 1):
            result = run_case(index, len(selected), record, args, repo, command, env)
            results.append(result)
            print_case_result(result)
            append_jsonl(output_path, result)
    finally:
        if args.cleanup and args.data_dir is None:
            shutil.rmtree(run_data_dir, ignore_errors=True)

    total = len(results)
    correct = sum(1 for item in results if item["match"])
    print(f"summary: {correct}/{total} matched in {time.time() - started:.1f}s")
    return 0 if correct == total else 1


def parse_args() -> argparse.Namespace:
    parser = argparse.ArgumentParser(
        description="Smoke-test remi-cat on a few LongMemEval/LongMemEval-S cases."
    )
    parser.add_argument(
        "dataset",
        help="Path to longmemeval_s.json, longmemeval_m.json, or a compatible jsonl file.",
    )
    parser.add_argument("--repo", default=".", help="Repository root. Default: current dir.")
    parser.add_argument(
        "--source-data-dir",
        default=DEFAULT_SOURCE_DATA_DIR,
        help="Initialized remi-cat data dir used as the profile/config template.",
    )
    parser.add_argument(
        "--data-dir",
        help="Eval data dir. Default: temporary copy derived from --source-data-dir.",
    )
    parser.add_argument(
        "--command",
        default=DEFAULT_COMMAND,
        help=f"Command prefix used to invoke remi-cat. Default: {DEFAULT_COMMAND!r}.",
    )
    parser.add_argument(
        "--mode",
        choices=("prompt", "cli"),
        default="prompt",
        help="Use pure prompt mode or the CLI IM wrapper. Default: prompt.",
    )
    parser.add_argument("--limit", type=int, default=3, help="Number of cases to run.")
    parser.add_argument(
        "--offset", type=int, default=0, help="Start offset before applying --limit."
    )
    parser.add_argument(
        "--case-id",
        action="append",
        default=[],
        help="Question/case id to run. Can be repeated; overrides offset/limit selection.",
    )
    parser.add_argument(
        "--max-sessions",
        type=int,
        default=None,
        help="Only ingest the last N haystack sessions for each case.",
    )
    parser.add_argument(
        "--sample-sessions",
        type=int,
        default=None,
        help="Ingest at most N sessions per case, always including answer_session_ids plus deterministic distractors.",
    )
    parser.add_argument(
        "--evidence-only",
        action="store_true",
        help="Only ingest sessions listed in answer_session_ids. Useful for cheap smoke tests, not official scoring.",
    )
    parser.add_argument(
        "--max-session-chars",
        type=int,
        default=40000,
        help="Truncate each ingested session transcript to this many characters.",
    )
    parser.add_argument(
        "--user",
        default="longmemeval-user",
        help="CLI user id used for all eval turns.",
    )
    parser.add_argument(
        "--name",
        default="LongMemEval",
        help="CLI display name used for all eval turns.",
    )
    parser.add_argument(
        "--timeout",
        type=float,
        default=180.0,
        help="Timeout in seconds for each remi-cat CLI call.",
    )
    parser.add_argument(
        "--retries",
        type=int,
        default=2,
        help="Retry each remi-cat CLI call this many times after transient failures.",
    )
    parser.add_argument(
        "--output",
        default="target/longmemeval-smoke/results.jsonl",
        help="JSONL result path. Use empty string to disable.",
    )
    parser.add_argument(
        "--append-output",
        action="store_true",
        help="Append to --output instead of replacing it at the start of the run.",
    )
    parser.add_argument(
        "--keep-data-dir",
        dest="cleanup",
        action="store_false",
        help="Keep the temporary eval data dir for debugging.",
    )
    parser.set_defaults(cleanup=True)
    return parser.parse_args()


def load_records(path_text: str) -> list[dict[str, Any]]:
    path = Path(path_text)
    raw = path.read_text(encoding="utf-8")
    stripped = raw.lstrip()
    if not stripped:
        return []
    if stripped[0] == "[":
        data = json.loads(raw)
        if not isinstance(data, list):
            raise ValueError("top-level JSON must be a list")
        return [item for item in data if isinstance(item, dict)]

    records: list[dict[str, Any]] = []
    for line_no, line in enumerate(raw.splitlines(), 1):
        line = line.strip()
        if not line:
            continue
        item = json.loads(line)
        if not isinstance(item, dict):
            raise ValueError(f"line {line_no} is not a JSON object")
        records.append(item)
    return records


def select_records(records: list[dict[str, Any]], args: argparse.Namespace) -> list[dict[str, Any]]:
    if args.case_id:
        wanted = set(args.case_id)
        return [record for record in records if case_id(record) in wanted]
    end = args.offset + args.limit
    return records[args.offset : end]


def prepare_data_dir(source_data_dir: Path, requested: str | None) -> Path:
    target = Path(requested).resolve() if requested else Path(tempfile.mkdtemp(prefix="remi-lme-"))
    target.mkdir(parents=True, exist_ok=True)

    for dirname in ("agents", "models", "skills"):
        src = source_data_dir / dirname
        dst = target / dirname
        if src.exists() and not dst.exists():
            shutil.copytree(src, dst)

    src_config = source_data_dir / "runtime.yaml"
    dst_config = target / "runtime.yaml"
    if src_config.exists() and not dst_config.exists():
        config = patch_runtime_yaml(src_config.read_text(encoding="utf-8"), target)
        dst_config.write_text(config, encoding="utf-8")
    elif not dst_config.exists() and not has_legacy_model_env():
        raise RuntimeError(
            f"{source_data_dir}/runtime.yaml not found and OPENAI_API_KEY/REMI_API_KEY is not set"
        )
    return target


def patch_runtime_yaml(raw: str, data_dir: Path) -> str:
    lines = raw.splitlines()
    out: list[str] = []
    for line in lines:
        stripped = line.strip()
        if stripped.startswith("data_dir:"):
            out.append(f"data_dir: {quote_yaml(str(data_dir))}")
        elif stripped.startswith("enabled:") and is_under_admin(out):
            out.append(re.sub(r"enabled:\s+\S+", "enabled: false", line))
        elif stripped.startswith("host_dir:"):
            out.append(re.sub(r"host_dir:\s+.*", f"host_dir: {quote_yaml(str(data_dir))}", line))
        elif stripped.startswith("container_name:"):
            out.append(
                re.sub(
                    r"container_name:\s+.*",
                    "container_name: remi-cat-sandbox-longmemeval",
                    line,
                )
            )
        else:
            out.append(line)
    return "\n".join(out) + "\n"


def quote_yaml(value: str) -> str:
    return json.dumps(value)


def is_under_admin(lines: list[str]) -> bool:
    for line in reversed(lines):
        if not line or line.startswith(" "):
            continue
        return line.strip() == "admin:"
    return False


def has_legacy_model_env() -> bool:
    return bool(os.environ.get("OPENAI_API_KEY") or os.environ.get("REMI_API_KEY"))


def run_case(
    index: int,
    total: int,
    record: dict[str, Any],
    args: argparse.Namespace,
    repo: Path,
    command: list[str],
    env: dict[str, str],
) -> dict[str, Any]:
    qid = case_id(record)
    channel = safe_channel(f"lme-{qid}")
    sessions = haystack_sessions(record)
    if args.evidence_only:
        sessions = evidence_sessions(record, sessions)
    elif args.sample_sessions is not None:
        sessions = sample_sessions_with_evidence(record, sessions, args.sample_sessions)
    if args.max_sessions is not None:
        sessions = sessions[-args.max_sessions :]

    print(f"[{index}/{total}] ingest {qid}: {len(sessions)} sessions")
    for session_index, session in enumerate(sessions, 1):
        prompt = build_ingest_prompt(record, session, session_index, args.max_session_chars)
        invoke_remi(
            repo,
            command,
            channel,
            args.user,
            args.name,
            prompt,
            env,
            args.timeout,
            args.retries,
        )

    question = str(record.get("question", "")).strip()
    if not question:
        raise ValueError(f"case {qid} has no question")
    answer = stringify_answer(record.get("answer", ""))
    query_prompt = (
        "Answer the following benchmark question using prior context and memory from this session. "
        "Before giving the final answer, first call the search tool with scope=memory and distinctive keywords from the question. "
        "Reply with only the answer, no explanation.\n\n"
        f"Question: {question}"
    )
    prediction = invoke_remi(
        repo,
        command,
        channel,
        args.user,
        args.name,
        query_prompt,
        env,
        args.timeout,
        args.retries,
    ).strip()
    matched = answer_matches(answer, prediction)
    return {
        "question_id": qid,
        "question_type": record.get("question_type"),
        "question": question,
        "answer": answer,
        "prediction": prediction,
        "match": matched,
        "haystack_sessions": len(sessions),
    }


def invoke_remi(
    repo: Path,
    command: list[str],
    channel: str,
    user: str,
    name: str,
    message: str,
    env: dict[str, str],
    timeout: float,
    retries: int,
) -> str:
    if env.get("REMI_EVAL_MODE") == "prompt":
        full_command = command + ["prompt", "--session", channel, message]
    else:
        full_command = command + [
            "cli",
            "--channel",
            channel,
            "--user",
            user,
            "--name",
            name,
            message,
        ]
    last_error = None
    for attempt in range(retries + 1):
        try:
            completed = subprocess.run(
                full_command,
                cwd=repo,
                env=env,
                text=True,
                stdout=subprocess.PIPE,
                stderr=subprocess.PIPE,
                timeout=timeout,
                check=False,
            )
        except subprocess.TimeoutExpired as err:
            last_error = RuntimeError(f"remi-cat timed out after {timeout}s")
        else:
            if completed.returncode == 0:
                return completed.stdout.strip()
            last_error = RuntimeError(
                f"remi-cat failed with exit code {completed.returncode}\n"
                f"stdout:\n{completed.stdout}\n"
                f"stderr:\n{completed.stderr}"
            )
        if attempt < retries:
            delay = 1.5 * (attempt + 1)
            print(
                f"  retry {attempt + 1}/{retries} after remi-cat call failure; sleeping {delay:.1f}s",
                file=sys.stderr,
            )
            time.sleep(delay)
    assert last_error is not None
    raise last_error


def build_ingest_prompt(
    record: dict[str, Any],
    session: Any,
    session_index: int,
    max_chars: int,
) -> str:
    text = session_to_text(session)
    if len(text) > max_chars:
        text = text[-max_chars:]
        text = "[truncated to tail]\n" + text

    session_id = session_identifier(record, session, session_index)
    return (
        "Store the following historical conversation transcript as prior context "
        "for this benchmark session. Do not answer questions from it yet. Reply exactly: OK.\n\n"
        f"Session id: {session_id}\n"
        f"Transcript:\n{text}"
    )


def session_to_text(session: Any) -> str:
    if isinstance(session, dict):
        messages = (
            session.get("messages")
            or session.get("turns")
            or session.get("conversation")
            or session.get("dialogue")
        )
        if messages is not None:
            header = []
            for key in ("session_id", "id", "date", "timestamp"):
                value = session.get(key)
                if value:
                    header.append(f"{key}: {value}")
            body = messages_to_text(messages)
            return "\n".join(header + [body]).strip()
        return json.dumps(session, ensure_ascii=False)
    if isinstance(session, list):
        return messages_to_text(session)
    return str(session)


def messages_to_text(messages: Any) -> str:
    if isinstance(messages, str):
        return messages
    if not isinstance(messages, list):
        return json.dumps(messages, ensure_ascii=False)

    lines: list[str] = []
    for item in messages:
        if isinstance(item, dict):
            role = (
                item.get("role")
                or item.get("speaker")
                or item.get("from")
                or item.get("name")
                or "message"
            )
            content = (
                item.get("content")
                or item.get("text")
                or item.get("message")
                or item.get("value")
                or ""
            )
            if isinstance(content, list):
                content = " ".join(map(str, content))
            lines.append(f"{role}: {content}")
        elif isinstance(item, (list, tuple)) and len(item) >= 2:
            lines.append(f"{item[0]}: {item[1]}")
        else:
            lines.append(str(item))
    return "\n".join(lines)


def haystack_sessions(record: dict[str, Any]) -> list[Any]:
    sessions = record.get("haystack_sessions")
    if isinstance(sessions, list):
        return sessions
    messages = record.get("messages") or record.get("conversation")
    if isinstance(messages, list):
        return [messages]
    raise ValueError(f"case {case_id(record)} has no haystack_sessions/messages")


def evidence_sessions(record: dict[str, Any], sessions: list[Any]) -> list[Any]:
    answer_ids = record.get("answer_session_ids")
    haystack_ids = record.get("haystack_session_ids")
    if not isinstance(answer_ids, list) or not isinstance(haystack_ids, list):
        return sessions

    wanted = {str(item) for item in answer_ids}
    filtered = [
        session
        for sid, session in zip(haystack_ids, sessions)
        if str(sid) in wanted
    ]
    return filtered or sessions


def sample_sessions_with_evidence(
    record: dict[str, Any], sessions: list[Any], limit: int
) -> list[Any]:
    if limit <= 0 or len(sessions) <= limit:
        return sessions

    answer_ids = record.get("answer_session_ids")
    haystack_ids = record.get("haystack_session_ids")
    if not isinstance(answer_ids, list) or not isinstance(haystack_ids, list):
        return evenly_spaced_sample(sessions, limit)

    wanted = {str(item) for item in answer_ids}
    evidence_indexes = [
        index for index, sid in enumerate(haystack_ids) if str(sid) in wanted
    ]
    selected = set(evidence_indexes)
    remaining = max(0, limit - len(selected))
    for index in evenly_spaced_indexes(len(sessions), remaining):
        selected.add(index)
    if len(selected) < limit:
        for index in range(len(sessions)):
            selected.add(index)
            if len(selected) >= limit:
                break
    return [sessions[index] for index in sorted(selected)[:limit]]


def evenly_spaced_sample(items: list[Any], limit: int) -> list[Any]:
    return [items[index] for index in evenly_spaced_indexes(len(items), limit)]


def evenly_spaced_indexes(total: int, limit: int) -> list[int]:
    if limit <= 0:
        return []
    if limit >= total:
        return list(range(total))
    if limit == 1:
        return [total - 1]
    return sorted({round(i * (total - 1) / (limit - 1)) for i in range(limit)})


def session_identifier(record: dict[str, Any], session: Any, index: int) -> str:
    if isinstance(session, dict):
        for key in ("session_id", "id", "date", "timestamp"):
            value = session.get(key)
            if value:
                return str(value)
    ids = record.get("haystack_session_ids")
    if isinstance(ids, list) and len(ids) >= index:
        return str(ids[index - 1])
    return str(index)


def case_id(record: dict[str, Any]) -> str:
    for key in ("question_id", "sample_id", "id", "case_id"):
        value = record.get(key)
        if value is not None:
            return str(value)
    return str(abs(hash(json.dumps(record, sort_keys=True, ensure_ascii=False))))


def safe_channel(value: str) -> str:
    return re.sub(r"[^A-Za-z0-9_.:-]+", "-", value)[:120]


def stringify_answer(answer: Any) -> str:
    if isinstance(answer, str):
        return answer.strip()
    if isinstance(answer, list):
        return " | ".join(str(item).strip() for item in answer)
    return str(answer).strip()


def answer_matches(answer: str, prediction: str) -> bool:
    answer_norm = normalize(answer)
    prediction_norm = normalize(prediction)
    if not answer_norm:
        return not prediction_norm
    parts = [part.strip() for part in answer_norm.split("|") if part.strip()]
    if len(parts) > 1:
        return any(part in prediction_norm for part in parts)
    return answer_norm in prediction_norm or prediction_norm in answer_norm


def normalize(value: str) -> str:
    lowered = value.lower()
    lowered = re.sub(r"\s+", " ", lowered)
    lowered = re.sub(r"[\"'`。，、！？!?,.;:()\[\]{}]", "", lowered)
    return lowered.strip()


def print_case_result(result: dict[str, Any]) -> None:
    status = "PASS" if result["match"] else "FAIL"
    print(
        f"{status} {result['question_id']} "
        f"type={result.get('question_type')} sessions={result['haystack_sessions']}"
    )
    print(f"  gold: {result['answer']}")
    print(f"  pred: {result['prediction'][:500]}")


def append_jsonl(path: Path | None, item: dict[str, Any]) -> None:
    if path is None:
        return
    with path.open("a", encoding="utf-8") as handle:
        handle.write(json.dumps(item, ensure_ascii=False) + "\n")


if __name__ == "__main__":
    raise SystemExit(main())
