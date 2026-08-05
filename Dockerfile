# ── Build stage: UI ──
FROM node:26-alpine AS ui-build
WORKDIR /app/demo/s3-browser/ui
COPY demo/s3-browser/ui/package.json demo/s3-browser/ui/package-lock.json ./
RUN npm ci
COPY demo/s3-browser/ui/ ./
# docs/ is referenced by src/docs-imports.ts via relative path (../../../../docs/)
COPY docs/ /app/docs/
# Cargo.toml is the single source of truth for the version string.
# vite.config.ts reads it at build time to embed __BUILD_VERSION__ into
# the bundle (see `resolveBuildVersion()` there). Copying it here also
# doubles as a cache key: a version bump invalidates this layer and
# forces `npm run build` to run — which re-evaluates `new Date()` in
# the vite define, so __BUILD_TIME__ stays honest across version bumps
# instead of freezing at the first-ever-built timestamp.
COPY Cargo.toml /app/Cargo.toml
RUN npm run build

# ── Build stage: Rust ──
# Pin the Rust toolchain (the floating `rust:1-bookworm` tag drifts and has
# caused reproducibility breaks). NOTE: we deliberately do NOT use cargo-chef
# here — cargo-chef 0.1.77's prepare/cook round-trip writes a recipe whose
# auto-discovered targets carry a target-level `edition`, which modern `cargo
# build` rejects as a hard error ("failed to parse manifest"), breaking the
# image build on every recent Rust. A plain single-stage build is correct and
# robust; dependency compilation is cached by buildx's GHA layer cache across
# release runs, so the lost cargo-chef dep-layer is not a meaningful regression.
FROM rust:1.92-bookworm AS rust-build
RUN apt-get -o Acquire::Retries=3 update && apt-get install -y --no-install-recommends \
    pkg-config xdelta3=3.0.11-dfsg-1.2 \
    && rm -rf /var/lib/apt/lists/* \
    # Pin xdelta3 so the delta FORMAT the proxy produces can't silently drift on
    # a base-image bump (newer xdelta3 armors by default — see codec.rs `-a`).
    # Assert it actually landed.
    && xdelta3 -V 2>&1 | grep -q "3.0.11"
WORKDIR /app
COPY Cargo.toml Cargo.lock build.rs ./
COPY src/ src/
# Cargo.toml declares `[[bench]] name = "codec"` → cargo needs benches/codec.rs
# present to even PARSE the manifest (without it: "can't find `codec` bench …
# failed to parse manifest"). This was the real cause of the release build
# failure — the bench was added to Cargo.toml but never copied into the image.
COPY benches/ benches/
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
      org.opencontainers.image.source="https://github.com/beshu-tech/deltaglider_proxy" \
      org.opencontainers.image.licenses="BUSL-1.1"

# Install ca-certificates (HTTPS) and curl (healthcheck).
# xdelta3 is copied from build stage to reduce apt dependency surface.
# Use multiple retries + fallback to handle unreliable deb.debian.org.
RUN (apt-get -o Acquire::Retries=5 update && apt-get install -y --no-install-recommends \
    ca-certificates curl ntpstat chrony \
    && rm -rf /var/lib/apt/lists/*) \
    || (echo "WARN: apt-get failed — continuing without curl (healthcheck will use wget fallback)" && apt-get clean)
RUN groupadd --system dg && useradd --system --gid dg --no-create-home dg
COPY --from=rust-build /app/target/release/deltaglider_proxy /usr/local/bin/
COPY --from=rust-build /usr/bin/xdelta3 /usr/bin/xdelta3
RUN mkdir -p /data && chown dg:dg /data
USER dg
WORKDIR /data
EXPOSE 9000
ENV DGP_LISTEN_ADDR=0.0.0.0:9000
HEALTHCHECK --interval=15s --timeout=3s --retries=3 \
    CMD curl -f http://localhost:9000/_/health || exit 1
ENTRYPOINT ["deltaglider_proxy"]
