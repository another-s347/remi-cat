#!/usr/bin/env bash
# setup.sh — Interactive install & setup for remi-cat
#
# What this script does:
#   1. Checks / installs prerequisites (Rust, protobuf-compiler, pkg-config,
#      Docker, Docker Compose)
#   2. Asks which deployment mode you want
#        a) Standalone  — single binary, no Docker required
#        b) Daemon+Agent — recommended production setup with Docker
#   3. Configures network ports (daemon mode)
#   4. Builds the required Rust binaries
#   5. Guides you through all non-secret configuration (env vars, container
#      name, data directory)
#   6. Configures Docker bind mounts / volumes (daemon mode)
#   7. Prompts for API secrets with hidden input (no echo)
#   8. Writes .env with all configuration + secrets
#   9. (Daemon mode) Runs `remi-daemon init-env` to import secrets into the
#      AES-256-GCM encrypted secret store and strip them from .env
#  10. (Daemon mode) Generates docker-compose.override.yml for any extra
#      bind mounts
#
# Usage:
#   chmod +x setup.sh && ./setup.sh

set -euo pipefail

# ── colour helpers ─────────────────────────────────────────────────────────────
RED='\033[0;31m'
GREEN='\033[0;32m'
YELLOW='\033[1;33m'
CYAN='\033[0;36m'
BOLD='\033[1m'
RESET='\033[0m'

info()    { echo -e "${CYAN}[INFO]${RESET}  $*"; }
success() { echo -e "${GREEN}[OK]${RESET}    $*"; }
warn()    { echo -e "${YELLOW}[WARN]${RESET}  $*"; }
error()   { echo -e "${RED}[ERROR]${RESET} $*" >&2; }
header()  { echo -e "\n${BOLD}${CYAN}══ $* ══${RESET}"; }
# ask() writes to /dev/tty so prompts are visible even inside $() substitution
ask()     { printf "${BOLD}${YELLOW}  ➜ ${RESET}%s " "$*" > /dev/tty; }

# ── prompt helpers ─────────────────────────────────────────────────────────────

# prompt_yn <question> [default_y|default_n]
# Returns 0 (yes) or 1 (no)
prompt_yn() {
    local question="$1"
    local default="${2:-default_y}"
    local hint
    if [[ "$default" == "default_n" ]]; then
        hint="[y/N]"
    else
        hint="[Y/n]"
    fi
    while true; do
        ask "${question} ${hint}:"
        read -r answer < /dev/tty
        answer="${answer:-}"
        if [[ -z "$answer" ]]; then
            [[ "$default" == "default_n" ]] && return 1 || return 0
        fi
        case "$answer" in
            [Yy]*) return 0 ;;
            [Nn]*) return 1 ;;
            *)     warn "Please answer y or n." ;;
        esac
    done
}

# prompt_value <question> <default>
# Echoes the entered (or default) value
prompt_value() {
    local question="$1"
    local default="$2"
    local value
    ask "${question} [${default}]:"
    read -r value < /dev/tty
    echo "${value:-$default}"
}

# prompt_optional <question>
# Echoes the entered value, or empty string if the user pressed Enter
prompt_optional() {
    local question="$1"
    local value
    ask "${question} (leave blank to skip):"
    read -r value < /dev/tty
    echo "${value:-}"
}

# prompt_secret <question>
# Prompts for a required secret with no echo. Retries until non-empty.
# Echoes the entered value (all UI goes to /dev/tty so $() capture is clean).
prompt_secret() {
    local question="$1"
    local value
    while true; do
        ask "${question}:"
        read -rs value < /dev/tty
        printf '\n' > /dev/tty
        if [[ -n "$value" ]]; then
            echo "$value"
            return 0
        fi
        printf "${YELLOW}[WARN]${RESET}  Value cannot be empty. Try again.\n" > /dev/tty
    done
}

# prompt_secret_optional <question>
# Prompts for an optional secret with no echo. Returns empty if skipped.
prompt_secret_optional() {
    local question="$1"
    local value
    ask "${question} (leave blank to skip):"
    read -rs value < /dev/tty
    printf '\n' > /dev/tty
    echo "${value:-}"
}

# ── OS detection ───────────────────────────────────────────────────────────────
detect_os() {
    if [[ -f /etc/os-release ]]; then
        # shellcheck source=/dev/null
        . /etc/os-release
        echo "${ID:-linux}"
    elif [[ "$(uname)" == "Darwin" ]]; then
        echo "darwin"
    else
        echo "unknown"
    fi
}

OS=$(detect_os)

# ── package manager helpers ────────────────────────────────────────────────────
apt_install() {
    sudo apt-get update -qq
    sudo apt-get install -y "$@"
}

brew_install() {
    brew install "$@"
}

install_pkg() {
    local pkg="$1"
    info "Installing ${pkg} …"
    case "$OS" in
        ubuntu|debian) apt_install "$pkg" ;;
        darwin)        brew_install "$pkg" ;;
        *)
            error "Cannot auto-install ${pkg} on OS '${OS}'."
            error "Please install it manually and re-run this script."
            exit 1
            ;;
    esac
}

# ── prerequisite checks ────────────────────────────────────────────────────────
check_rust() {
    header "Rust toolchain"
    if command -v rustc &>/dev/null && command -v cargo &>/dev/null; then
        success "Rust is already installed ($(rustc --version))"
        return
    fi
    warn "Rust is not installed."
    if prompt_yn "Install Rust via rustup?"; then
        info "Running rustup installer …"
        curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y
        # shellcheck source=/dev/null
        source "${HOME}/.cargo/env"
        success "Rust installed ($(rustc --version))"
    else
        error "Rust is required. Aborting."
        exit 1
    fi
}

check_protoc() {
    header "protobuf-compiler (protoc)"
    if command -v protoc &>/dev/null; then
        success "protoc is already installed ($(protoc --version))"
        return
    fi
    warn "protoc is not installed."
    if prompt_yn "Install protobuf-compiler?"; then
        case "$OS" in
            ubuntu|debian) apt_install protobuf-compiler ;;
            darwin)        brew_install protobuf ;;
            *)
                error "Cannot auto-install protoc on OS '${OS}'. Please install it manually."
                exit 1
                ;;
        esac
        success "protoc installed ($(protoc --version))"
    else
        error "protoc is required to build the project. Aborting."
        exit 1
    fi
}

check_pkg_config() {
    header "pkg-config"
    if command -v pkg-config &>/dev/null; then
        success "pkg-config is already installed"
        return
    fi
    warn "pkg-config is not installed."
    if prompt_yn "Install pkg-config?"; then
        case "$OS" in
            ubuntu|debian) apt_install pkg-config ;;
            darwin)        brew_install pkg-config ;;
            *)
                error "Cannot auto-install pkg-config on OS '${OS}'. Please install it manually."
                exit 1
                ;;
        esac
        success "pkg-config installed"
    else
        error "pkg-config is required to build the project. Aborting."
        exit 1
    fi
}

check_openssl() {
    header "OpenSSL development libraries"
    # libssl-dev / openssl on different distros
    if pkg-config --exists openssl 2>/dev/null || \
       [[ -f /usr/include/openssl/ssl.h ]] || \
       [[ "$(uname)" == "Darwin" ]]; then
        success "OpenSSL dev headers found"
        return
    fi
    warn "OpenSSL development headers not found."
    if prompt_yn "Install libssl-dev?"; then
        case "$OS" in
            ubuntu|debian) apt_install libssl-dev ;;
            darwin)        brew_install openssl ;;
            *)
                error "Cannot auto-install libssl-dev on OS '${OS}'. Please install it manually."
                exit 1
                ;;
        esac
        success "libssl-dev installed"
    else
        error "OpenSSL dev headers are required to build the project. Aborting."
        exit 1
    fi
}

check_docker() {
    header "Docker"
    if command -v docker &>/dev/null; then
        success "Docker is already installed ($(docker --version))"
    else
        warn "Docker is not installed."
        echo "  Please install Docker Desktop (macOS/Windows) or Docker Engine (Linux):"
        echo "  https://docs.docker.com/engine/install/"
        if ! prompt_yn "Continue without Docker? (only Standalone mode will be available)"; then
            exit 1
        fi
        DOCKER_AVAILABLE=false
        return
    fi

    if command -v docker compose &>/dev/null 2>&1 || \
       docker compose version &>/dev/null 2>&1; then
        success "Docker Compose plugin is available"
        DOCKER_COMPOSE_CMD="docker compose"
    elif command -v docker-compose &>/dev/null; then
        success "docker-compose (standalone) is available"
        DOCKER_COMPOSE_CMD="docker-compose"
    else
        warn "Docker Compose not found."
        echo "  Please install the Docker Compose plugin:"
        echo "  https://docs.docker.com/compose/install/"
        if ! prompt_yn "Continue without Docker Compose? (only Standalone mode will be available)"; then
            exit 1
        fi
        DOCKER_AVAILABLE=false
        return
    fi

    DOCKER_AVAILABLE=true
}

# ── deployment mode selection ─────────────────────────────────────────────────
choose_deployment_mode() {
    header "Deployment mode"
    echo "  a) Standalone  — single binary (remi-cat), no Docker required"
    echo "                   Good for local testing."
    echo "  b) Daemon+Agent — remi-daemon on the host + remi-cat-agent in Docker"
    echo "                   Recommended for production."
    if [[ "${DOCKER_AVAILABLE:-true}" == "false" ]]; then
        warn "Docker is not available — only Standalone mode is possible."
        DEPLOY_MODE="standalone"
        return
    fi
    while true; do
        ask "Choose mode [a/b] (default: b):"
        read -r mode_choice < /dev/tty
        mode_choice="${mode_choice:-b}"
        case "$mode_choice" in
            a|A|standalone) DEPLOY_MODE="standalone"; break ;;
            b|B|daemon)     DEPLOY_MODE="daemon";     break ;;
            *) warn "Please enter 'a' or 'b'." ;;
        esac
    done
    success "Selected: ${DEPLOY_MODE}"
}

# ── port configuration (daemon mode only) ─────────────────────────────────────
configure_ports() {
    [[ "$DEPLOY_MODE" != "daemon" ]] && return

    header "Port configuration"

    echo
    info "The daemon exposes two TCP ports on the host:"
    echo "  • gRPC  — agent connects here for message relay (default 50051)"
    echo "  • Mgmt  — admin WebSocket API (default 50052)"
    echo

    DAEMON_GRPC_PORT=$(prompt_value "Daemon gRPC port" "50051")
    DAEMON_GRPC_ADDR="0.0.0.0:${DAEMON_GRPC_PORT}"

    DAEMON_MGMT_PORT=$(prompt_value "Daemon management port" "50052")
    DAEMON_MGMT_ADDR="0.0.0.0:${DAEMON_MGMT_PORT}"

    # Address the agent container uses to reach the daemon on the host.
    # Default uses host.docker.internal which resolves to the host gateway.
    DAEMON_ADDR=$(prompt_value \
        "Daemon gRPC address as seen from inside the agent container" \
        "http://host.docker.internal:${DAEMON_GRPC_PORT}")

    success "Ports — gRPC: ${DAEMON_GRPC_PORT}, mgmt: ${DAEMON_MGMT_PORT}"
}

# ── install mode selection ─────────────────────────────────────────────────────
choose_install_mode() {
    header "Installation mode"
    echo "  a) Download pre-built release binary  (recommended for production)"
    echo "  b) Build from source                  (for development / custom builds)"
    while true; do
        ask "Choose install mode [a/b] (default: a):"
        read -r _mode_choice < /dev/tty
        _mode_choice="${_mode_choice:-a}"
        case "$_mode_choice" in
            a|A|release) INSTALL_MODE="release"; break ;;
            b|B|source)  INSTALL_MODE="source";  break ;;
            *) warn "Please enter 'a' or 'b'." ;;
        esac
    done
    success "Install mode: ${INSTALL_MODE}"
}

# ── download pre-built release binaries ───────────────────────────────────────
download_binaries() {
    [[ "${INSTALL_MODE:-source}" != "release" ]] && return

    header "Downloading release binaries"

    if ! command -v curl &>/dev/null && ! command -v wget &>/dev/null; then
        die "Neither curl nor wget found — cannot download binaries.  Install one and retry."
    fi

    local base_url
    if [[ -n "${RELEASE_TAG:-}" && "${RELEASE_TAG}" != "latest" ]]; then
        base_url="https://github.com/${GITHUB_REPO}/releases/download/${RELEASE_TAG}"
    else
        base_url="https://github.com/${GITHUB_REPO}/releases/latest/download"
    fi

    local bin_dir="${PWD}/bin"
    mkdir -p "$bin_dir"

    _dl() {
        local url="$1" dest="$2"
        info "  ↓ ${url}"
        if command -v curl &>/dev/null; then
            curl -fL --progress-bar -o "$dest" "$url" || die "Download failed: ${url}"
        else
            wget -q --show-progress -O "$dest" "$url" || die "Download failed: ${url}"
        fi
        chmod +x "$dest"
    }

    case "$DEPLOY_MODE" in
        standalone)
            _dl "${base_url}/remi-cat-linux-x86_64" "${bin_dir}/remi-cat"
            STANDALONE_BIN="${bin_dir}/remi-cat"
            success "Downloaded: ${STANDALONE_BIN}"
            DAEMON_BIN=""
            ADMIN_BIN=""
            ;;
        daemon)
            _dl "${base_url}/remi-daemon-linux-x86_64" "${bin_dir}/remi-daemon"
            DAEMON_BIN="${bin_dir}/remi-daemon"
            success "Downloaded: ${DAEMON_BIN}"

            if prompt_yn "Also download remi-admin (admin WebUI)?"; then
                _dl "${base_url}/remi-admin-linux-x86_64" "${bin_dir}/remi-admin"
                ADMIN_BIN="${bin_dir}/remi-admin"
                success "Downloaded: ${ADMIN_BIN}"
            else
                ADMIN_BIN=""
            fi
            STANDALONE_BIN=""
            ;;
    esac
}

# ── build binaries ─────────────────────────────────────────────────────────────
build_binaries() {
    [[ "${INSTALL_MODE:-source}" != "source" ]] && return

    header "Building Rust binaries"

    # Ensure .cargo/env is sourced so cargo is on PATH after fresh rustup install
    if [[ -f "${HOME}/.cargo/env" ]]; then
        # shellcheck source=/dev/null
        source "${HOME}/.cargo/env"
    fi

    case "$DEPLOY_MODE" in
        standalone)
            info "Building remi-cat (standalone binary) …"
            cargo build --release
            DAEMON_BIN=""
            STANDALONE_BIN="$(pwd)/target/release/remi-cat"
            success "Built: ${STANDALONE_BIN}"
            ;;
        daemon)
            info "Building remi-daemon …"
            cargo build --release -p remi-daemon
            DAEMON_BIN="$(pwd)/target/release/remi-daemon"
            success "Built: ${DAEMON_BIN}"

            if prompt_yn "Also build the admin UI (remi-admin)?"; then
                info "Building remi-admin …"
                cargo build --release -p remi-admin
                ADMIN_BIN="$(pwd)/target/release/remi-admin"
                success "Built: ${ADMIN_BIN}"
            else
                ADMIN_BIN=""
            fi
            ;;
    esac
}

# ── release / self-update configuration ──────────────────────────────────────
configure_updates() {
    header "Release & self-update configuration"
    echo
    info "remi-daemon supports a /update command that downloads and replaces itself."
    info "These settings are also used to pull the Docker image tag at build time."
    echo

    GITHUB_REPO=$(prompt_value "GitHub repository (owner/repo)" "another-s347/remi-cat")
    RELEASE_TAG=$(prompt_value "Release tag to track (used for docker compose build; 'latest' = follow latest)" "latest")

    echo
    info "By default the daemon constructs the update URL from GITHUB_REPO."
    info "Set an explicit URL below only if you host binaries elsewhere."
    DAEMON_UPDATE_URL=$(prompt_optional "Explicit daemon binary download URL (leave blank to use GITHUB_REPO)")
}

# ── non-secret configuration ───────────────────────────────────────────────────
configure() {
    header "Configuration (non-secret settings)"

    echo
    info "API keys (FEISHU credentials, OPENAI_API_KEY, EXA_API_KEY) are collected"
    info "separately and imported into the encrypted secret store."
    echo

    # OpenAI model
    OPENAI_MODEL=$(prompt_value "OpenAI-compatible model name" "gpt-4o")

    # OpenAI base URL
    OPENAI_BASE_URL=$(prompt_secret_optional "OpenAI-compatible API base URL (e.g. https://api.openai.com/v1)")

    # Log level
    if [[ "$DEPLOY_MODE" == "daemon" ]]; then
        DEFAULT_LOG="remi_cat_agent=info,bot_core=info,remi_daemon=info"
    else
        DEFAULT_LOG="remi_cat=info,bot_core=info"
    fi
    RUST_LOG=$(prompt_value "Log level (RUST_LOG)" "$DEFAULT_LOG")

    # Owner pre-configuration
    REMI_CAT_OWNER_ID=$(prompt_optional "Feishu owner open_id (e.g. ou_xxxxxxxx) — skips /pair on first boot")

    if [[ "$DEPLOY_MODE" == "daemon" ]]; then
        # Container / data dir settings
        AGENT_CONTAINER=$(prompt_value "Docker container name for the agent" "remi-cat")
        REMI_DATA_DIR=$(prompt_value "Daemon data directory (host path)" ".remi-cat")
    else
        REMI_DATA_DIR=$(prompt_value "Bot data directory" ".remi-cat")
        AGENT_CONTAINER=""
    fi
}

# ── volume / bind-mount configuration (daemon mode only) ─────────────────────
configure_volumes() {
    [[ "$DEPLOY_MODE" != "daemon" ]] && return

    header "Docker volume / bind-mount configuration"

    echo
    info "The agent container already uses a named Docker volume:"
    info "  remi-cat-data  →  /app/data  (persists memory, skills, owner data)"
    echo
    info "You can add extra host bind mounts to share files with the agent,"
    info "e.g. a local documents folder the agent can read/write."
    echo

    VOLUME_MOUNTS=()   # array of "host_path:container_path:ro_flag" strings

    while prompt_yn "Add an extra bind mount?" "default_n"; do
        local host_path=""
        while true; do
            host_path=$(prompt_optional "Host path (absolute, e.g. /home/user/docs)")
            if [[ -z "$host_path" ]]; then
                warn "Host path cannot be empty."; continue
            fi
            if [[ "$host_path" != /* ]]; then
                warn "Host path must be absolute (start with /)."; continue
            fi
            break
        done

        local container_path=""
        while true; do
            container_path=$(prompt_optional "Container path (absolute, e.g. /app/docs)")
            if [[ -z "$container_path" ]]; then
                warn "Container path cannot be empty."; continue
            fi
            if [[ "$container_path" != /* ]]; then
                warn "Container path must be absolute (start with /)."; continue
            fi
            break
        done

        local ro_flag=""
        if prompt_yn "Mount read-only?" "default_n"; then
            ro_flag=":ro"
        fi

        VOLUME_MOUNTS+=("${host_path}:${container_path}${ro_flag}")
        success "Bind mount queued: ${host_path} → ${container_path}${ro_flag:+ (read-only)}"
    done

    if [[ ${#VOLUME_MOUNTS[@]} -gt 0 ]]; then
        success "${#VOLUME_MOUNTS[@]} extra bind mount(s) configured"
    else
        info "No extra bind mounts added"
    fi
}

# ── secret credential input ────────────────────────────────────────────────────
input_secrets() {
    header "Secret credentials (input hidden)"

    echo
    if [[ "$DEPLOY_MODE" == "daemon" ]]; then
        info "Secrets will be written to .env, then imported into the daemon's"
        info "AES-256-GCM encrypted secret store via 'remi-daemon init-env',"
        info "after which they are stripped from .env so they are never stored"
        info "in plaintext on disk."
    else
        warn "Standalone mode: secrets will remain in .env (plaintext)."
        warn "Keep .env protected (chmod 600) and never commit it to git."
    fi
    echo

    FEISHU_APP_ID=$(prompt_secret     "Feishu App ID      (e.g. cli_xxxxxxxxxxxxxxxx)")
    FEISHU_APP_SECRET=$(prompt_secret "Feishu App Secret")
    OPENAI_API_KEY=$(prompt_secret    "OpenAI API key     (e.g. sk-...)")
    EXA_API_KEY=$(prompt_secret_optional "Exa API key (for web-search tool)")

    success "Secrets captured (values are not displayed)"
}

# ── write / update .env ────────────────────────────────────────────────────────
write_env() {
    header "Writing .env"

    local env_file=".env"

    # Protect the file immediately: secrets will be written here temporarily.
    touch "$env_file"
    chmod 600 "$env_file"

    cat > "$env_file" <<EOF
# remi-cat configuration — generated by setup.sh
# ────────────────────────────────────────────────────────────────────────────
EOF

    # ── Secrets section ───────────────────────────────────────────────────
    if [[ "$DEPLOY_MODE" == "daemon" ]]; then
        cat >> "$env_file" <<EOF
# ── Feishu / LLM credentials (imported into secret store by init-env) ────────
# These lines are temporary — 'remi-daemon init-env' will strip them after
# importing them into the AES-256-GCM encrypted secret store.
EOF
    else
        cat >> "$env_file" <<EOF
# ── Feishu / LLM credentials ─────────────────────────────────────────────────
# WARNING: standalone mode — secrets remain in this file (plaintext).
# Run: chmod 600 .env   and never commit this file to version control.
EOF
    fi

    cat >> "$env_file" <<EOF
FEISHU_APP_ID=${FEISHU_APP_ID}
FEISHU_APP_SECRET=${FEISHU_APP_SECRET}
OPENAI_API_KEY=${OPENAI_API_KEY}
EOF

    if [[ -n "${OPENAI_BASE_URL:-}" ]]; then
        echo "OPENAI_BASE_URL=${OPENAI_BASE_URL}" >> "$env_file"
    else
        echo "# OPENAI_BASE_URL=" >> "$env_file"
    fi

    cat >> "$env_file" <<EOF
OPENAI_MODEL=${OPENAI_MODEL}
EOF

    if [[ -n "${EXA_API_KEY:-}" ]]; then
        echo "EXA_API_KEY=${EXA_API_KEY}" >> "$env_file"
    else
        echo "# EXA_API_KEY=" >> "$env_file"
    fi

    # ── Non-secret runtime config ─────────────────────────────────────────
    cat >> "$env_file" <<EOF

# ── Log level ────────────────────────────────────────────────────────────────
RUST_LOG=${RUST_LOG}
EOF

    # Owner ID
    if [[ -n "${REMI_CAT_OWNER_ID:-}" ]]; then
        cat >> "$env_file" <<EOF

# ── Owner pre-configuration ──────────────────────────────────────────────────
REMI_CAT_OWNER_ID=${REMI_CAT_OWNER_ID}
EOF
    else
        cat >> "$env_file" <<EOF

# ── Owner pre-configuration (optional) ──────────────────────────────────────
# REMI_CAT_OWNER_ID=
EOF
    fi

    # Data dir
    cat >> "$env_file" <<EOF

# ── Data directory ───────────────────────────────────────────────────────────
REMI_DATA_DIR=${REMI_DATA_DIR}
EOF
    # Standalone: record Agent.md path (outside the agent's data_dir sandbox)
    if [[ "$DEPLOY_MODE" == "standalone" ]]; then
        cat >> "$env_file" <<EOF

# ── Agent.md path (read-only for the agent; edit directly on the host) ──────────
AGENT_MD_PATH=./Agent.md
EOF
    fi
    # Daemon-specific settings
    if [[ "$DEPLOY_MODE" == "daemon" ]]; then
        cat >> "$env_file" <<EOF

# ── Daemon network (daemon+agent mode) ──────────────────────────────────────
DAEMON_ADDR=${DAEMON_ADDR}
DAEMON_GRPC_ADDR=${DAEMON_GRPC_ADDR}
DAEMON_MGMT_ADDR=${DAEMON_MGMT_ADDR}

# ── Docker agent container name ──────────────────────────────────────────────
AGENT_CONTAINER=${AGENT_CONTAINER}
EOF
    fi

    # ── Self-update / release tracking ────────────────────────────────────
    cat >> "$env_file" <<EOF

# ── Release & self-update ────────────────────────────────────────────────────
# GITHUB_REPO is used by both remi-daemon (/update command) and docker compose
# (GITHUB_REPO build arg in docker-compose.yml).
GITHUB_REPO=${GITHUB_REPO:-another-s347/remi-cat}
# RELEASE_TAG controls which image tag docker compose uses for the build arg.
RELEASE_TAG=${RELEASE_TAG:-latest}
EOF
    if [[ -n "${DAEMON_UPDATE_URL:-}" ]]; then
        echo "# Explicit override URL for remi-daemon binary downloads:" >> "$env_file"
        echo "DAEMON_UPDATE_URL=${DAEMON_UPDATE_URL}" >> "$env_file"
    else
        echo "# DAEMON_UPDATE_URL=  # leave blank to auto-construct from GITHUB_REPO" >> "$env_file"
    fi

    success ".env written (mode 600) → ${env_file}"
}

# ── generate docker-compose.override.yml (daemon + extra volumes only) ────────
write_compose_override() {
    [[ "$DEPLOY_MODE" != "daemon" ]] && return
    [[ ${#VOLUME_MOUNTS[@]} -eq 0 ]] && return

    header "Writing docker-compose.override.yml"

    local override_file="docker-compose.override.yml"

    {
        echo "# Auto-generated by setup.sh — extra bind mounts requested during setup."
        echo "# Docker Compose merges this file automatically with docker-compose.yml."
        echo "services:"
        echo "  remi-cat:"
        echo "    volumes:"
        for mount in "${VOLUME_MOUNTS[@]}"; do
            echo "      - \"${mount}\""
        done
    } > "$override_file"

    success "docker-compose.override.yml written with ${#VOLUME_MOUNTS[@]} extra bind mount(s)"
    for mount in "${VOLUME_MOUNTS[@]}"; do
        info "  bind: ${mount}"
    done
}

# ── import secrets into the encrypted secret store (daemon mode only) ─────────
init_secret_store() {
    [[ "$DEPLOY_MODE" != "daemon" ]] && return

    header "Initializing encrypted secret store"

    if [[ -z "${DAEMON_BIN:-}" || ! -x "${DAEMON_BIN}" ]]; then
        warn "Daemon binary not found — skipping automatic secret import."
        warn "After the build completes, run manually:"
        warn "  ${DAEMON_BIN:-./target/release/remi-daemon} init-env"
        return
    fi

    info "Running: remi-daemon init-env"
    info "(imports FEISHU_APP_ID, FEISHU_APP_SECRET, OPENAI_API_KEY, OPENAI_MODEL,"
    info " OPENAI_BASE_URL, EXA_API_KEY from .env into the encrypted store,"
    info " then strips those lines from .env)"
    echo

    if "${DAEMON_BIN}" init-env; then
        success "Secrets sealed into encrypted store — credential lines removed from .env"
    else
        warn "Secret store initialization returned an error."
        warn "Your secrets remain in .env.  Re-run manually when ready:"
        warn "  ${DAEMON_BIN} init-env"
    fi
}

# ── initialise Agent.md ───────────────────────────────────────────────
init_agent_md() {
    header "Agent.md initialisation"
    echo

    if [[ "$DEPLOY_MODE" == "daemon" ]]; then
        # Daemon mode: Agent.md lives at <data_dir>/Agent.md on the host and is
        # bind-mounted into the container as /app/config/Agent.md (read-only).
        # The agent cannot write to it; only the host / admin UI can.
        local agent_file="${REMI_DATA_DIR:-".remi-cat"}/Agent.md"
        mkdir -p "$(dirname "$agent_file")"
        if [[ -f "$agent_file" ]]; then
            info "${agent_file} already exists — skipping initialisation."
        else
            info "Creating ${agent_file} (bind-mounted read-only into the container)."
            info "Edit it on the host or via the admin UI. The agent cannot modify it."
            echo
            if prompt_yn "Write a default Agent.md now?" "default_n"; then
                cat > "$agent_file" <<'AGENT'
# Agent

<!-- Edit this file to define the agent's core instructions, capabilities, and rules. -->
<!-- Only the host operator or admin UI can change this file. -->
AGENT
                success "${agent_file} created"
            else
                touch "$agent_file"
                info "Empty ${agent_file} created (bind mount requires the file to exist)."
            fi
        fi
    else
        # Standalone mode: Agent.md lives at ./Agent.md (outside data_dir sandbox).
        # Set AGENT_MD_PATH in .env so the bot reads from here, not data_dir/Agent.md.
        local agent_file="./Agent.md"
        if [[ -f "$agent_file" ]]; then
            info "./Agent.md already exists — skipping initialisation."
        else
            info "Creating ./Agent.md outside the agent's data directory sandbox."
            info "The agent's file tools cannot reach this path."
            echo
            if prompt_yn "Write a default Agent.md now?" "default_n"; then
                cat > "$agent_file" <<'AGENT'
# Agent

<!-- Edit this file to define the agent's core instructions, capabilities, and rules. -->
<!-- This file can only be edited directly on the host (not by the agent). -->
AGENT
                success "./Agent.md created"
            else
                touch "$agent_file"
                info "Empty ./Agent.md created."
            fi
        fi
    fi
}

# ── initialise soul.md (daemon mode) ─────────────────────────────────────────
init_soul_md() {
    local soul_file="soul.md"

    # Standalone: data_dir/Soul.md is the target; for daemon it's ./soul.md
    # which is bind-mounted into the container at /app/data/Soul.md.
    if [[ "$DEPLOY_MODE" == "daemon" ]]; then
        if [[ -f "$soul_file" ]]; then
            info "soul.md already exists — skipping initialisation."
            return
        fi
        header "Soul.md initialisation"
        echo
        info "soul.md will be bind-mounted into the agent container at /app/data/Soul.md."
        info "The agent can edit it with its file tools; changes take effect immediately."
        info "You can also edit it directly on the host at any time."
        echo
        if prompt_yn "Write a default soul.md now?" "default_n"; then
            cat > "$soul_file" <<'SOUL'
# Soul

<!-- Edit this file to shape the agent's personality, values, and long-term goals. -->
<!-- Changes take effect on the next message — no restart needed. -->
SOUL
            success "soul.md created"
        else
            # Create an empty file so the bind mount target exists.
            touch "$soul_file"
            info "Empty soul.md created (bind mount requires the file to exist on the host)."
        fi
    else
        # Standalone: soul.md lives at data_dir/Soul.md
        local standalone_soul="${REMI_DATA_DIR}/Soul.md"
        if [[ -f "$standalone_soul" ]]; then
            info "${standalone_soul} already exists — skipping initialisation."
            return
        fi
        header "Soul.md initialisation"
        echo
        info "Soul.md will be stored at ${standalone_soul}."
        info "The agent can edit it with its file tools; changes take effect on the next message."
        echo
        if prompt_yn "Write a default Soul.md now?" "default_n"; then
            mkdir -p "${REMI_DATA_DIR}"
            cat > "$standalone_soul" <<'SOUL'
# Soul

<!-- Edit this file to shape the agent's personality, values, and long-term goals. -->
<!-- Changes take effect on the next message — no restart needed. -->
SOUL
            success "${standalone_soul} created"
        fi
    fi
}

# ── offer to start services now ───────────────────────────────────────────────
offer_start_services() {
    header "Start services"

    case "$DEPLOY_MODE" in
        standalone)
            if ! prompt_yn "Start the bot now?"; then
                return
            fi
            info "Sourcing .env and starting remi-cat …"
            set -a
            # shellcheck source=/dev/null
            source .env
            set +a
            exec "${STANDALONE_BIN}"
            ;;
        daemon)
            if ! prompt_yn "Start the daemon now?"; then
                return
            fi
            info "Sourcing .env …"
            set -a
            # shellcheck source=/dev/null
            source .env
            set +a

            info "Starting remi-daemon in the background …"
            "${DAEMON_BIN}" &
            DAEMON_PID=$!
            success "remi-daemon started (pid: ${DAEMON_PID})"

            # Give the daemon a moment to bind its gRPC port before the agent connects.
            sleep 1

            if prompt_yn "Also start the agent container now?"; then
                info "Running: ${DOCKER_COMPOSE_CMD:-docker compose} up -d"
                ${DOCKER_COMPOSE_CMD:-docker compose} up -d
                success "Agent container started"
            fi

            if [[ -n "${ADMIN_BIN:-}" ]]; then
                if prompt_yn "Start the admin UI now?"; then
                    info "Starting remi-admin in the background …"
                    "${ADMIN_BIN}" &
                    success "remi-admin started"
                fi
            fi
            ;;
    esac
}

# ── final instructions ─────────────────────────────────────────────────────────
print_next_steps() {
    header "Setup complete — next steps"
    echo

    case "$DEPLOY_MODE" in
        standalone)
            warn "Secrets remain in .env (plaintext — standalone mode)."
            echo "  Ensure: chmod 600 .env"
            echo
            echo -e "${BOLD}Run standalone bot:${RESET}"
            echo "  1. Load config:"
            echo "       set -a; source .env; set +a"
            echo "  2. Start the bot:"
            echo "       ${STANDALONE_BIN}"
            ;;
        daemon)
            success "Secrets have been sealed into the encrypted store."
            info   ".env now contains only non-sensitive runtime config."
            echo
            info "soul.md (host) is bind-mounted into the container at /app/data/Soul.md."
            info "Edit it on the host or let the agent edit it — changes are real-time."
            echo
            echo -e "${BOLD}Run daemon + agent:${RESET}"
            echo "  1. Load runtime config (non-secret):"
            echo "       set -a; source .env; set +a"
            echo "  2. Start the daemon on the host:"
            echo "       ${DAEMON_BIN}"
            echo "  3. Start the agent container:"
            echo "       ${DOCKER_COMPOSE_CMD:-docker compose} up -d"
            if [[ -n "${ADMIN_BIN:-}" ]]; then
                echo "  4. (Optional) Start the admin UI:"
                echo "       ${ADMIN_BIN}    # Runs on http://localhost:8770"
            fi
            echo
            echo -e "${BOLD}Secret store commands:${RESET}"
            echo "  List stored keys    :  ${DAEMON_BIN} secrets list"
            echo "  Add/update a secret :  ${DAEMON_BIN} secrets set KEY VALUE"
            echo "  Remove a secret     :  ${DAEMON_BIN} secrets delete KEY"
            echo "  Re-import from .env :  ${DAEMON_BIN} init-env"
            echo
            if [[ ${#VOLUME_MOUNTS[@]} -gt 0 ]]; then
                echo -e "${BOLD}Extra bind mounts (docker-compose.override.yml):${RESET}"
                for mount in "${VOLUME_MOUNTS[@]}"; do
                    echo "  ${mount}"
                done
                echo
            fi
            echo "  Ports:"
            echo "    gRPC   (host): ${DAEMON_GRPC_ADDR}"
            echo "    Mgmt   (host): ${DAEMON_MGMT_ADDR}"
            echo "    Agent → daemon: ${DAEMON_ADDR}"
            ;;
    esac

    echo
    success "Done! 🎉"
}

# ── main ───────────────────────────────────────────────────────────────────────
main() {
    echo
    echo -e "${BOLD}${CYAN}╔══════════════════════════════════════════╗${RESET}"
    echo -e "${BOLD}${CYAN}║       remi-cat  —  Interactive Setup     ║${RESET}"
    echo -e "${BOLD}${CYAN}╚══════════════════════════════════════════╝${RESET}"
    echo

    # Docker check first (determines whether daemon mode is available)
    check_docker

    # Mode selection
    choose_deployment_mode

    # Install mode: download release vs build from source
    choose_install_mode

    # Build prerequisites — only needed when compiling from source
    if [[ "${INSTALL_MODE}" == "source" ]]; then
        check_rust
        check_protoc
        check_pkg_config
        check_openssl
    fi

    # Port configuration (daemon mode — before build, no binary needed)
    configure_ports

    # Release / self-update configuration (sets GITHUB_REPO, RELEASE_TAG, DAEMON_UPDATE_URL)
    configure_updates

    # Download pre-built release binaries (if release mode)
    download_binaries

    # Build from source (if source mode)
    build_binaries

    # Non-secret env vars
    configure

    # Docker volumes / bind mounts (daemon mode)
    configure_volumes

    # Secret credentials (hidden input)
    input_secrets

    # Write .env (secrets included temporarily)
    write_env

    # Seal secrets into encrypted store, strip from .env (daemon mode)
    init_secret_store

    # docker-compose.override.yml for extra bind mounts (daemon mode)
    write_compose_override

    # Initialise Agent.md (admin-only) and Soul.md (agent-editable)
    init_agent_md
    init_soul_md

    # Offer to start services immediately
    offer_start_services

    # Print final instructions
    print_next_steps
}

main "$@"
