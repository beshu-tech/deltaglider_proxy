FROM ubuntu:24.04

LABEL org.opencontainers.image.source="https://github.com/beshu-tech/deltaglider_proxy"
LABEL org.opencontainers.image.description="CI builder for DeltaGlider Proxy — Rust + Node + xdelta3 + Docker CLI"

ENV DEBIAN_FRONTEND=noninteractive

# ── System packages ─────────────────────────────────────────────────
# clang + lld are required by .cargo/config.toml which pins
# `linker = "clang"` with `-fuse-ld=lld` for ~3-5× faster linking.
RUN apt-get update && apt-get install -y --no-install-recommends \
    build-essential \
    ca-certificates \
    clang \
    curl \
    git \
    jq \
    libssl-dev \
    lld \
    pkg-config \
    wget \
    xdelta3=3.0.11-dfsg-1.2 \
  && rm -rf /var/lib/apt/lists/*
# Pin xdelta3 to the SAME version the runtime image (Dockerfile) ships, so the
# integration tests validate the exact delta format prod produces — and adding
# `-a` to codec.rs (armor-proofing for newer xdelta3) is exercised on the pinned
# binary. The roundtrip + legacy-fixture codec tests are the cross-version net.

# ── Docker CLI only (not the full daemon — we use the host socket) ──
RUN curl -fsSL https://download.docker.com/linux/static/stable/x86_64/docker-27.5.1.tgz \
    | tar xz --strip-components=1 -C /usr/local/bin docker/docker

# ── Node.js 26 via NodeSource ──────────────────────────────────────
RUN curl -fsSL https://deb.nodesource.com/setup_26.x | bash - \
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
# cargo-llvm-cov (coverage), cargo-deny (supply-chain), and lychee (docs
# link check) were previously `cargo install`ed from source on every CI run.
# Bake them so the nightly coverage/supply-chain jobs and the docs job pay
# zero install cost.
RUN cargo install cargo-audit --locked \
  && cargo install cargo-sbom --locked \
  && cargo install cargo-llvm-cov --locked \
  && cargo install cargo-deny --locked \
  && cargo install lychee --locked \
  && cargo install sccache --locked \
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
  && cargo llvm-cov --version \
  && cargo deny --version \
  && lychee --version \
  && sccache --version \
  && xdelta3 -V \
  && (xdelta3 -V 2>&1 | grep -q "3.0.11" || (echo "FATAL: xdelta3 not pinned to 3.0.11" && exit 1)) \
  && mc --version \
  && docker --version \
  && git --version \
  && clang --version \
  && ld.lld --version \
  && echo "--- All tools OK ---"
