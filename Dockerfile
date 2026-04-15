FROM rust:1.94-bookworm AS builder
WORKDIR /build

COPY Cargo.toml Cargo.lock ./
COPY src ./src

RUN cargo build --release --locked --bin orderbook

FROM debian:bookworm-slim
RUN apt-get update \
    && apt-get install -y --no-install-recommends ca-certificates \
    && rm -rf /var/lib/apt/lists/*
WORKDIR /app
COPY --from=builder /build/target/release/orderbook /usr/local/bin/orderbook
COPY config.toml /app/config.toml

# Run as non-root. Grant the container required Linux capabilities at
# deployment time when using PACKET_MMAP, realtime scheduling, or host tuning.
USER 65534
CMD ["/usr/local/bin/orderbook", "/app/config.toml"]
