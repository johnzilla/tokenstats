# Deploying tokenstats (DigitalOcean Droplet)

## Requirements

- Droplet with Docker + Docker Compose plugin (Ubuntu 22.04+ recommended)
- Outbound HTTPS + WebSocket (Nostr relays, OpenRouter, Coinbase)
- Open inbound TCP **8080** (or put Nginx/Caddy in front on 80/443)

## Quick start

```bash
git clone https://github.com/johnzilla/tokenstats.git
cd tokenstats
docker compose up -d --build
curl -s http://127.0.0.1:8080/health | jq
```

Dashboard: `http://<droplet-ip>:8080/`

Data persists in the Docker volume `tokenstats-data` → `/data/tokenstats.db` in the container.

## Configuration

Compose reads env vars (see `docker-compose.yml`). Useful overrides:

| Variable | Purpose | Default |
|----------|---------|---------|
| `TOKENSTATS_PUBLISH_PORT` | Host port | `8080` |
| `TOKENSTATS_POLL_INTERVAL_SECS` | Catalog poll period | `60` |
| `TOKENSTATS_BTC_INTERVAL_SECS` | BTC/USD refresh | `60` |
| `TOKENSTATS_PERSIST_INTERVAL_SECS` | SQLite snapshot | `30` |
| `TOKENSTATS_RELAYS` | Nostr relays (comma) | damus / nos.lol / nostr.band |
| `TOKENSTATS_NODES` | Seed Routstr base URLs | (none) |
| `TOKENSTATS_NO_NOSTR` | `"true"` to skip Nostr | off |
| `TOKENSTATS_NO_OPENROUTER` | `"true"` to skip OpenRouter | off |
| `RUST_LOG` | tracing filter | `info,tokenstats=info,...` |

Create a `.env` file next to compose if you prefer not to export vars.

## Graceful restart

```bash
docker compose stop    # SIGTERM → final SQLite snapshot → exit
docker compose up -d
```

## Reverse proxy (optional)

Point Caddy/Nginx at `127.0.0.1:8080` and terminate TLS. Health check path: `/health`.

## Firewall sketch (ufw)

```bash
ufw allow OpenSSH
ufw allow 8080/tcp   # or 80/443 if proxied
ufw enable
```

## Logs

```bash
docker compose logs -f tokenstats
```

JSON logs are enabled in the image (`TOKENSTATS_LOG_JSON=true`) for easier aggregation.
