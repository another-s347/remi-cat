#!/usr/bin/env bash
# setup.sh — Interactive install & setup for remi-cat
#
# What this script does:
#   1. Checks / installs prerequisites (Rust, protobuf-compiler, pkg-config,
#      Docker, Docker Compose)
#   2. Asks which deployment mode you want
#        a) Standalone  — single binary, no Docker required
#        b) Daemon+Agent — recommended production setup with Docker
#   3. Builds the required Rust binaries
#   4. Guides you through all non-secret configuration
#   5. Generates / updates .env with placeholders for API keys
#      (FEISHU_APP_ID, FEISHU_APP_SECRET, OPENAI_API_KEY are NOT set here;
#       they will be injected from your secret store later)
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
ask()     { echo -en "${BOLD}${YELLOW}  ➜ ${RESET}$* "; }

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
        read -r answer
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
    ask "${question} [${default}]:"
    read -r value
    echo "${value:-$default}"
}

# prompt_optional <question>
# Echoes the entered value, or empty string if the user pressed Enter
prompt_optional() {
    local question="$1"
    ask "${question} (leave blank to skip):"
    read -r value
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
        read -r mode_choice
        mode_choice="${mode_choice:-b}"
        case "$mode_choice" in
            a|A|standalone) DEPLOY_MODE="standalone"; break ;;
            b|B|daemon)     DEPLOY_MODE="daemon";     break ;;
            *) warn "Please enter 'a' or 'b'." ;;
        esac
    done
    success "Selected: ${DEPLOY_MODE}"
}

# ── build binaries ─────────────────────────────────────────────────────────────
build_binaries() {
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

# ── non-secret configuration ───────────────────────────────────────────────────
configure() {
    header "Configuration (non-secret settings)"

    echo
    info "API keys (FEISHU_APP_ID, FEISHU_APP_SECRET, OPENAI_API_KEY) are NOT"
    info "configured here — they will be injected from your secret store later."
    echo

    # OpenAI model
    OPENAI_MODEL=$(prompt_value "OpenAI-compatible model name" "gpt-4o")

    # OpenAI base URL
    OPENAI_BASE_URL=$(prompt_optional "OpenAI-compatible API base URL (e.g. https://api.openai.com/v1)")

    # Log level
    if [[ "$DEPLOY_MODE" == "daemon" ]]; then
        DEFAULT_LOG="remi_cat_agent=info,bot_core=info"
    else
        DEFAULT_LOG="remi_cat=info"
    fi
    RUST_LOG=$(prompt_value "Log level (RUST_LOG)" "$DEFAULT_LOG")

    # Owner pre-configuration
    REMI_CAT_OWNER_ID=$(prompt_optional "Feishu owner open_id (e.g. ou_xxxxxxxx) — skips /pair on first boot")

    # Daemon gRPC address (daemon mode only)
    if [[ "$DEPLOY_MODE" == "daemon" ]]; then
        DAEMON_ADDR=$(prompt_value "Daemon gRPC address (seen by the agent container)" "http://host.docker.internal:50051")
        DAEMON_GRPC_BIND=$(prompt_value "Daemon gRPC bind address" "0.0.0.0:50051")
    fi
}

# ── write / update .env ────────────────────────────────────────────────────────
write_env() {
    header "Writing .env"

    local env_file=".env"

    cat > "$env_file" <<EOF
# remi-cat configuration — generated by setup.sh
# ────────────────────────────────────────────────────────────────────────────
# !! SECRET KEYS BELOW ARE LEFT AS PLACEHOLDERS !!
# Inject real values from your secret store before running the service.
# ────────────────────────────────────────────────────────────────────────────

# ── Feishu Enterprise App (SECRET — fill in from secret store) ───────────────
FEISHU_APP_ID=cli_xxxxxxxxxxxxxxxx
FEISHU_APP_SECRET=xxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxx

# ── OpenAI-compatible LLM (SECRET — fill in from secret store) ───────────────
OPENAI_API_KEY=sk-xxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxx
EOF

    # Optional base URL
    if [[ -n "$OPENAI_BASE_URL" ]]; then
        echo "OPENAI_BASE_URL=${OPENAI_BASE_URL}" >> "$env_file"
    else
        echo "# OPENAI_BASE_URL=" >> "$env_file"
    fi

    cat >> "$env_file" <<EOF

# ── Model ────────────────────────────────────────────────────────────────────
OPENAI_MODEL=${OPENAI_MODEL}

# ── Log level ────────────────────────────────────────────────────────────────
RUST_LOG=${RUST_LOG}
EOF

    # Owner ID
    if [[ -n "$REMI_CAT_OWNER_ID" ]]; then
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

    # Daemon-specific settings
    if [[ "$DEPLOY_MODE" == "daemon" ]]; then
        cat >> "$env_file" <<EOF

# ── Daemon (daemon+agent mode) ───────────────────────────────────────────────
DAEMON_ADDR=${DAEMON_ADDR}
DAEMON_GRPC_BIND=${DAEMON_GRPC_BIND}
EOF
    fi

    success ".env written to ${env_file}"
}

# ── final instructions ─────────────────────────────────────────────────────────
print_next_steps() {
    header "Setup complete — next steps"

    echo
    warn "Before starting the service, inject your secret API keys into .env:"
    echo "  • FEISHU_APP_ID"
    echo "  • FEISHU_APP_SECRET"
    echo "  • OPENAI_API_KEY"
    echo

    case "$DEPLOY_MODE" in
        standalone)
            echo -e "${BOLD}Run standalone bot:${RESET}"
            echo "  1. Export your secrets:"
            echo "       set -a; source .env; set +a"
            echo "  2. Start the bot:"
            echo "       ${STANDALONE_BIN}"
            ;;
        daemon)
            echo -e "${BOLD}Run daemon + agent:${RESET}"
            echo "  1. Export your secrets (for the daemon on the host):"
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
            echo "  Docker image will be built from the Dockerfile and pulls"
            echo "  the latest remi-cat-agent binary from GitHub Releases."
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

    # Prerequisites
    check_rust
    check_protoc
    check_pkg_config
    check_openssl
    check_docker

    # Mode selection
    choose_deployment_mode

    # Build
    build_binaries

    # Configure
    configure

    # Write .env
    write_env

    # Print final instructions
    print_next_steps
}

main "$@"
