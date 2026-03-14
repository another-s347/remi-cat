#!/usr/bin/env bash
# deploy.sh — build locally and deploy to a remote server via SSH.
#
# Usage:
#   ./deploy.sh [options]
#
# Options:
#   -h HOST       SSH host (default: root@43.138.201.129)
#   -d DIR        Remote deploy directory (default: /root/remi-cat)
#   -r            Skip local build (use existing target/release binaries)
#   --no-docker   Skip rebuilding the Docker image / restarting the container
#
# Examples:
#   ./deploy.sh
#   ./deploy.sh -h root@1.2.3.4 -d /opt/remi-cat
#   ./deploy.sh -r                        # skip rebuild, just redeploy

set -euo pipefail

# ── defaults ──────────────────────────────────────────────────────────────────
SSH_HOST=""
REMOTE_DIR=""
SKIP_BUILD=0
SKIP_DOCKER=0

# ── arg parsing ───────────────────────────────────────────────────────────────
while [[ $# -gt 0 ]]; do
    case "$1" in
        -h) SSH_HOST="$2"; shift 2 ;;
        -d) REMOTE_DIR="$2"; shift 2 ;;
        -r) SKIP_BUILD=1; shift ;;
        --no-docker) SKIP_DOCKER=1; shift ;;
        *) echo "Unknown option: $1"; exit 1 ;;
    esac
done

if [[ -z "${SSH_HOST}" || -z "${REMOTE_DIR}" ]]; then
    echo "Usage: $0 -h <ssh-host> -d <remote-dir> [-r] [--no-docker]"
    echo "  -h  SSH host, e.g. root@1.2.3.4"
    echo "  -d  Remote deploy directory, e.g. /root/remi-cat"
    echo "  -r  Skip local build (use existing target/release binaries)"
    echo "  --no-docker  Skip Docker image rebuild / container restart"
    exit 1
fi

REMOTE_BIN="${REMOTE_DIR}/bin"
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"

bold()    { printf '\033[1m%s\033[0m\n' "$*"; }
info()    { printf '\033[1;34m  ➜\033[0m %s\n' "$*"; }
success() { printf '\033[1;32m  ✓\033[0m %s\n' "$*"; }
error()   { printf '\033[1;31m  ✗\033[0m %s\n' "$*" >&2; }

echo
bold "═══════════════════════════════════════════"
bold "  remi-cat deploy → ${SSH_HOST}:${REMOTE_DIR}"
bold "═══════════════════════════════════════════"
echo

# ── 1. local build ────────────────────────────────────────────────────────────
if [[ $SKIP_BUILD -eq 0 ]]; then
    info "Building release binaries..."
    cd "${SCRIPT_DIR}"
    cargo build --release -p remi-cat -p remi-cat-agent -p remi-daemon -p remi-admin \
        2>&1 | grep -E "^error|Compiling remi|Finished"
    success "Build complete"
else
    info "Skipping build (using existing target/release binaries)"
fi

# ── 2. stage files ────────────────────────────────────────────────────────────
STAGE=$(mktemp -d)
trap 'rm -rf "${STAGE}"' EXIT

cd "${SCRIPT_DIR}"
for bin in remi-cat remi-cat-agent remi-daemon remi-admin; do
    src="target/release/${bin}"
    dst="${STAGE}/${bin}-linux-x86_64"
    [[ -f "$src" ]] || { error "Binary not found: ${src}"; exit 1; }
    cp "$src" "$dst"
    sha256sum "$dst" > "${dst}.sha256"
done

info "Staged binaries:"
ls -lh "${STAGE}/"

# ── 3. stop daemon on remote ──────────────────────────────────────────────────
info "Stopping remote daemon (if running)..."
ssh "${SSH_HOST}" '
    if pgrep -f remi-daemon-linux-x86_64 > /dev/null 2>&1; then
        pkill -f remi-daemon-linux-x86_64 || true
        sleep 1
        echo "daemon stopped"
    else
        echo "daemon was not running"
    fi
'

# ── 4. upload binaries ────────────────────────────────────────────────────────
info "Uploading binaries to ${SSH_HOST}:${REMOTE_BIN}/ ..."
ssh "${SSH_HOST}" "mkdir -p '${REMOTE_BIN}'"
scp "${STAGE}"/* "${SSH_HOST}:${REMOTE_BIN}/"
success "Upload complete"

# ── 5. rebuild Docker image with new agent binary ─────────────────────────────
if [[ $SKIP_DOCKER -eq 0 ]]; then
    info "Rebuilding Docker image with local agent binary..."
    ssh "${SSH_HOST}" "
        cd '${REMOTE_DIR}' || exit 1

        # Build image from local binary (no network download)
        docker build -t remi-cat:latest -f- . <<'DOCKERFILE'
FROM debian:bookworm-slim
RUN apt-get update && apt-get install -y --no-install-recommends ca-certificates && rm -rf /var/lib/apt/lists/*
COPY bin/remi-cat-agent-linux-x86_64 /usr/local/bin/remi-cat-agent
RUN chmod +x /usr/local/bin/remi-cat-agent
VOLUME [\"/app/data\"]
WORKDIR /app/data
ENTRYPOINT [\"/usr/local/bin/remi-cat-agent\"]
DOCKERFILE

        # Restart container
        docker compose down 2>/dev/null || true
        docker compose up -d
        echo 'container restarted'
    "
    success "Docker image rebuilt and container restarted"
fi

# ── 6. restart daemon ─────────────────────────────────────────────────────────
info "Starting daemon..."
ssh "${SSH_HOST}" "
    cd '${REMOTE_DIR}'
    chmod +x bin/remi-daemon-linux-x86_64
    nohup bin/remi-daemon-linux-x86_64 > daemon.log 2>&1 &
    sleep 2
    if pgrep -f remi-daemon-linux-x86_64 > /dev/null; then
        echo 'daemon started OK'
    else
        echo 'ERROR: daemon failed to start'
        tail -20 daemon.log
        exit 1
    fi
"

# ── 7. summary ────────────────────────────────────────────────────────────────
echo
success "Deploy complete!"
info  "Running processes:"
ssh "${SSH_HOST}" 'ps aux | grep remi | grep -v grep || true'
