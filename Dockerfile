# syntax=docker/dockerfile:1
# Multi-stage build for tokenstats — DigitalOcean Droplet / any Linux host.

FROM rust:1.85-bookworm AS builder
WORKDIR /app

# Cache dependency builds
COPY Cargo.toml Cargo.lock ./
RUN mkdir src && echo 'fn main() {}' > src/main.rs \
    && cargo build --release \
    && rm -rf src

COPY src ./src
# Touch so cargo rebuilds the binary after real sources land
RUN touch src/main.rs \
    && cargo build --release \
    && strip target/release/tokenstats

FROM debian:bookworm-slim AS runtime
RUN apt-get update \
    && apt-get install -y --no-install-recommends ca-certificates curl \
    && rm -rf /var/lib/apt/lists/* \
    && useradd --system --create-home --uid 10001 tokenstats

WORKDIR /app
COPY --from=builder /app/target/release/tokenstats /usr/local/bin/tokenstats

RUN mkdir -p /data && chown tokenstats:tokenstats /data
USER tokenstats

ENV TOKENSTATS_BIND=0.0.0.0:8080 \
    TOKENSTATS_DB=/data/tokenstats.db \
    TOKENSTATS_LOG_JSON=true \
    RUST_LOG=info,tokenstats=info,tower_http=info

EXPOSE 8080
VOLUME ["/data"]

HEALTHCHECK --interval=30s --timeout=5s --start-period=20s --retries=3 \
  CMD curl -fsS "http://127.0.0.1:8080/health" >/dev/null || exit 1

ENTRYPOINT ["tokenstats"]
CMD ["serve"]
