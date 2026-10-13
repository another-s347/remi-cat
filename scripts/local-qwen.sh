#!/usr/bin/env bash
set -euo pipefail
project_dir="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$project_dir"
proxy_pid=""
credential_file=""
cleanup() {
  if [[ -n "$proxy_pid" ]]; then kill "$proxy_pid" 2>/dev/null || true; wait "$proxy_pid" 2>/dev/null || true; fi
  if [[ -n "$credential_file" ]]; then rm -f -- "$credential_file"; fi
}
trap cleanup EXIT
if ! curl --noproxy '*' --silent --fail --max-time 2 http://127.0.0.1:8080/v1/models >/dev/null; then
  if [[ ! -x /mnt/c/Windows/System32/curl.exe ]]; then
    echo 'No local model service at 127.0.0.1:8080 and Windows curl is unavailable.' >&2
    exit 1
  fi
  python3 scripts/windows-local-model-proxy.py &
  proxy_pid=$!
  ready=false
  for attempt in {1..30}; do
    if curl --noproxy '*' --silent --fail --max-time 2 http://127.0.0.1:8080/v1/models >/dev/null; then ready=true; break; fi
    if ! kill -0 "$proxy_pid" 2>/dev/null; then break; fi
    sleep 0.2
  done
  if [[ "$ready" != true ]]; then echo 'Cannot reach the Windows-local model service.' >&2; exit 1; fi
fi
export OPENAI_API_KEY=local REMI_MODEL_PROFILE=local-qwen REMI_DATA_DIR="$project_dir/.remi-cat"
export NO_PROXY="${NO_PROXY:+$NO_PROXY,}127.0.0.1,localhost"
export no_proxy="${no_proxy:+$no_proxy,}127.0.0.1,localhost"
# TUI resolves a Profile independently of REMI_DATA_DIR and loads its runtime
# config as authority over REMI_MODEL_PROFILE. Select both explicitly.
export REMI_PROFILE="$project_dir/profiles/local-qwen/profile.yaml"
export REMI_RUNTIME_CONFIG="$project_dir/profiles/local-qwen/runtime.yaml"
# Application runtimes take credentials from the selected SecretStore rather
# than the process environment. Supply only the local service's placeholder.
credential_file="$(mktemp /tmp/remi-local-qwen-credentials.XXXXXX)"
printf 'OPENAI_API_KEY=local\n' > "$credential_file"
export REMI_SECRET_BACKEND=dotenv REMI_SECRET_DOTENV_PATH="$credential_file"
remi_bin="${REMI_LOCAL_BINARY:-$project_dir/target/debug/remi-cat}"
if [[ ! -x "$remi_bin" ]]; then echo 'Build remi-cat first with cargo build, or set REMI_LOCAL_BINARY.' >&2; exit 1; fi
if [[ $# == 0 ]]; then set -- tui; fi
"$remi_bin" --profile "$REMI_PROFILE" "$@"
