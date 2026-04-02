FROM rust:1.82-slim AS builder
WORKDIR /app
COPY . .
RUN cargo build --release --bin appbase

FROM debian:bookworm-slim
RUN apt-get update && apt-get install -y ca-certificates && rm -rf /var/lib/apt/lists/*
COPY --from=builder /app/target/release/appbase /usr/local/bin/
EXPOSE 3000
ENV APPBASE_MASTER_KEY=""
CMD ["appbase", "serve", "/app/server.js"]
