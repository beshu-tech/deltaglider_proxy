# ── Build stage: UI ──
FROM node:22-alpine AS ui-build
WORKDIR /app/demo/s3-browser/ui
COPY demo/s3-browser/ui/package.json demo/s3-browser/ui/package-lock.json ./
RUN npm ci
COPY demo/s3-browser/ui/ ./
RUN npm run build

# ── Build stage: cargo-chef plan (captures dependency graph) ──
FROM rust:1-bookworm AS chef
RUN cargo install cargo-chef --locked
WORKDIR /app

FROM chef AS planner
COPY Cargo.toml Cargo.lock build.rs ./
COPY src/ src/
RUN cargo chef prepare --recipe-path recipe.json

# ── Build stage: dependency cache (only reruns when recipe.json changes) ──
FROM chef AS rust-deps
RUN apt-get update && apt-get install -y --no-install-recommends \
    pkg-config \
    && rm -rf /var/lib/apt/lists/*
COPY --from=planner /app/recipe.json recipe.json
# Cook dependencies — this layer is cached until Cargo.toml/lock changes
RUN mkdir -p demo/s3-browser/ui/dist \
    && cargo chef cook --release --recipe-path recipe.json

# ── Build stage: Rust ──
FROM rust-deps AS rust-build
COPY Cargo.toml Cargo.lock build.rs ./
COPY src/ src/
COPY --from=ui-build /app/demo/s3-browser/ui/dist demo/s3-browser/ui/dist
RUN cargo build --release

# ── Runtime ──
# Security notes:
# - Runs as non-root user 'dg' (least privilege).
# - Only ca-certificates, xdelta3, and curl are installed (minimal attack surface).
#   curl is required for the HEALTHCHECK probe; no shell utilities beyond busybox.
# - No secrets are embedded in the image — all credentials are provided at runtime
#   via environment variables or mounted config files.
#
# Kubernetes / container orchestrator hardening (apply in your deployment manifest):
#   securityContext:
#     runAsNonRoot: true
#     readOnlyRootFilesystem: true
#     allowPrivilegeEscalation: false
#     capabilities:
#       drop: [ALL]
#   # Mount a writable volume for the config DB and data directory:
#   volumeMounts:
#     - name: data
#       mountPath: /data
#     - name: tmp
#       mountPath: /tmp
FROM debian:bookworm-slim

LABEL org.opencontainers.image.title="DeltaGlider Proxy" \
      org.opencontainers.image.description="S3-compatible proxy with transparent delta compression" \
      org.opencontainers.image.vendor="DeltaGlider" \
      org.opencontainers.image.source="https://github.com/sscarduzio/deltaglider-proxy" \
      org.opencontainers.image.licenses="MIT"

RUN apt-get update && apt-get install -y --no-install-recommends \
    ca-certificates xdelta3 curl \
    && rm -rf /var/lib/apt/lists/*
RUN groupadd --system dg && useradd --system --gid dg --no-create-home dg
COPY --from=rust-build /app/target/release/deltaglider_proxy /usr/local/bin/
USER dg
EXPOSE 9000
ENV DGP_LISTEN_ADDR=0.0.0.0:9000
HEALTHCHECK --interval=15s --timeout=3s --retries=3 \
    CMD curl -f http://localhost:9000/health || exit 1
ENTRYPOINT ["deltaglider_proxy"]
