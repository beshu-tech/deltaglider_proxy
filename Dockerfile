# ── Build stage: UI ──
FROM node:22-alpine AS ui-build
WORKDIR /app/demo/s3-browser/ui
COPY demo/s3-browser/ui/package.json demo/s3-browser/ui/package-lock.json ./
RUN npm ci
COPY demo/s3-browser/ui/ ./
RUN npm run build

# ── Build stage: Rust ──
FROM rust:1-bookworm AS rust-build
RUN apt-get update && apt-get install -y --no-install-recommends \
    clang libclang-dev pkg-config \
    && rm -rf /var/lib/apt/lists/*
WORKDIR /app
COPY Cargo.toml Cargo.lock ./
COPY src/ src/
COPY --from=ui-build /app/demo/s3-browser/ui/dist demo/s3-browser/ui/dist
RUN cargo build --release

# ── Runtime ──
FROM debian:bookworm-slim
RUN apt-get update && apt-get install -y --no-install-recommends \
    ca-certificates xdelta3 \
    && rm -rf /var/lib/apt/lists/*
COPY --from=rust-build /app/target/release/deltaglider_proxy /usr/local/bin/
EXPOSE 9000 9001
ENV DGP_LISTEN_ADDR=0.0.0.0:9000
ENTRYPOINT ["deltaglider_proxy"]
