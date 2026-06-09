#!/usr/bin/env bash
# Build and upload the single remi-cat executable to a remote host.

set -euo pipefail

SSH_HOST=""
REMOTE_DIR=""
SKIP_BUILD=0

while [[ $# -gt 0 ]]; do
    case "$1" in
        -h) SSH_HOST="$2"; shift 2 ;;
        -d) REMOTE_DIR="$2"; shift 2 ;;
        -r) SKIP_BUILD=1; shift ;;
        *) echo "Unknown option: $1"; exit 1 ;;
    esac
done

if [[ -z "${SSH_HOST}" || -z "${REMOTE_DIR}" ]]; then
    echo "Usage: $0 -h <ssh-host> -d <remote-dir> [-r]"
    exit 1
fi

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
BIN="${SCRIPT_DIR}/target/release/remi-cat"

if [[ "${SKIP_BUILD}" -eq 0 ]]; then
    cd "${SCRIPT_DIR}"
    cargo build --release -p remi-cat
fi

[[ -f "${BIN}" ]] || { echo "Binary not found: ${BIN}" >&2; exit 1; }

ssh -o StrictHostKeyChecking=accept-new "${SSH_HOST}" "mkdir -p '${REMOTE_DIR}/bin'"
scp "${BIN}" "${SSH_HOST}:${REMOTE_DIR}/bin/remi-cat"
ssh "${SSH_HOST}" "chmod +x '${REMOTE_DIR}/bin/remi-cat'"

echo "Deployed ${REMOTE_DIR}/bin/remi-cat"
