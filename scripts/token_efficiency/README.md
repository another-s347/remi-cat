Token efficiency benchmark
==========================

This runs **real CLI harnesses against the same Kimi API**, not mock agent loops.
`tasks.py` creates identical independent workspaces for CSV repair, a retry utility,
and log diagnosis. Acceptance checks are outside each agent's workspace.

The 2026-10-31 report is in `docs/benchmarks/token-efficiency-2026-10-31.md`.
Full request/response evidence and npm lockfiles are kept under
`target/token-efficiency-2026-10-31/`, which Git ignores.

Requirements: Python with `tiktoken` and `PyYAML`; built remi-cat; Node 24.22.2;
`@deepseek-ai/dsh@0.2.1-rc.5`; `@opencode/cli-linux-x64@2.2.5`.
The experiment's installed packages have lockfiles in the evidence directory.
Use the platform OpenCode executable directly if its npm wrapper's postinstall
has not run. DSH's npm package also needs its declared peer packages; the saved
runtime lockfile includes them. No global configuration is changed.

Example (paths may be overridden with `--remi`, `--opencode`, `--node`, `--dsh`):

```sh
TIKTOKEN_CACHE_DIR=/tmp/remi-token-bench/tiktoken-cache \
  /tmp/remi-token-bench/venv/bin/python scripts/token_efficiency/run.py \
  --output /tmp/remi-token-bench/new-run --repeats 2 --workers 2
python3 scripts/token_efficiency/grade.py /tmp/remi-token-bench/new-run
python3 scripts/token_efficiency/analyze.py /tmp/remi-token-bench/new-run \
  --output /tmp/remi-token-bench/new-summary.json
```

`--harness remi-lean` tests a short coding prompt and a smaller tool allowlist.
`--harness remi-no-todo` removes only the default Todo instruction and Todo tools.
These are **benchmark-only profile variants**, not product changes.

Accounting rules:

- The proxy reads only `KIMI_API_KEY` from the repository dotenv into memory.
  It never saves authentication headers. Harness children receive placeholder
  credentials and isolated HOME/config/data directories.
- The proxy records original and forwarded JSON bodies separately. It fixes
  model, temperature 0.6, thinking disabled, and maximum output 8192; it requests
  streaming usage. Auxiliary title requests are included in totals.
- Authoritative totals come from API `usage`, once per response. Cached input
  is a subset of prompt tokens. Reasoning is a subset of completion tokens.
  Missing usage stays missing; estimates never replace it.
- Component counts use **cl100k_base estimates**, not Kimi's billing tokenizer.
  They include serialization and intentionally do not add up to API totals.
- Use a fresh output directory outside any repository, such as `/tmp/remi-token-bench/...`. Isolating HOME does not prevent ancestor project-skill discovery. The runner rejects this repository as an output ancestor; copy finished evidence into `target/` afterward. Current concurrency is for throughput, not
  controlled latency measurement. Provider caches are not flushed.
- A task passes when independent acceptance checks pass. Process exit 0 and
  agent self-reported test success are recorded separately from correctness.
- Acceptance v2 tests the injected sleep callback at positive delay. v1 required
  a callback even at delay 0, which the task did not explicitly require. Regrading
  preserves original verdicts and logs; agents never receive the hidden checks.

The workloads are intentionally small and use a single model/mode. They do not
establish general coding quality, long-session compression performance, or the
cost of user-specific memories and skills.

Extended integration suite
--------------------------

`--suite extended` adds three separate projects: Node.js DAG scheduling and JSON
CLI, Python/SQLite stock reservation and version migration, and a Rust/std-only
persistent text index. Each workspace contains `SPEC.md`, starter modules and a
small existing test. External acceptance code lives in `extended_checks/` and
is not copied into agent workspaces. It checks APIs and subprocess/IO behavior;
it is not a complete proof of all behavior or code quality.

Use the same frozen harness binaries, parameters, timeout and randomized job
order for all three harnesses. Example:

```sh
TIKTOKEN_CACHE_DIR=/tmp/remi-token-bench/tiktoken-cache \
  /tmp/remi-token-bench/venv/bin/python scripts/token_efficiency/run.py \
  --suite extended --output /tmp/remi-token-bench/new-extended-run \
  --timeout 900 --repeats 2 --workers 2 --order-seed 20261031
python3 scripts/token_efficiency/grade.py --suite extended /tmp/remi-token-bench/new-extended-run
```

Extended runs prepend the same pinned Node executable directory and installed
stable Rust toolchain directory to every child PATH. Rust projects have no
external crates; no dependency download is needed. The grader uses the pinned
Node executable and offline Rust builds. The current fixture version is
`extended-v1`; freeze fixture/check hashes and validate baseline/reference
behavior before starting a batch. Preserve failed and timed-out runs.

For a prompt-only Remi retest, pass `--remi-agent-file /absolute/path/default.md`
to replace the isolated default agent while keeping the frozen binary. Add
`--remi-wait-background-tasks` to match the existing waiting/async follow-up
mode; keep an explicit external `--timeout`. Save the prompt snapshot and flags
with the experiment. These options affect Remi cases only.

These are author-constructed, longer single-session integration tasks. They are
not an independent public benchmark, and do not by themselves establish context
compaction quality, multi-hour endurance or general harness superiority.
