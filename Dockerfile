# Stage 1: Build
FROM rust:1-bookworm AS builder

RUN apt-get update && apt-get install -y \
    build-essential \
    pkg-config \
    libssl-dev \
    && rm -rf /var/lib/apt/lists/*

WORKDIR /build
COPY . .

RUN cargo build --release -p pkg6depotd -p pkg6repo

# Stage 2: Runtime
FROM debian:bookworm-slim

RUN apt-get update && apt-get install -y \
    ca-certificates \
    libssl3 \
    && rm -rf /var/lib/apt/lists/*

COPY --from=builder /build/target/release/pkg6depotd /usr/local/bin/
COPY --from=builder /build/target/release/pkg6repo /usr/local/bin/
COPY docker/pkg6depotd.kdl /etc/pkg6depotd.kdl

RUN mkdir -p /var/pkg/repo

EXPOSE 8080

ENTRYPOINT ["pkg6depotd", "--config", "/etc/pkg6depotd.kdl", "--no-daemon"]
