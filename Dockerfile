# Build all platform binaries.
FROM rust:latest AS builder
WORKDIR /build
COPY Cargo.toml Cargo.lock ./
COPY crates/ crates/
RUN cargo build --release \
    -p zeroship-control \
    -p zeroship-gateway \
    -p zeroship-worker \
    -p zeroship-sandbox \
    -p zeroship

# Use Ubuntu 24.04 (glibc 2.39) instead of Debian bookworm (glibc 2.36)
FROM ubuntu:24.04
# `docker.io` gives us the docker CLI inside the sandbox container so
# zeroship-sandbox can shell out via docker.sock (Docker-out-of-Docker).
RUN apt-get update && apt-get install -y --no-install-recommends \
    ca-certificates curl docker.io && rm -rf /var/lib/apt/lists/*
COPY --from=builder /build/target/release/zeroship-control /usr/local/bin/
COPY --from=builder /build/target/release/zeroship-gate /usr/local/bin/
COPY --from=builder /build/target/release/zeroship-worker /usr/local/bin/
COPY --from=builder /build/target/release/zeroship-sandbox /usr/local/bin/
COPY --from=builder /build/target/release/zeroship /usr/local/bin/
