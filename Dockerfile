FROM rust:1.88-bookworm AS builder

RUN apt-get update && apt-get install -y --no-install-recommends \
    cmake \
    clang \
    libssl-dev \
    pkg-config \
    && rm -rf /var/lib/apt/lists/*

WORKDIR /build
COPY Cargo.toml Cargo.lock ./
COPY src/ src/

RUN cargo build --release

FROM debian:bookworm-slim

RUN apt-get update && apt-get install -y --no-install-recommends \
    ca-certificates \
    && rm -rf /var/lib/apt/lists/*

COPY --from=builder /build/target/release/reth-sentry-node /usr/local/bin/
COPY sentry.toml /etc/sentry-node/sentry.toml

RUN mkdir -p /data

EXPOSE 30303/tcp 30303/udp 8546/tcp

ENTRYPOINT ["reth-sentry-node"]
CMD ["--data-dir", "/data", "--config", "/etc/sentry-node/sentry.toml"]
