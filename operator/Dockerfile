# Build from the repository root: docker build -f operator/Dockerfile operator/
# Builder MUST stay on bookworm: distroless/cc-debian12 ships glibc 2.36, and a
# trixie-built binary demands GLIBC_2.39 and dies at exec (caught live on 0.2.1).
FROM rust:1.97-slim-bookworm AS builder
WORKDIR /build
COPY . .
RUN cargo build --release --locked

FROM gcr.io/distroless/cc-debian12:nonroot
COPY --from=builder /build/target/release/deltaglider-operator /deltaglider-operator
USER nonroot
ENTRYPOINT ["/deltaglider-operator"]
