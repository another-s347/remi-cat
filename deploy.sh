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
    cargo build --release -p remi-cat-agent -p remi-daemon -p remi-admin \
        2>&1 | grep -E "^error|Compiling remi|Finished"
    success "Build complete"
else
    info "Skipping build (using existing target/release binaries)"
fi

# ── 2. stage and encode ───────────────────────────────────────────────────────
STAGE=$(mktemp -d)
trap 'rm -rf "${STAGE}"' EXIT

cd "${SCRIPT_DIR}"
for bin in remi-cat-agent remi-daemon remi-admin; do
    src="target/release/${bin}"
    dst="${STAGE}/${bin}-linux-x86_64"
    [[ -f "$src" ]] || { error "Binary not found: ${src}"; exit 1; }
    cp "$src" "$dst"
done

info "Encoding binaries for transfer (this may take a moment)..."
TAR_B64=$(tar -C "${STAGE}" -czf - . | base64 | tr -d '\n')
ENCODED_KB=$(( ${#TAR_B64} / 1024 ))
success "Encoded ${ENCODED_KB} KB — ready to transfer"

COMPOSE_B64=$(base64 < "${SCRIPT_DIR}/docker-compose.yml" | tr -d '\n')
SOUL_B64=""
if [[ -f "${SCRIPT_DIR}/soul.md" ]]; then
    SOUL_B64=$(base64 < "${SCRIPT_DIR}/soul.md" | tr -d '\n')
fi
AGENT_MD_B64=""
REMI_DATA_DIR="${REMI_DATA_DIR:-.remi-cat}"
if [[ -f "${SCRIPT_DIR}/${REMI_DATA_DIR}/Agent.md" ]]; then
    AGENT_MD_B64=$(base64 < "${SCRIPT_DIR}/${REMI_DATA_DIR}/Agent.md" | tr -d '\n')
fi

# ── 3-6. Single SSH session: upload + all remote steps ───────────────────────
info "Connecting to ${SSH_HOST} (you will be prompted for password once)..."

ssh -o StrictHostKeyChecking=accept-new "${SSH_HOST}" bash << REMOTESCRIPT
set -euo pipefail

REMOTE_DIR='${REMOTE_DIR}'
REMOTE_BIN='${REMOTE_BIN}'
SKIP_DOCKER='${SKIP_DOCKER}'
DAEMON_BIN="\${REMOTE_BIN}/remi-daemon-linux-x86_64"
REMI_DATA_DIR='${REMI_DATA_DIR}'

# ── extract binaries ─────────────────────────────────────────────────────────
echo "==> Extracting binaries..."
mkdir -p "\${REMOTE_BIN}"
base64 -d << '__REMI_TAR_EOF__' | tar -xzf - -C "\${REMOTE_BIN}"
${TAR_B64}
__REMI_TAR_EOF__
chmod +x "\${REMOTE_BIN}"/*-linux-x86_64
ls -lh "\${REMOTE_BIN}/"

# ── write support files ──────────────────────────────────────────────────────
echo "==> Writing docker-compose.yml..."
mkdir -p "\${REMOTE_DIR}"
base64 -d << '__COMPOSE_EOF__' > "\${REMOTE_DIR}/docker-compose.yml"
${COMPOSE_B64}
__COMPOSE_EOF__

if [[ -n '${SOUL_B64}' ]]; then
    echo "==> Writing soul.md..."
    base64 -d << '__SOUL_EOF__' > "\${REMOTE_DIR}/soul.md"
${SOUL_B64}
__SOUL_EOF__
fi

mkdir -p "\${REMOTE_DIR}/\${REMI_DATA_DIR}"
if [[ -n '${AGENT_MD_B64}' ]]; then
    echo "==> Writing Agent.md..."
    base64 -d << '__AGENT_MD_EOF__' > "\${REMOTE_DIR}/\${REMI_DATA_DIR}/Agent.md"
${AGENT_MD_B64}
__AGENT_MD_EOF__
fi

touch "\${REMOTE_DIR}/soul.md"
touch "\${REMOTE_DIR}/\${REMI_DATA_DIR}/Agent.md"

# ── stop existing daemon ─────────────────────────────────────────────────────
echo "==> Stopping daemon..."
if pgrep -xf "\${DAEMON_BIN}" > /dev/null 2>&1; then
    pkill -xf "\${DAEMON_BIN}" || true
    sleep 2
    echo "    daemon stopped"
else
    echo "    daemon was not running"
fi

# ── rebuild Docker image ─────────────────────────────────────────────────────
if [[ "\${SKIP_DOCKER}" == "0" ]]; then
    echo "==> Rebuilding Docker image..."
    cd "\${REMOTE_DIR}"
    docker build -t remi-cat:latest -f- . << 'DOCKERFILE'
FROM debian:bookworm-slim
RUN apt-get update && apt-get install -y --no-install-recommends ca-certificates && rm -rf /var/lib/apt/lists/*
COPY bin/remi-cat-agent-linux-x86_64 /usr/local/bin/remi-cat-agent
RUN chmod +x /usr/local/bin/remi-cat-agent
VOLUME ["/app/data"]
WORKDIR /app/data
ENTRYPOINT ["/usr/local/bin/remi-cat-agent"]
DOCKERFILE
    echo "==> Restarting container..."
    docker compose down 2>/dev/null || true
    docker compose up -d
    echo "    container restarted"
fi

# ── start daemon ─────────────────────────────────────────────────────────────
echo "==> Starting daemon..."
cd "\${REMOTE_DIR}"
if [[ -f .env ]]; then
    set -o allexport
    source .env
    set +o allexport
fi
nohup "\${DAEMON_BIN}" >> daemon.log 2>&1 &
disown
sleep 3
if pgrep -xf "\${DAEMON_BIN}" > /dev/null 2>&1; then
    echo "    daemon started OK"
else
    echo "ERROR: daemon failed to start — last log lines:"
    tail -30 daemon.log
    exit 1
fi

echo "==> Running processes:"
ps aux | grep remi | grep -v grep || true
REMOTESCRIPT

echo
success "Deploy complete!"

