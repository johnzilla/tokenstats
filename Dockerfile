# syntax=docker/dockerfile:1
# Multi-stage production image for tokenstats (DigitalOcean Droplet / any Linux host).
#
# Build:  docker build -t tokenstats:local .
# Run:    docker run --rm -p 8080:8080 -v tsdata:/data tokenstats:local

# ---------------------------------------------------------------------------
# Builder
# ---------------------------------------------------------------------------
FROM rust:1.86-bookworm AS builder

WORKDIR /app

# Cache dependency compilation (rebuilds only when Cargo.* change)
COPY Cargo.toml Cargo.lock ./
RUN mkdir src \
    && echo 'fn main() { println!("cache"); }' > src/main.rs \
    && cargo build --release \
    && rm -rf src target/release/deps/tokenstats* target/release/tokenstats*

# Real sources
COPY src ./src
RUN touch src/main.rs \
    && cargo build --release \
    && strip /app/target/release/tokenstats

# ---------------------------------------------------------------------------
# Runtime (minimal, non-root)
# ---------------------------------------------------------------------------
FROM debian:bookworm-slim AS runtime

LABEL org.opencontainers.image.title="tokenstats" \
      org.opencontainers.image.description="Sovereign inference market observability" \
      org.opencontainers.image.source="https://github.com/johnzilla/tokenstats" \
      org.opencontainers.image.licenses="MPL-2.0"

RUN apt-get update \
    && apt-get install -y --no-install-recommends ca-certificates curl \
    && rm -rf /var/lib/apt/lists/* \
    && groupadd --gid 10001 tokenstats \
    && useradd --uid 10001 --gid tokenstats --home-dir /app --create-home --shell /usr/sbin/nologin tokenstats \
    && mkdir -p /data \
    && chown -R tokenstats:tokenstats /app /data

COPY --from=builder --chown=tokenstats:tokenstats /app/target/release/tokenstats /usr/local/bin/tokenstats

USER tokenstats
WORKDIR /app

ENV TOKENSTATS_BIND=0.0.0.0:8080 \
    TOKENSTATS_DB=/data/tokenstats.db \
    TOKENSTATS_LOG_JSON=true \
    RUST_LOG=info,tokenstats=info,tower_http=info

EXPOSE 8080
VOLUME ["/data"]

# Graceful shutdown: tokenstats handles SIGTERM (final SQLite snapshot)
STOPSIGNAL SIGTERM

HEALTHCHECK --interval=30s --timeout=5s --start-period=25s --retries=3 \
  CMD curl -fsS "http://127.0.0.1:8080/health" >/dev/null || exit 1

ENTRYPOINT ["tokenstats"]
CMD ["serve"]
