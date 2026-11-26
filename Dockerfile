# syntax=docker/dockerfile:1
FROM rust:1-bookworm AS builder
WORKDIR /app
COPY Cargo.toml Cargo.lock ./
RUN mkdir -p src benches && echo "" > src/lib.rs && echo "fn main() {}" > src/main.rs && echo "fn main() {}" > benches/engine_benchmarks.rs \
    && cargo build --release --locked --bin tileharvester \
    && rm -rf src benches
COPY src/ ./src/
COPY benches/ ./benches/
COPY tests/ ./tests/
RUN touch src/lib.rs src/main.rs && rm -rf target/release/tileharvester target/release/deps/*tileharvester* \
    && cargo build --release --locked --bin tileharvester

FROM debian:bookworm-slim
LABEL org.opencontainers.image.title="TileHarvester" \
      org.opencontainers.image.description="Automatically add Squadrats stats to Strava activities" \
      org.opencontainers.image.source="https://github.com/jb381/TileHarvester" \
      org.opencontainers.image.licenses="MIT"
WORKDIR /app
RUN apt-get update && apt-get install -y --no-install-recommends ca-certificates \
    && rm -rf /var/lib/apt/lists/* \
    && groupadd --gid 10001 tileharvester \
    && useradd --uid 10001 --gid 10001 --no-create-home tileharvester \
    && mkdir -p /data && chown tileharvester:tileharvester /app /data
COPY --from=builder /app/target/release/tileharvester /usr/local/bin/tileharvester
USER tileharvester
ENV TH_DATA_DIR=/data
STOPSIGNAL SIGTERM
HEALTHCHECK --interval=5m --timeout=10s --start-period=15s --retries=3 CMD ["tileharvester", "health", "--no-check-rate-limit"]
ENTRYPOINT ["tileharvester"]
CMD ["sync"]
