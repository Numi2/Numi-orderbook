FROM rust:1.79-bullseye as builder
WORKDIR /build
COPY . .
RUN cargo build --release

FROM debian:bullseye-slim
RUN apt-get update && apt-get install -y ca-certificates && rm -rf /var/lib/apt/lists/*
WORKDIR /app
COPY --from=builder /build/target/release/orderbook /usr/local/bin/orderbook
COPY config.toml /app/config.toml
# Run as non-root; grant NET_ADMIN/RAW if capturing or using AF_PACKET
USER 65534
CMD ["/usr/local/bin/orderbook", "/app/config.toml"]


