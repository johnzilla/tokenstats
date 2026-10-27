# tokenstats usage guide

## Install & run

```bash
git clone https://github.com/johnzilla/tokenstats.git
cd tokenstats
cargo run --release -- serve
```

Open [http://127.0.0.1:8080/](http://127.0.0.1:8080/).

Binary after release build: `./target/release/tokenstats serve`.

## Environment variables

| Variable | Equivalent flag | Default |
|----------|-----------------|---------|
| `TOKENSTATS_BIND` | `--bind` | `127.0.0.1:8080` |
| `TOKENSTATS_RELAYS` | `--relay` | damus, nos.lol, nostr.band |
| `TOKENSTATS_NODES` | `--node` | (none) |
| `TOKENSTATS_ALLOW_PRIVATE_NODES` | `--allow-private-nodes` | false |
| `TOKENSTATS_DB` | `--db` | `data/tokenstats.db` |
| `TOKENSTATS_NO_NOSTR` | `--no-nostr` | false |
| `TOKENSTATS_NO_POLL` | `--no-poll` | false |
| `TOKENSTATS_NO_OPENROUTER` | `--no-openrouter` | false |
| `TOKENSTATS_NO_ORCAROUTER` | `--no-orcarouter` | false |
| `TOKENSTATS_NO_ORACLE` | `--no-oracle` | false |
| `TOKENSTATS_NO_PERSIST` | `--no-persist` | false |
| `TOKENSTATS_POLL_INTERVAL_SECS` | `--poll-interval-secs` | `60` |
| `TOKENSTATS_BTC_INTERVAL_SECS` | `--btc-interval-secs` | `60` |
| `TOKENSTATS_NORMALIZE_INTERVAL_SECS` | `--normalize-interval-secs` | `15` |
| `TOKENSTATS_PERSIST_INTERVAL_SECS` | `--persist-interval-secs` | `30` |
| `TOKENSTATS_HTTP_TIMEOUT_SECS` | `--http-timeout-secs` | `20` |
| `TOKENSTATS_NODE_CONCURRENCY` | `--node-concurrency` | `16` |
| `TOKENSTATS_NODE_TIMEOUT_SECS` | `--node-timeout-secs` | `10` |
| `TOKENSTATS_QUOTE_TTL_SECS` | `--quote-ttl-secs` | `86400` (`0` = never expire) |
| `TOKENSTATS_MAX_NODES` | `--max-nodes` | `512` (`0` = unlimited; `--node` seeds don't count) |
| `TOKENSTATS_NODE_TTL_SECS` | `--node-ttl-secs` | `604800` (`0` = never expire) |
| `TOKENSTATS_STALE_AFTER_SECS` | `--stale-after-secs` | `600` |
| `TOKENSTATS_MAX_PRICE_USD_PER_1M` | `--max-price-usd-per-1m` | `1000` (`0` = no ceiling) |
| `TOKENSTATS_OPENROUTER_URL` | `--openrouter-url` | OpenRouter models URL |
| `TOKENSTATS_ORCAROUTER_URL` | `--orcarouter-url` | `https://api.orcarouter.ai/v1/models` |
| `TOKENSTATS_PLAUSIBLE_SCRIPT` | `--plausible-script` | (none: no analytics) |
| `TOKENSTATS_BTC_USD_URL` | `--btc-usd-url` | Coinbase spot URL |
| `TOKENSTATS_LOG_JSON` | `--log-json` | false |
| `RUST_LOG` | (overrides `-v`) | derived from `-v` |

## Shutdown

`Ctrl-C` (SIGINT) or `SIGTERM` (Docker stop / systemd):

1. Cancel background tasks (Nostr, poller, oracle, persist)
2. Drain HTTP (`axum` graceful shutdown)
3. Write a **final SQLite snapshot** when persistence is enabled
4. Exit 0 on clean stop

## Common workflows

### Observe centralized catalogs only

```bash
tokenstats serve --no-nostr                  # OpenRouter + OrcaRouter
tokenstats serve --no-nostr --no-orcarouter  # OpenRouter only
```

Useful on restricted networks or when you only need centralized reference catalogs.

### Watch a specific Routstr node

```bash
tokenstats serve --node https://api.example.com --node https://other.example.com
```

Nodes are polled concurrently (`--node-concurrency`) at `/v1/models` (falling back to `/models`), each within `--node-timeout-secs`. Failures and timeouts count against reliability.

### Dead nodes and failure classes

Relays keep replaying announcements from nodes whose domains are long gone, so after 5 failed catalog fetches in a row a node turns **dormant**: it is polled on a backoff (2, 4, 8 … poll intervals, capped at 1 hour) instead of every cycle, and comes back immediately when a fetch succeeds or it announces itself again. Dormant nodes stay listed.

`GET /api/nodes` reports each node's `status` (`healthy` · `failing` · `dormant` · `skipped` · `pending`), `failure_class` for the current failure streak (`dns`, `timeout`, `tls`, `connect`, `http_4xx`, `http_5xx`, `parse`, `ssrf_skip`, `other`) and `next_poll_at` for dormant nodes. The feed's `sources.routstr` adds `dormant`, `skipped` and `failure_classes` counts.

### Private-address guard (SSRF)

Nostr announcements are unauthenticated, so by default tokenstats **never requests** a Nostr-discovered node whose host is, or resolves to, a non-public address: loopback (`127/8`, `::1`, `localhost`, `*.localhost`), RFC1918 (`10/8`, `172.16/12`, `192.168/16`), CGNAT `100.64/10`, link-local (`169.254/16` incl. cloud metadata `169.254.169.254`, `fe80::/10`), unique-local `fc00::/7`, unspecified (`0.0.0.0/8`, `::`) and multicast/broadcast. The guarded client connects only to checked addresses (no DNS rebinding) and refuses redirects to such hosts.

- Skipped nodes still appear in `GET /api/nodes` with `skip_reason` set (e.g. `"127.0.0.1 is loopback"`), `status: "skipped"` and `failure_class: "ssrf_skip"`; they get no reliability samples. `skip_reason` is `null` for polled nodes.
- Operator-seeded `--node` endpoints are trusted, so `--node http://127.0.0.1:8000` works for local Routstr testing.
- On a trusted LAN, `--allow-private-nodes` (`TOKENSTATS_ALLOW_PRIVATE_NODES=true`) polls private Nostr-announced nodes too. Don't enable it on a public deployment.

### Freshness

- A successful catalog fetch replaces that provider endpoint's models: models it no longer lists are removed. An empty response is ignored rather than wiping the provider.
- Quotes not re-observed within `--quote-ttl-secs` (default 24h) are dropped, e.g. when a node stays down. Expiry runs only while polling is enabled, so `--no-poll` inspection of an old snapshot keeps everything.
- Quotes older than `--stale-after-secs` (default 10 min) are kept but flagged `stale: true`; pass `exclude_stale=1` to hide them.
- A USD price above `--max-price-usd-per-1m` (default $1000) is treated as a unit error and nulled (both USD and sats for that side).

### Node cap and expiry

Anyone who can post to a subscribed relay can announce endpoints, so discovered nodes are bounded:

- **Cap** (`--max-nodes`, default 512). At the limit, a new announcement evicts the lowest-ranked discovered node, where rank = (catalog fetched in the last 24h, `last_seen`). A newcomer that ranks below every node is ignored, so a flood of fresh announcements can only displace nodes that aren't answering. `--node` seeds are never evicted.
- **Expiry** (`--node-ttl-secs`, default 7 days). A node is dropped once it has been neither announced nor successfully polled for the TTL *while tokenstats kept trying to poll it*. `last_seen` uses the announcement's own timestamp (clamped to now), so relay replays of old events don't refresh it. The "kept trying" clock is in memory, so a restart restarts it; the cap is the hard bound.

### BTC/USD rate

Rates outside $100–$10M are rejected (the previous rate is kept and a warning logged), so a feed glitch can't corrupt every sats price. `/health` and `/api/summary` report `btc_usd_at` and `btc_usd_age_secs` (also persisted), and the feed goes `degraded` when the rate is older than `--stale-after-secs` or its age is unknown.

### Reliability and latency history

Every catalog fetch (reference catalogs and Routstr nodes) is recorded twice: as a raw sample for `reliability_1h`, and in an hourly bucket kept for 7 days and saved to SQLite (`endpoint_hours`). From the buckets tokenstats serves `reliability_24h`, `reliability_7d`, and 24h `latency_p50_ms` / `latency_p95_ms` on quotes, nodes and CSV.

Each cycle also sends every Routstr node a cheap `GET /v1/info` probe, in parallel with its catalog fetch and through the same private-address guard. Probe response times have their own history (`endpoint_probe_hours`) and are preferred for latency; `latency_source` says which was used (`probe` or `catalog`). Probes never count toward reliability.

Latency values are bucketed into ~25% bins (the value is the bin's upper edge). They measure how responsive an endpoint is, not inference speed. Filter with `min_reliability_24h=0.9` and `max_latency_ms=500`.

The probe also returns what the node says about itself (`name`, `version`, `npub`, `mints`, `onion_url`). `identity_verified` is `true` when its `npub` matches the Nostr key that announced it. Probe details and identity are kept in memory and refresh within a cycle after restart.

### Security headers

Every response carries `X-Content-Type-Options: nosniff` and a `Content-Security-Policy`: no inline scripts (the dashboard JS is `/static/app.js`), scripts and connections only from this origin, inline styles allowed, no framing. Setting `--plausible-script` (https only) adds that one origin to `script-src`/`connect-src` and loads `/static/plausible-init.js`. Without it, the dashboard loads no third-party code.

### Feed health

`/health` and `/api/summary` include `status` (`ok` · `degraded` · `down` · `starting` · `offline`) and a `feed` object: `reasons`, `stale_quotes`, `last_cycle` (nodes ok/failed, delisted, expired, elapsed), per-source `endpoints`/`healthy`/`failing`/`dormant`/`failure_classes`/`models`/`last_success_at`/`last_error` (Routstr also `skipped`), `last_cycle.nodes_dormant` (not due this cycle), and Nostr `discovery.lag_secs`. Many dead Routstr nodes alone does not degrade the feed; zero healthy nodes does. The dashboard shows a feed strip under the header (see [Feed strip and quality signals](#feed-strip-and-quality-signals)).

### Ephemeral / CI smoke

```bash
tokenstats serve --no-nostr --no-persist --bind 127.0.0.1:0
```

(`--bind 127.0.0.1:0` only if you capture the bound port from logs; default `8080` is simpler.)

### Offline inspect last snapshot

```bash
tokenstats serve --no-nostr --no-poll --db data/tokenstats.db
```

Restores quotes/nodes from SQLite; no network polling.

## Dashboard tips

### Best Now

Shows the cheapest **blended** quote for each popular family (Claude Sonnet, Grok, DeepSeek, etc.). Under the price: **provider · host** for the winning node/catalog.

Flagship variants are preferred over mini/nano/lite when available.

### Feed strip and quality signals

Under the header, a one-line strip mirrors `/health` `feed`: status (with reasons when not `ok`), last poll cycle age and duration, each reference catalog (✓ + model count; hover a source for its last error), Routstr nodes OK / total (with dormant count; hover for failure classes), stale quote count, BTC rate age and Nostr announcement lag.

The price board shows the API's per-quote signals: **Rel 24h** (endpoint catalog-fetch success; 1h right after a restart; hover for 1h / 24h / 7d), **Latency** (24h p50, hover for p95 and whether it comes from the `/v1/info` probe or the catalog fetch), **Age** (highlighted when stale), and an **outlier** tag on the blend. The **Quality** pills toggle `healthy` / `exclude_stale` / `exclude_outliers`; like the other filters they carry into links and CSV export.

### Blended cost

\[
\text{blend} = \frac{P_{\text{in}} + r \cdot P_{\text{out}}}{1 + r}
\]

where \(r\) is the output:input ratio (2, 3, or 4). Default \(r = 3\) (1:3 in:out).

### Filter presets

| Preset | Intent |
|--------|--------|
| **Cheapest Frontier** | Popular frontier families, non-free, sorted by blend |
| **Fastest** | Fast model classes by name (`flash`, `mini`, `turbo`, `haiku`, …), ranked by measured endpoint latency (reliable ≥90% / 24h with a successful last fetch first) |
| **Most Private** | Routstr/Nostr/onion/localhost endpoints; onion first, then identity-verified nodes; drops nodes below 50% 24h reliability or whose last fetch failed |

Presets run after the other filters, so combine them with `kind=chat` to keep embedding models out.
| **Local-first** | Ollama, vLLM, localhost, free local-style rows |

### Capabilities

Each quote carries `tools`, `reasoning` and `vision` (`true` / `false` / `null` = not published), plus `input_modalities` and `max_output_tokens`, read from OpenRouter-style catalog metadata (`supported_parameters`, `architecture.input_modalities`, `top_provider`). Routstr nodes publish modalities but not `supported_parameters`, so their `tools` / `reasoning` are filled from OpenRouter (else OrcaRouter) for the same `canonical_model` and named in `capabilities_inferred`: the model supports it upstream, the node may not forward it.

Filter with `capabilities=tools,vision` (all must be `true`) on the board, `/api/quotes`, `/api/compare` and CSV. The dashboard shows a **Capabilities** pill row and small tags on each model (dashed = inferred).

```bash
curl -s 'localhost:8080/api/quotes?kind=chat&healthy=1&capabilities=tools&limit=20' | jq '.[] | {model, node, blended_usd, capabilities_inferred}'
```

### Compare a model across sources

`/compare?model=claude-sonnet-4.5` (or **compare** when hovering a board row) puts every offer of one model side by side: OpenRouter, OrcaRouter and each Routstr node, cheapest blend first, with **vs cheapest**, out sats, Rel 24h, latency, age and a healthy mark. Without `model` it lists the models offered by the most sources.

Catalogs spell ids differently (`anthropic/claude-sonnet-4.5`, `claude-sonnet-4-5`, `claude-sonnet-4-5-20250929`), so offers are joined on `canonical_model`: vendor prefix dropped, lowercased, `.` / `_` as `-`, trailing release date stripped. A `:variant` suffix (`:free`, `:batch`) stays its own group. `model` matches as a substring of that key. Blend ratio and the quality filters apply; presets don't. Each group's cheapest offer and spread skip outliers (usually a node's unit bug), which stay listed with a negative **vs cheapest**.

### Copy curl / OpenAI config

Hover a Best Now or price-board row → **curl** or **config** (or **compare**, see above).

- **curl**: `POST {base}/chat/completions` with `Authorization: Bearer $API_KEY`
- **config**: JSON with `base_url`, `model`, and a Python OpenAI client snippet

Base URL is taken from the quote’s endpoint (OpenRouter defaults to `https://openrouter.ai/api/v1`). You still supply a real API key / Cashu token to *execute* inference.

### Export CSV

Click **Export CSV** or:

```bash
curl -OJ 'http://127.0.0.1:8080/export.csv?preset=frontier&ratio=3'
curl -OJ 'http://127.0.0.1:8080/export.csv?source=openrouter&model=claude'
curl -OJ 'http://127.0.0.1:8080/export.csv?source=openrouter,orcarouter&model=claude'
```

Columns include source, provider, model, USD/sats, blended price, delta, endpoint, context, timestamps, plus `kind`, `age_secs`, `stale`, reliability / latency, `outlier`, `healthy`, `canonical_model`, and `tools` / `reasoning` / `vision` / `max_output_tokens` / `capabilities_inferred` (`;`-separated). Export honors the same filters as the board and API.

## API examples

```bash
# Health
curl -s localhost:8080/health | jq

# Best Now + counts
curl -s localhost:8080/api/summary | jq '.best_now[:5]'

# Filtered quotes
curl -s 'localhost:8080/api/quotes?preset=fastest&limit=20' | jq '.[].model'

# Routable-looking chat quotes only: fresh, reliable, no outliers
curl -s 'localhost:8080/api/quotes?kind=chat&exclude_stale=1&min_reliability=0.5&exclude_outliers=1&limit=500' | jq length

# One model across all sources: cheapest healthy offer and spread
curl -s 'localhost:8080/api/compare?model=claude-sonnet-4.5&exclude_stale=1' | jq '.[0] | {canonical_model, offers, spread_pct, node: .cheapest_healthy.node, blend: .cheapest_healthy.blended_usd}'

# Feed status
curl -s localhost:8080/health | jq '{status, reasons: .feed.reasons}'

# Nodes
curl -s localhost:8080/api/nodes | jq
```

## Persistence

- Path: `data/tokenstats.db` by default (WAL mode).
- Load on startup; full snapshot every ~30 seconds.
- Disable with `--no-persist`.
- Directory `data/` is gitignored.

## Troubleshooting

| Symptom | What to try |
|---------|-------------|
| Empty board | Wait for the first catalog poll; check logs with `-v` |
| No nodes | Nostr blocked? Use `--node URL` or wait for kind 38421 events |
| Node has `skip_reason` | Its announced endpoint is private/loopback; seed it with `--node` or use `--allow-private-nodes` on a trusted LAN |
| Sats show `—` | BTC/USD fetch failed (Coinbase); USD columns still work |
| Deltas always `—` | Need a second catalog poll after prices change |
| Stale data after restart | Confirm `--db` path; avoid `--no-persist` if you want restore |

## Protocol references

- [RIP-02 Discovery](https://github.com/Routstr/protocol/blob/main/RIP-02.md) — kind 38421
- [RIP-05 Pricing](https://github.com/Routstr/protocol/blob/main/RIP-05.md) — units & cost model
- [Routstr docs](https://docs.routstr.com/)
