# Dockerfile for remi-cat-agent (runs inside Docker as an isolation sandbox).
# The binary is downloaded from GitHub Releases — no Rust toolchain needed.
#
# Build args:
#   GITHUB_REPO   — owner/repo  (default: another-s347/remi-cat)
#   RELEASE_TAG   — vX.Y.Z or "latest"  (default: latest)
#
# Examples:
#   docker build .                                    # pull latest release
#   docker build --build-arg RELEASE_TAG=v0.2.0 .    # pin a specific version

FROM debian:bookworm-slim

ARG GITHUB_REPO=another-s347/remi-cat
ARG RELEASE_TAG=latest

RUN apt-get update && apt-get install -y --no-install-recommends \
    ca-certificates \
    curl \
    && rm -rf /var/lib/apt/lists/*

# Resolve the download base URL depending on whether a specific tag was given.
# "latest" → .../releases/latest/download/
# "vX.Y.Z" → .../releases/download/vX.Y.Z/
RUN set -eux; \
    if [ "${RELEASE_TAG}" = "latest" ]; then \
    BASE_URL="https://github.com/${GITHUB_REPO}/releases/latest/download"; \
    else \
    BASE_URL="https://github.com/${GITHUB_REPO}/releases/download/${RELEASE_TAG}"; \
    fi; \
    BIN="remi-cat-agent-linux-x86_64"; \
    curl -fsSL "${BASE_URL}/${BIN}"        -o /usr/local/bin/remi-cat-agent; \
    curl -fsSL "${BASE_URL}/${BIN}.sha256" -o /tmp/remi-cat-agent.sha256; \
    # Verify checksum (sha256sum file format: "<hash>  <filename>") \
    EXPECTED=$(awk '{print $1}' /tmp/remi-cat-agent.sha256); \
    ACTUAL=$(sha256sum /usr/local/bin/remi-cat-agent | awk '{print $1}'); \
    if [ "${EXPECTED}" != "${ACTUAL}" ]; then \
    echo "SHA256 mismatch: expected ${EXPECTED}, got ${ACTUAL}"; \
    exit 1; \
    fi; \
    chmod +x /usr/local/bin/remi-cat-agent; \
    rm /tmp/remi-cat-agent.sha256

VOLUME ["/app/data"]
WORKDIR /app/data

ENTRYPOINT ["/usr/local/bin/remi-cat-agent"]

