# Deploy tokenstats on a DigitalOcean Droplet

Simple, production-ready path: **Ubuntu Droplet + Docker Compose + Caddy (HTTPS)**.

Assumes:

- [doctl](https://docs.digitalocean.com/reference/doctl/) installed and authenticated (`doctl auth init`)
- SSH key already on your DigitalOcean account
- Domain DNS you control (`tokenstats.ai` and/or `tokenta.pe`)

---

## 0. One-time: check doctl

```bash
doctl account get
doctl compute ssh-key list
doctl compute region list
```

Note your SSH key **ID** (or fingerprint) from the list. Examples below use placeholders:

| Placeholder | Example / meaning |
|-------------|-------------------|
| `SSH_KEY_ID` | e.g. `53936424` from `doctl compute ssh-key list` |
| `REGION` | e.g. `nyc1` |
| `DROPLET_NAME` | e.g. `tokenstats` |
| `DOMAIN` | `tokenstats.ai` or `tokenta.pe` |

---

## 1. Dockerfile review (already good)

| Concern | Status |
|---------|--------|
| Multi-stage build | Yes — `rust:1.85-bookworm` → `debian:bookworm-slim` |
| Non-root | Yes — uid `10001` `tokenstats` |
| Healthcheck | Yes — `GET /health` |
| Graceful stop | `STOPSIGNAL SIGTERM` + compose `stop_grace_period: 20s` (final SQLite snapshot) |
| Data dir | Volume `/data` → `TOKENSTATS_DB=/data/tokenstats.db` |

Rebuild after code changes:

```bash
docker compose build --no-cache tokenstats   # on the Droplet, in the repo
```

---

## 2. Create the Droplet with doctl

### Option A — Docker marketplace image (fastest)

Ubuntu 22.04 + Docker preinstalled:

```bash
export SSH_KEY_ID=53936424          # <-- your key id
export REGION=nyc1
export DROPLET_NAME=tokenstats

doctl compute droplet create "$DROPLET_NAME" \
  --region "$REGION" \
  --size s-1vcpu-1gb \
  --image docker-20-04 \
  --ssh-keys "$SSH_KEY_ID" \
  --wait \
  --enable-monitoring \
  --tag-names tokenstats,production

# Public IP
doctl compute droplet get "$DROPLET_NAME" --format ID,Name,PublicIPv4,Status
export DROPLET_IP=$(doctl compute droplet get "$DROPLET_NAME" --format PublicIPv4 --no-header)
echo "Droplet IP: $DROPLET_IP"
```

**Size notes**

| Size | Monthly (approx) | When |
|------|------------------|------|
| `s-1vcpu-1gb` | ~$6 | Fine for MVP / light traffic |
| `s-1vcpu-2gb` | ~$12 | Safer if Rust builds on-box feel tight |

First `docker compose build` compiles Rust and can use ~1–2 GB RAM; if OOM, use 2 GB or build elsewhere and `docker load`.

### Option B — Ubuntu 24.04 + install Docker yourself

```bash
doctl compute droplet create "$DROPLET_NAME" \
  --region "$REGION" \
  --size s-1vcpu-2gb \
  --image ubuntu-24-04-x64 \
  --ssh-keys "$SSH_KEY_ID" \
  --wait \
  --enable-monitoring \
  --tag-names tokenstats,production

export DROPLET_IP=$(doctl compute droplet get "$DROPLET_NAME" --format PublicIPv4 --no-header)
```

Then SSH and install Docker Engine + Compose plugin (official quick path):

```bash
ssh root@"$DROPLET_IP"

apt-get update
apt-get install -y ca-certificates curl
install -m 0755 -d /etc/apt/keyrings
curl -fsSL https://download.docker.com/linux/ubuntu/gpg -o /etc/apt/keyrings/docker.asc
chmod a+r /etc/apt/keyrings/docker.asc
. /etc/os-release
echo "deb [arch=$(dpkg --print-architecture) signed-by=/etc/apt/keyrings/docker.asc] https://download.docker.com/linux/ubuntu $VERSION_CODENAME stable" \
  > /etc/apt/sources.list.d/docker.list
apt-get update
apt-get install -y docker-ce docker-ce-cli containerd.io docker-compose-plugin
docker --version && docker compose version
```

### Firewall (DigitalOcean cloud firewall or UFW)

**UFW on the Droplet:**

```bash
ssh root@"$DROPLET_IP"
ufw allow OpenSSH
ufw allow 80/tcp
ufw allow 443/tcp
# Only if you need direct app access without proxy:
# ufw allow 8080/tcp
ufw --force enable
ufw status
```

**Or create a DO cloud firewall with doctl:**

```bash
doctl compute firewall create \
  --name tokenstats-fw \
  --droplet-ids "$(doctl compute droplet get "$DROPLET_NAME" --format ID --no-header)" \
  --inbound-rules "protocol:tcp,ports:22,address:0.0.0.0/0,address:::/0 protocol:tcp,ports:80,address:0.0.0.0/0,address:::/0 protocol:tcp,ports:443,address:0.0.0.0/0,address:::/0" \
  --outbound-rules "protocol:tcp,ports:all,address:0.0.0.0/0,address:::/0 protocol:udp,ports:all,address:0.0.0.0/0,address:::/0 protocol:icmp,address:0.0.0.0/0,address:::/0"
```

---

## 3. Point DNS at the Droplet

At your registrar / DNS host:

| Type | Name | Value |
|------|------|--------|
| A | `@` (or `tokenstats.ai`) | `$DROPLET_IP` |
| A | `www` (optional) | `$DROPLET_IP` |
| A | `@` for `tokenta.pe` (if used) | `$DROPLET_IP` |

Wait until:

```bash
dig +short tokenstats.ai   # should show $DROPLET_IP
```

Caddy will fail ACME until DNS is correct.

---

## 4. Deploy the app on the Droplet

```bash
ssh root@"$DROPLET_IP"

# App directory
mkdir -p /opt/tokenstats
cd /opt/tokenstats

# Clone (public repo)
git clone https://github.com/johnzilla/tokenstats.git .
# Or: git pull if already cloned

# Env for Caddy domain + app knobs
cp deploy/env.example .env
nano .env
# Set at least:
#   DOMAIN=tokenstats.ai
#   # or: DOMAIN=tokenstats.ai, tokenta.pe
#   # or: DOMAIN=tokenta.pe
#   ACME_EMAIL=you@example.com
```

### 4a. Recommended: Compose with Caddy (HTTPS)

```bash
cd /opt/tokenstats

# Build + start app + Caddy
docker compose --profile proxy up -d --build

docker compose ps
docker compose logs -f tokenstats
# Ctrl-C to detach logs

curl -fsS http://127.0.0.1:8080/health
curl -fsSI https://tokenstats.ai/health
```

When the **proxy** profile is up, Caddy listens on 80/443 and proxies to `tokenstats:8080`. The app still publishes host `:8080` by default for debugging; lock that down if you want (see §6).

### 4b. App only (HTTP on :8080, no TLS yet)

```bash
docker compose up -d --build
curl -fsS "http://$DROPLET_IP:8080/health"
```

---

## 5. HTTPS reverse proxy

### Path A — Caddy via Docker Compose (preferred)

Files:

- `docker-compose.yml` — service `caddy` under `profiles: [proxy]`
- `deploy/Caddyfile` — reverse_proxy + Let's Encrypt

```bash
# .env
DOMAIN=tokenstats.ai
# Dual domain example:
# DOMAIN=tokenstats.ai, tokenta.pe
ACME_EMAIL=you@example.com

docker compose --profile proxy up -d --build
docker compose logs -f caddy
```

Caddy stores certs in the `caddy-data` volume. Every name listed in `DOMAIN` needs a DNS A record to the Droplet.

### Path B — Nginx on the host + Certbot

1. Run **only** the app, bound to localhost (edit compose ports):

```yaml
# docker-compose.yml ports for tokenstats:
ports:
  - "127.0.0.1:8080:8080"
```

```bash
docker compose up -d --build   # no proxy profile
```

2. Install Nginx + Certbot:

```bash
apt-get update
apt-get install -y nginx certbot python3-certbot-nginx
cp /opt/tokenstats/deploy/nginx.conf.example /etc/nginx/sites-available/tokenstats
# Edit server_name for tokenstats.ai and/or tokenta.pe
ln -sf /etc/nginx/sites-available/tokenstats /etc/nginx/sites-enabled/
rm -f /etc/nginx/sites-enabled/default
nginx -t && systemctl reload nginx

certbot --nginx -d tokenstats.ai -d www.tokenstats.ai
# or: certbot --nginx -d tokenta.pe
```

---

## 6. Updates (redeploy)

```bash
ssh root@"$DROPLET_IP"
cd /opt/tokenstats
git pull origin main
docker compose --profile proxy up -d --build
# SQLite data lives in volume tokenstats-data — safe across rebuilds
docker compose ps
curl -fsS https://tokenstats.ai/health
```

Graceful restart only:

```bash
docker compose stop tokenstats   # SIGTERM → final DB snapshot
docker compose --profile proxy up -d
```

### Update Droplet itself (OS packages)

```bash
doctl compute ssh "$DROPLET_NAME" --ssh-command "apt-get update && apt-get -y upgrade"
```

---

## 7. Useful doctl commands

```bash
# List / inspect
doctl compute droplet list
doctl compute droplet get tokenstats

# SSH
doctl compute ssh tokenstats

# Resize (power off first for some size changes)
doctl compute droplet-action power-off tokenstats --wait
doctl compute droplet-action resize tokenstats --size s-1vcpu-2gb --wait
doctl compute droplet-action power-on tokenstats --wait

# Rebuild OS (destructive to disk — not the Docker volume if you only wipe mistakingly; prefer snapshots)
doctl compute droplet-action snapshot tokenstats --snapshot-name tokenstats-$(date +%Y%m%d) --wait

# Destroy (careful)
# doctl compute droplet delete tokenstats --force
```

---

## 8. Configuration reference

| Variable | Where | Default |
|----------|--------|---------|
| `DOMAIN` | `.env` / Caddy | `tokenstats.ai` (comma-list OK) |
| `ACME_EMAIL` | `.env` / Caddy | `admin@tokenstats.ai` |
| `TOKENSTATS_BIND` | container | `0.0.0.0:8080` |
| `TOKENSTATS_DB` | container | `/data/tokenstats.db` |
| `TOKENSTATS_POLL_INTERVAL_SECS` | compose | `60` |
| `RUST_LOG` | compose | `info,tokenstats=info,...` |

App CLI/env details: [USAGE.md](./USAGE.md).

---

## 9. Troubleshooting

| Symptom | Check |
|---------|--------|
| ACME / TLS fail | DNS A record, ports 80/443 open, `docker compose logs caddy` |
| Empty dashboard | `docker compose logs tokenstats` — OpenRouter/outbound HTTPS? |
| OOM on build | Use `s-1vcpu-2gb` or build on a larger machine and `docker save` / `load` |
| Permission on `/data` | Volume should be owned by uid 10001 inside container (image sets this) |
| Stale binary | `docker compose build --no-cache tokenstats && docker compose --profile proxy up -d` |

Health:

```bash
curl -fsS http://127.0.0.1:8080/health | jq
curl -fsS https://$DOMAIN/api/quotes?limit=1 | jq
```

---

## 10. Minimal checklist

1. `doctl auth init` / account OK  
2. Create droplet (`docker-20-04`, `s-1vcpu-1gb` or `2gb`)  
3. UFW: 22, 80, 443  
4. DNS A → Droplet IP  
5. `git clone` → `/opt/tokenstats`  
6. `.env` with `DOMAIN` (e.g. `tokenstats.ai` or `tokenta.pe`) + `ACME_EMAIL`
7. `docker compose --profile proxy up -d --build`  
8. `curl https://$DOMAIN/health`  

That’s it — dashboard live with HTTPS, data on a Docker volume, redeploy with `git pull` + compose build.
