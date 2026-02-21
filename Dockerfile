# ── Build stage: UI ──
FROM node:22-alpine AS ui-build
WORKDIR /app/demo/s3-browser/ui
COPY demo/s3-browser/ui/package.json demo/s3-browser/ui/package-lock.json ./
RUN npm ci
COPY demo/s3-browser/ui/ ./
RUN npm run build

# ── Build stage: Rust dependency cache ──
FROM rust:1-bookworm AS rust-deps
RUN apt-get update && apt-get install -y --no-install-recommends \
    pkg-config \
    && rm -rf /var/lib/apt/lists/*
WORKDIR /app
COPY Cargo.toml Cargo.lock ./
# Dummy sources to compile dependencies only (cached until Cargo.toml/lock change)
RUN mkdir -p src && echo 'fn main() {}' > src/main.rs && touch src/lib.rs \
    && mkdir -p demo/s3-browser/ui/dist \
    && cargo build --release \
    && rm -rf src/

# ── Build stage: Rust ──
FROM rust-deps AS rust-build
COPY src/ src/
COPY --from=ui-build /app/demo/s3-browser/ui/dist demo/s3-browser/ui/dist
# Remove all dummy crate artifacts so cargo fully rebuilds our code
RUN rm -f target/release/deltaglider_proxy \
           target/release/deps/deltaglider_proxy-* \
           target/release/deps/libdeltaglider_proxy-* \
    && rm -rf target/release/.fingerprint/deltaglider_proxy-* \
    && cargo build --release

# ── Runtime ──
FROM debian:bookworm-slim
RUN apt-get update && apt-get install -y --no-install-recommends \
    ca-certificates xdelta3 curl \
    && rm -rf /var/lib/apt/lists/*
RUN groupadd --system dg && useradd --system --gid dg --no-create-home dg
COPY --from=rust-build /app/target/release/deltaglider_proxy /usr/local/bin/
USER dg
EXPOSE 9000 9001
ENV DGP_LISTEN_ADDR=0.0.0.0:9000
HEALTHCHECK --interval=15s --timeout=3s --retries=3 \
    CMD curl -f http://localhost:9000/health || exit 1
ENTRYPOINT ["deltaglider_proxy"]
