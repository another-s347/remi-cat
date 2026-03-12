# Dockerfile for remi-cat-agent (runs inside Docker).
# remi-daemon runs on the host and is built separately.

# ── Stage 1: Builder ──────────────────────────────────────────────────────────
FROM rust:1.85-slim AS builder

RUN apt-get update && apt-get install -y --no-install-recommends \
    pkg-config \
    libssl-dev \
    ca-certificates \
    protobuf-compiler \
    && rm -rf /var/lib/apt/lists/*

WORKDIR /build

# ── Dependency layer (cache-friendly) ────────────────────────────────────────
COPY Cargo.toml Cargo.lock ./
COPY crates/im-gateway/Cargo.toml  crates/im-gateway/Cargo.toml
COPY crates/im-feishu/Cargo.toml   crates/im-feishu/Cargo.toml
COPY crates/matcher/Cargo.toml     crates/matcher/Cargo.toml
COPY crates/bot-core/Cargo.toml    crates/bot-core/Cargo.toml
COPY crates/proto/Cargo.toml       crates/proto/Cargo.toml
COPY crates/daemon/Cargo.toml      crates/daemon/Cargo.toml
COPY crates/agent/Cargo.toml       crates/agent/Cargo.toml
# Proto file is needed at build time for tonic-build.
COPY crates/proto/proto/           crates/proto/proto/
COPY crates/proto/build.rs         crates/proto/build.rs
COPY crates/daemon/build.rs        crates/daemon/build.rs 2>/dev/null || true

RUN mkdir -p src \
    crates/im-gateway/src \
    crates/im-feishu/src \
    crates/matcher/src \
    crates/bot-core/src \
    crates/proto/src \
    crates/daemon/src \
    crates/agent/src \
    && echo 'fn main() {}' > src/main.rs \
    && echo '' > crates/im-gateway/src/lib.rs \
    && echo '' > crates/im-feishu/src/lib.rs \
    && echo '' > crates/matcher/src/lib.rs \
    && echo '' > crates/bot-core/src/lib.rs \
    && echo '' > crates/proto/src/lib.rs \
    && echo 'fn main() {}' > crates/daemon/src/main.rs \
    && echo 'fn main() {}' > crates/agent/src/main.rs

RUN cargo build --release --bin remi-cat-agent 2>&1 | tail -5 || true

# ── Full source build ─────────────────────────────────────────────────────────
COPY src/           src/
COPY crates/im-gateway/src/  crates/im-gateway/src/
COPY crates/im-feishu/src/   crates/im-feishu/src/
COPY crates/matcher/src/     crates/matcher/src/
COPY crates/bot-core/src/    crates/bot-core/src/
COPY crates/proto/src/       crates/proto/src/
COPY crates/daemon/src/      crates/daemon/src/
COPY crates/agent/src/       crates/agent/src/

RUN touch crates/im-gateway/src/lib.rs \
    crates/im-feishu/src/lib.rs \
    crates/matcher/src/lib.rs \
    crates/bot-core/src/lib.rs \
    crates/proto/src/lib.rs \
    crates/agent/src/main.rs

RUN cargo build --release --bin remi-cat-agent

# ── Stage 2: Runtime ──────────────────────────────────────────────────────────
FROM debian:bookworm-slim AS runtime

RUN apt-get update && apt-get install -y --no-install-recommends \
    ca-certificates \
    && rm -rf /var/lib/apt/lists/*

WORKDIR /app

COPY --from=builder /build/target/release/remi-cat-agent /app/remi-cat-agent

VOLUME ["/app/data"]
WORKDIR /app/data

ENTRYPOINT ["/app/remi-cat-agent"]

