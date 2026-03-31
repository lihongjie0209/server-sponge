# Build stage
FROM rust:1.85-bookworm AS builder
WORKDIR /app
COPY Cargo.toml Cargo.lock* ./
COPY src/ src/
RUN cargo build --release

# Runtime stage
FROM debian:bookworm-slim
RUN apt-get update && apt-get install -y --no-install-recommends \
    procps \
    stress-ng \
    && rm -rf /var/lib/apt/lists/*

COPY --from=builder /app/target/release/server-sponge /usr/local/bin/server-sponge

EXPOSE 8080

# Lower process priority
ENV RUST_LOG=info
ENTRYPOINT ["nice", "-n", "10", "server-sponge"]
