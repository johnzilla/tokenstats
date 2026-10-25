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
| Multi-stage build | Yes — `rust:1.86-bookworm` → `debian:bookworm-slim` (keep in step with `rust-version` and the CI `msrv` job) |
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
| `s-1vcpu-1gb` | ~$6 | Runtime is fine; **first Docker build may OOM** |
| `s-1vcpu-2gb` | ~$12 | Safer for on-box `cargo build --release` |

The Dockerfile already uses low-memory cargo settings (`CARGO_BUILD_JOBS=1`, LTO off). If the build still dies with `signal: 9, SIGKILL`, add swap **or** resize:

```bash
# Temporary 2G swap (survives until reboot unless you make fstab permanent)
fallocate -l 2G /swapfile || dd if=/dev/zero of=/swapfile bs=1M count=2048
chmod 600 /swapfile
mkswap /swapfile
swapon /swapfile
free -h

cd /opt/tokenstats && docker compose --profile proxy up -d --build
```

Or resize with doctl:

```bash
doctl compute droplet-action power-off tokenstats --wait
doctl compute droplet-action resize tokenstats --size s-1vcpu-2gb --resize-disk=false --wait
doctl compute droplet-action power-on tokenstats --wait
```

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

### Firewall (DigitalOcean cloud firewall **and** UFW)

> [!IMPORTANT]
> **Docker bypasses UFW.** Ports published by Docker (`ports:` in compose) get their own iptables rules, so UFW never sees that traffic. A published `8080` is reachable from the internet even though UFW doesn't allow it. Two fixes, use both:
> 1. Bind the app to localhost: `TOKENSTATS_PUBLISH_PORT=127.0.0.1:8080` in `.env` (the default in `deploy/env.example`).
> 2. Put a **DigitalOcean cloud firewall** in front. It filters before traffic reaches the Droplet, so Docker can't go around it.

> [!WARNING]
> The **Docker marketplace image** (`docker-20-04` / "Ubuntu Docker on Ubuntu 22.04") ships UFW rules allowing **2375/2376** (the Docker remote API) from anywhere. dockerd listens only on its local socket by default, so nothing is exposed until someone enables TCP, but delete the rules:
>
> ```bash
> ufw delete allow 2375/tcp && ufw delete allow 2376/tcp && ufw status numbered
> ```

**UFW on the Droplet:**

```bash
ssh root@"$DROPLET_IP"
ufw allow OpenSSH
ufw allow 80/tcp
ufw allow 443/tcp
# Note: UFW does not govern Docker-published ports (see above), so there is
# no point allowing or denying 8080 here; use TOKENSTATS_PUBLISH_PORT instead.
ufw --force enable
ufw status
```

**Plus a DO cloud firewall (recommended) with doctl:**

Inbound 22, 80, 443/tcp and 443/udp (Caddy serves HTTP/3). Outbound stays fully open: tokenstats reaches catalogs, Coinbase, Nostr relays and nodes on arbitrary ports.

```bash
doctl compute firewall create \
  --name tokenstats-fw \
  --droplet-ids "$(doctl compute droplet get "$DROPLET_NAME" --format ID --no-header)" \
  --inbound-rules "protocol:tcp,ports:22,address:0.0.0.0/0,address:::/0 protocol:tcp,ports:80,address:0.0.0.0/0,address:::/0 protocol:tcp,ports:443,address:0.0.0.0/0,address:::/0 protocol:udp,ports:443,address:0.0.0.0/0,address:::/0" \
  --outbound-rules "protocol:tcp,ports:all,address:0.0.0.0/0,address:::/0 protocol:udp,ports:all,address:0.0.0.0/0,address:::/0 protocol:icmp,address:0.0.0.0/0,address:::/0"
```

A `403 Forbidden` here means the doctl token can't write firewalls (read-only or custom scopes without `firewall:create`). Create a token with firewall scopes and add it as a separate context (`doctl auth init --context firewall-admin`), or create the firewall in the console under **Networking → Firewalls**. If SSH ever breaks after a firewall change, the Droplet's web console in the DO panel still works.

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

When the **proxy** profile is up, Caddy listens on 80/443 and proxies to `tokenstats:8080` over the internal Docker network. With `TOKENSTATS_PUBLISH_PORT=127.0.0.1:8080` (the `env.example` default) the app is reachable only from the Droplet itself (`curl localhost:8080/health`), not from the internet. Check from outside: `curl -m 8 http://$DROPLET_IP:8080/health` should time out.

### 4b. App only (HTTP on :8080, no TLS yet)

The app publishes on localhost only by default. For a quick IP smoke test, set `TOKENSTATS_PUBLISH_PORT=8080` in `.env` (public), and remove it once Caddy is up.

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

There is no CI deploy: the Droplet pulls the public repo and builds the image itself.

```bash
ssh root@"$DROPLET_IP"
cd /opt/tokenstats

# 1. Note what's running (your rollback point)
git log --oneline -1

# 2. Back up the SQLite DB (WAL mode: copy all three files)
mkdir -p /root/ts-backup
for f in tokenstats.db tokenstats.db-wal tokenstats.db-shm; do docker cp tokenstats:/data/$f /root/ts-backup/ 2>/dev/null; done

# 3. Make sure swap exists: a Cargo.lock change recompiles every dependency
free -h

# 4. Pull + rebuild (old container serves until the new one is up)
git pull origin main
docker compose --profile proxy up -d --build
docker compose logs -f tokenstats     # wait for "poll cycle complete", Ctrl-C
```

Verify from anywhere:

```bash
curl -s https://tokenstats.ai/health | jq '{status, reasons: .feed.reasons, quotes, sources: (.feed.sources | keys)}'
```

`status` should reach `ok` within a minute or two; `degraded` lists its `reasons`.

**Rollback:** `git checkout <sha from step 1> && docker compose --profile proxy up -d --build`. Schema changes are additive (new columns/tables are created on open and ignored by older builds), so the DB doesn't need restoring. Restore `/root/ts-backup` into the volume only if the data itself is bad.

**Expect the board to shrink after upgrading from a pre-#1 build** (before Oct 2026): quotes from delisted models and long-dead nodes are dropped on the first cycle (`--quote-ttl-secs`, default 24h). On tokenstats.ai this went from ~11k to ~8k quotes.

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
| `TOKENSTATS_PLAUSIBLE_SCRIPT` | `.env` / compose | unset = no analytics; set the site's `https://plausible.io/js/pa-….js` to enable |
| `TOKENSTATS_PUBLISH_PORT` | `.env` / compose | `127.0.0.1:8080` (compose default and `env.example`); set `8080` only for a public IP smoke test |

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
| `:8080` reachable from the internet | Docker bypasses UFW: set `TOKENSTATS_PUBLISH_PORT=127.0.0.1:8080` in `.env`, `docker compose --profile proxy up -d`, and add the cloud firewall (§2) |
| `doctl … firewall create` → 403 | Token lacks firewall write scope; see §2 |
| `/health` `status` not `ok` | Read `.feed.reasons`; `down` = no catalog fetched recently (outbound HTTPS / DNS?) |

Health:

```bash
curl -fsS http://127.0.0.1:8080/health | jq
curl -fsS https://$DOMAIN/api/quotes?limit=1 | jq
```

---

## 10. Minimal checklist

1. `doctl auth init` / account OK  
2. Create droplet (`docker-20-04`, `s-1vcpu-1gb` or `2gb`)  
3. Firewall: UFW 22/80/443 (delete marketplace 2375/2376 rules) **and** DO cloud firewall 22, 80, 443/tcp+udp  
4. DNS A → Droplet IP  
5. `git clone` → `/opt/tokenstats`  
6. `.env` with `DOMAIN` (e.g. `tokenstats.ai` or `tokenta.pe`) + `ACME_EMAIL` + `TOKENSTATS_PUBLISH_PORT=127.0.0.1:8080`
7. `docker compose --profile proxy up -d --build`  
8. `curl https://$DOMAIN/health` (expect `"status":"ok"`) and confirm `http://$DROPLET_IP:8080` is **not** reachable  

That’s it — dashboard live with HTTPS, data on a Docker volume, redeploy with `git pull` + compose build.
