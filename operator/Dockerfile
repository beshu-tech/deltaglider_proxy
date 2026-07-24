# Build from the repository root: docker build -f operator/Dockerfile operator/
FROM rust:1.97-slim AS builder
WORKDIR /build
COPY . .
RUN cargo build --release --locked

FROM gcr.io/distroless/cc-debian12:nonroot
COPY --from=builder /build/target/release/deltaglider-operator /deltaglider-operator
USER nonroot
ENTRYPOINT ["/deltaglider-operator"]
