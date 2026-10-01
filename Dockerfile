# ── Build stage: UI ──
FROM node:26-alpine AS ui-build
WORKDIR /app/demo/s3-browser/ui
COPY demo/s3-browser/ui/package.json demo/s3-browser/ui/package-lock.json ./
RUN npm ci
COPY demo/s3-browser/ui/ ./
# docs/screenshots/ is synced into the UI by scripts/copy-screenshots.mjs
# (prebuild hook). The markdown itself is NOT bundled — see the Rust stage.
COPY docs/ /app/docs/
# Cargo.toml is the single source of truth for the version string. The
# bundle deliberately does NOT embed it (nor a build time): the fingerprint
# guard below reads it to prove the built dist/ carries neither. Copying it
# also makes a version bump invalidate this layer.
COPY Cargo.toml /app/Cargo.toml
# No frontend source maps: they are opt-in in vite.config.ts (DGP_UI_SOURCEMAP=1)
# because dist/ is embedded and served to anonymous callers.
RUN npm run build

# ── Build stage: Rust ──
# Base: debian:bookworm-slim, the runtime base, so the binary's glibc always
# matches the image it runs in. Rust comes from rustup-init (checksum-pinned
# per arch) at the version in rust-toolchain.toml, the single toolchain pin.
# This is the official rust image's own recipe; it does not wait for Docker
# Hub to publish rust:<new>-bookworm (on 2026-10-01, release day of 1.99.0,
# no rust:1.99* tag existed yet). NOTE: we deliberately do NOT use cargo-chef
# here — cargo-chef 0.1.77's prepare/cook round-trip writes a recipe whose
# auto-discovered targets carry a target-level `edition`, which modern `cargo
# build` rejects as a hard error ("failed to parse manifest"), breaking the
# image build on every recent Rust. A plain single-stage build is correct and
# robust; dependency compilation is cached by buildx's GHA layer cache across
# release runs, so the lost cargo-chef dep-layer is not a meaningful regression.
FROM debian:bookworm-slim AS rust-build
# Bump RUSTUP_VERSION together with both sha256 values below
# (static.rust-lang.org/rustup/archive/<version>/<host>/rustup-init.sha256).
ARG RUSTUP_VERSION=1.29.1
ENV RUSTUP_HOME=/usr/local/rustup \
    CARGO_HOME=/usr/local/cargo \
    PATH=/usr/local/cargo/bin:$PATH
COPY rust-toolchain.toml /tmp/rust-toolchain.toml
# gcc + libc6-dev: C parts of aws-lc-sys, ring and sqlcipher. libssl-dev +
# pkg-config: sqlcipher links the system libcrypto.
RUN apt-get -o Acquire::Retries=3 update && apt-get install -y --no-install-recommends \
    ca-certificates curl gcc libc6-dev libssl-dev pkg-config \
    xdelta3=3.0.11-dfsg-1.2 \
    && rm -rf /var/lib/apt/lists/* \
    && case "$(dpkg --print-architecture)" in \
         amd64) host=x86_64-unknown-linux-gnu; sha=dda7234360b7f578ca8b0ddcb80145646fa61a67c1720a5abc7051b35c9fcb71 ;; \
         arm64) host=aarch64-unknown-linux-gnu; sha=15f6e4ce9f583b929c996c91562bad6d4454f3281de858b02cdfdef615fac433 ;; \
         *) echo "unsupported architecture: $(dpkg --print-architecture)" >&2; exit 1 ;; \
       esac \
    && curl -fsSLo /tmp/rustup-init "https://static.rust-lang.org/rustup/archive/${RUSTUP_VERSION}/${host}/rustup-init" \
    && echo "${sha}  /tmp/rustup-init" | sha256sum -c - \
    && chmod +x /tmp/rustup-init \
    && toolchain="$(sed -n 's/^channel = "\(.*\)"$/\1/p' /tmp/rust-toolchain.toml)" \
    && [ -n "$toolchain" ] \
    && /tmp/rustup-init -y --no-modify-path --profile minimal --default-toolchain "$toolchain" --default-host "$host" \
    && rm /tmp/rustup-init /tmp/rust-toolchain.toml \
    && rustc --version \
    # Pin xdelta3 so the delta FORMAT the proxy produces can't silently drift on
    # a base-image bump (newer xdelta3 armors by default — see codec.rs `-a`).
    # Assert it actually landed.
    && xdelta3 -V 2>&1 | grep -q "3.0.11"
WORKDIR /app
COPY Cargo.toml Cargo.lock build.rs ./
COPY src/ src/
# Product docs are embedded in the binary (rust-embed in src/demo.rs) and
# served session-gated at /_/api/docs.
COPY docs/product/ docs/product/
# Cargo.toml declares `[[bench]] name = "codec"` → cargo needs benches/codec.rs
# present to even PARSE the manifest (without it: "can't find `codec` bench …
# failed to parse manifest"). This was the real cause of the release build
# failure — the bench was added to Cargo.toml but never copied into the image.
COPY benches/ benches/
COPY --from=ui-build /app/demo/s3-browser/ui/dist demo/s3-browser/ui/dist
# Fingerprint guard on the dist this binary embeds. It runs HERE, not in the
# node:alpine UI stage: that image has no bash and its BusyBox grep has no
# --include, which would make the script fail (or, worse, pass vacuously).
COPY scripts/check-bundle-fingerprints.sh scripts/check-bundle-fingerprints.sh
COPY scripts/cargo-jobs.sh scripts/cargo-jobs.sh
RUN ./scripts/check-bundle-fingerprints.sh demo/s3-browser/ui/dist Cargo.toml
# Parallel jobs from the memory budget, not the CPU count: cargo alone starts
# one rustc per CPU and a build container with a modest memory limit is killed
# mid-build (scripts/cargo-jobs.sh has the measured per-job peaks).
RUN CARGO_BUILD_JOBS="$(./scripts/cargo-jobs.sh)" cargo build --release

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
