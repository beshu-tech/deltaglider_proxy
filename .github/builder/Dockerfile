FROM ubuntu:24.04

LABEL org.opencontainers.image.source="https://github.com/beshu-tech/deltaglider_proxy"
LABEL org.opencontainers.image.description="CI builder for DeltaGlider Proxy — Rust + Node + xdelta3 + Docker CLI"

ENV DEBIAN_FRONTEND=noninteractive

# ── System packages ─────────────────────────────────────────────────
RUN apt-get update && apt-get install -y --no-install-recommends \
    build-essential \
    ca-certificates \
    curl \
    git \
    jq \
    libssl-dev \
    pkg-config \
    wget \
    xdelta3 \
  && rm -rf /var/lib/apt/lists/*

# ── Docker CLI only (not the full daemon — we use the host socket) ──
RUN curl -fsSL https://download.docker.com/linux/static/stable/x86_64/docker-27.5.1.tgz \
    | tar xz --strip-components=1 -C /usr/local/bin docker/docker

# ── Node.js 20 (LTS) via NodeSource ────────────────────────────────
RUN curl -fsSL https://deb.nodesource.com/setup_20.x | bash - \
  && apt-get install -y --no-install-recommends nodejs \
  && rm -rf /var/lib/apt/lists/*

# ── Rust stable via rustup (minimal profile — no docs) ─────────────
ENV RUSTUP_HOME=/usr/local/rustup \
    CARGO_HOME=/usr/local/cargo \
    PATH="/usr/local/cargo/bin:${PATH}"

RUN curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs \
    | sh -s -- -y --default-toolchain stable --profile minimal \
  && rustup component add rustfmt clippy \
  && rm -rf "$RUSTUP_HOME"/toolchains/*/share/doc \
  && rm -rf "$RUSTUP_HOME"/toolchains/*/share/man

# ── Cargo tools (baked in, zero runtime install) ───────────────────
RUN cargo install cargo-audit --locked \
  && cargo install cargo-sbom --locked \
  && rm -rf "$CARGO_HOME"/registry "$CARGO_HOME"/git

# ── MinIO client (mc) ──────────────────────────────────────────────
RUN curl -fsSL https://dl.min.io/client/mc/release/linux-amd64/mc -o /usr/local/bin/mc \
  && chmod +x /usr/local/bin/mc

# ── Verify everything is in place ──────────────────────────────────
RUN echo "--- Builder image verification ---" \
  && node --version \
  && npm --version \
  && rustc --version \
  && cargo --version \
  && rustfmt --version \
  && cargo clippy --version \
  && cargo audit --version \
  && cargo sbom --help | head -1 \
  && xdelta3 -V \
  && mc --version \
  && docker --version \
  && git --version \
  && echo "--- All tools OK ---"
