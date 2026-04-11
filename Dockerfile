# Build all platform binaries.
FROM rust:latest AS builder
WORKDIR /build
COPY Cargo.toml Cargo.lock ./
COPY crates/ crates/
RUN cargo build --release \
    -p appbase-control \
    -p appbase-gateway \
    -p appbase-worker \
    -p appbase

# Use Ubuntu 24.04 (glibc 2.39) instead of Debian bookworm (glibc 2.36)
FROM ubuntu:24.04
RUN apt-get update && apt-get install -y --no-install-recommends \
    ca-certificates curl && rm -rf /var/lib/apt/lists/*
COPY --from=builder /build/target/release/appbase-control /usr/local/bin/
COPY --from=builder /build/target/release/appbase-gate /usr/local/bin/
COPY --from=builder /build/target/release/appbase-worker /usr/local/bin/
COPY --from=builder /build/target/release/appbase /usr/local/bin/
