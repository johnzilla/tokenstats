# AGENTS.md — tokenstats

Guidance for coding agents working in this repository.

## Project

**tokenstats** is a Rust observability layer + price oracle for sovereign inference markets (Routstr) and reference catalogs (OpenRouter, OrcaRouter). Stack: Tokio, Axum 0.7, nostr-sdk 0.30, reqwest, rusqlite, clap. License: **MPL-2.0**.

## Layout

```
src/
  main.rs           CLI wiring, signals, graceful shutdown
  config.rs         Cli + Config (flags/env/intervals/sources)
  market.rs         Popular families, blend math, presets
  nostr/            RIP-02 kind 38421 listener
  providers/        reference.rs (OpenRouter, OrcaRouter), routstr.rs, catalog.rs
  oracle/           BTC/USD + sats normalization
  store/            In-memory quotes, nodes, poll history
  persist.rs        SQLite load/save loop
  web/              Axum routes + HTML dashboard + CSV
```

Do not invent parallel crates or a workspace unless asked. Keep the binary single-crate MVP.

## Principles

1. **MVP-first** — prefer in-memory + optional SQLite; no cloud deps, no account systems.
2. **RIP-aligned** — discovery is kind **38421** (not the marketing roadmap’s 40500 alone). Still subscribe to 40500 for forward-compat.
3. **Observe, don’t pay** — never implement Cashu spend paths unless explicitly requested; catalogs and discovery only.
4. **Dual units** — store/display USD and sats per 1M tokens; blend = `(in + r*out)/(1+r)`.
5. **Small diffs** — match existing style; avoid drive-by refactors and unsolicited deps.

## Conventions

### Rust

- Edition 2021, MSRV 1.86 (`Cargo.toml` `rust-version`, `Dockerfile` builder and the CI `msrv` job move together).
- Errors: `anyhow` at boundaries; avoid panics in request paths.
- Logging: `tracing` with module targets; `--log-json` for containers.
- Async: Tokio `full`; background work via `tokio::spawn` + `CancellationToken`.
- Shared state: `Arc<Store>` with internal `RwLock`s.

### Config

Centralized in `src/config.rs`. Prefer adding new knobs there (CLI + `TOKENSTATS_*` env) instead of hardcoding intervals. Background loops must select on cancel.

### Store

- Quote key: `{source}:{provider_id}:{model}` (latest-wins).
- Catalog fetches go through `replace_catalog` (drops delisted models for that source/provider/endpoint; empty fetch is a no-op). Quotes older than `--quote-ttl-secs` are expired by the poller.
- On upsert, preserve `prev_price_*` for delta indicators; oracle sats updates must **not** clobber deltas (`apply_sats_normalization`).
- Nodes are bounded: `upsert_node` enforces `--max-nodes` (evicts the lowest (fetched-in-24h, last_seen) discovered node, refuses newcomers that rank below everyone) and `prune_stale_nodes` expires nodes not announced/polled OK for `--node-ttl-secs` after a full TTL of attempts. Nostr `last_seen` = event `created_at` (clamped), never "now".
- BTC/USD: `Store::set_btc_usd(rate, at)` rejects rates outside `pricing::BTC_USD_MIN..=MAX`; `btc_usd_at` is persisted in `meta` and drives feed staleness.
- Node reliability: `record_poll` feeds raw 1h samples (`reliability_1h`) and hourly buckets with latency histograms (`src/store/history.rs`, 7 days) behind `endpoint_stats()`. Buckets persist in `endpoint_hours`; only hours touched since the last save are upserted.
- SSRF guard (`src/providers/netguard.rs`): non-`cli` nodes (i.e. Nostr announcements) are never requested when the host is or resolves to loopback/private/CGNAT/link-local/unique-local/unspecified, unless `--allow-private-nodes`. Use the guarded client (public-only resolver + redirect checks) for any new fetch of announced URLs. Skips set `ProviderNode.skip_reason`.
- Node probes: each cycle `NodeFetcher::poll` runs the catalog fetch and a `GET /v1/info` side by side (same guard). Probes feed `record_probe` (latency history in `endpoint_probe_hours` + latest `ProbeStatus`, not persisted), never reliability. Identity = `nostr::same_pubkey(announced pubkey, info.npub)`.

### Web / dashboard

- Server-rendered HTML is intentional (no frontend build).
- Query params: `source`, `model`, `preset`, `ratio`, `limit`, plus quality filters `kind`, `max_age_secs`, `min_reliability`, `exclude_stale`, `exclude_outliers` (carried through board links and CSV export by `QuotesQuery::qs`).
- Export path: `/export.csv` must honor the same filters as the board.
- Escape all user/network-derived strings in HTML (`esc` / `esc_attr`).
- Hrefs built from query params go through `percent_encode` (unreserved chars only) **and** `esc_attr`. A reflected-XSS regression test guards this.
- No inline `<script>`: the app sends a strict `Content-Security-Policy` (`src/web/mod.rs`). Put dashboard JS in `src/web/static/` and serve it from `/static/`. Third-party scripts only via config (`--plausible-script`), which adds just that origin to the CSP.

### Persistence

- Default path `data/tokenstats.db`; always gitignore `data/` and `*.db*`.
- Snapshot on interval + final snapshot on graceful shutdown.
- Use `spawn_blocking` for rusqlite.

### Git

- Commits pushed to GitHub must be **SSH-signed** (`commit.gpgsign=true`, `gpg.format=ssh`).

## Integration API: `GET /api/quotes`

**Primary machine-readable surface for arbstr and other agents.** Prefer this over scraping the HTML dashboard. Implementation: `src/web/handlers.rs` (`api_quotes` + `enrich_json`). Filter/sort helpers must stay in sync with `/export.csv` and the dashboard.

### Purpose

Returns the current in-memory price board as a **JSON array of quote objects** after optional filters, with **blended cost** and **price deltas** computed server-side. Use it to:

- Discover cheapest / frontier models across OpenRouter, OrcaRouter and Routstr nodes
- Compare USD and sats pricing for routing or arbitrage
- Feed downstream agents (e.g. **arbstr**) without re-implementing catalog polling

Related helpers (not substitutes for the full board):

| Endpoint | Use when |
|----------|----------|
| `GET /api/quotes` | **Main integration** — full filtered quote list |
| `GET /api/summary` | Compact Best Now strip + counts + `btc_usd` + feed `status`/`feed` |
| `GET /api/nodes` | Discovered Routstr endpoints + reliability (`reliability_1h/24h/7d`, `polls_24h`) + `latency_p50_ms/p95_ms`/`latency_source` + `identity_verified` + `probe` (`probed_at`, `ok`, `error`) + `info` (self-reported `/v1/info`: `name`, `version`, `npub`, `mints`, `onion_url`) + `last_success_at` / `last_error` / `consecutive_failures` + `skip_reason` (string \| null) when the SSRF guard refused to poll |
| `GET /health` | Liveness + feed `status` / `feed` object / quote count / `btc_usd` + `btc_usd_at` / `btc_usd_age_secs` / `last_updated` |
| `GET /export.csv` | Same filters as quotes, for offline analysis |

Default base URL locally: `http://127.0.0.1:8080`.

### Request

```
GET /api/quotes?source=&model=&kind=&preset=&ratio=&limit=&max_age_secs=&min_reliability=&min_reliability_24h=&max_latency_ms=&exclude_stale=&exclude_outliers=
```

| Query param | Type | Default | Description |
|-------------|------|---------|-------------|
| `source` | string | (all) | Exact source match (case-insensitive), comma-separated for several: `openrouter`, `orcarouter`, `routstr,nostr` |
| `kind` | string | (all) | Model kind(s), comma-separated: `chat`, `embedding`, `image`, `audio`, `rerank`, `other` |
| `max_age_secs` | int | (none) | Only quotes with `age_secs` ≤ this |
| `min_reliability` | float | (none) | Only quotes whose endpoint's 1h success rate ≥ this (0–1). Quotes with no poll history are **excluded** |
| `min_reliability_24h` | float | (none) | Same over 24h (hourly buckets, survives restarts). Steadier than 1h for routing |
| `max_latency_ms` | int | (none) | Only quotes whose endpoint's 24h p50 response time ≤ this. No latency samples → **excluded** |
| `exclude_stale` | bool | off | `1`/`true` hides quotes with `stale: true` |
| `exclude_outliers` | bool | off | `1`/`true` hides quotes with `outlier: true` |
| `model` | string | (all) | Case-insensitive **substring** on model id (e.g. `deepseek`, `claude-sonnet`) |
| `preset` | string | (none) | View preset (see below). When set, applies preset filter/sort instead of plain blend sort |
| `ratio` | float | `3` | Output:input workload for blended cost (`1:r`). Allowed range effectively `>0` and `≤20`; invalid falls back to `3` |
| `limit` | int | `200` | Max rows returned; capped at **2000** |

**Presets** (`preset=`):

| Value | Aliases | Behavior |
|-------|---------|----------|
| `frontier` | `cheapest-frontier`, `cheapest_frontier` | Popular frontier families, non-free, sorted by blend |
| `fastest` | `fast` | Fast model classes by name (`flash`, `mini`, `turbo`, …; no free way to measure tokens/s), then endpoints with measured latency and ≥90% 24h reliability first, by 24h p50, then blend |
| `private` | `most-private`, `most_private` | Routstr/Nostr/onion/localhost; drops endpoints with 24h reliability < 50%; onion (announced or `/v1/info`) first, then identity-verified nodes, then other Routstr, by 24h reliability, then blend |
| `local` | `local-first`, `local_first` | Local/ollama/vLLM-style candidates |

Without `preset`, results are sorted by **ascending blended USD**, then model name.

### Response

- **Status:** `200 OK`
- **Content-Type:** `application/json`
- **Body:** JSON **array** (not wrapped in `{ data: ... }`)

Empty store / no matches → `[]`.

#### Quote object fields

| Field | Type | Notes |
|-------|------|--------|
| `source` | string | `openrouter` \| `orcarouter` \| `routstr` \| … |
| `provider` | string | Human-readable provider / node name |
| `provider_id` | string | Stable id (e.g. `openrouter`, Routstr `d` tag, seed id) |
| `model` | string | Model id as published by the catalog |
| `price_in_usd` | number \| null | USD per **1M input** tokens |
| `price_out_usd` | number \| null | USD per **1M output** tokens |
| `price_in_sats` | number \| null | Sats per 1M input (null until BTC rate known) |
| `price_out_sats` | number \| null | Sats per 1M output |
| `blended_usd` | number \| null | `(in + r×out) / (1+r)` using request `ratio` (or default 3) |
| `blend_ratio` | number | `r` used for this response row |
| `delta_out_pct` | number \| null | % change in out price vs previous snapshot (positive = more expensive) |
| `delta_in_pct` | number \| null | % change in in price vs previous snapshot |
| `prev_price_out_usd` | number \| null | Previous out price used for delta |
| `prev_price_in_usd` | number \| null | Previous in price |
| `endpoint` | string \| null | Base API URL when known |
| `region` | string \| null | Optional region from discovery |
| `context_length` | number \| null | Context window tokens if published |
| `observed_at` | string (RFC3339) | When this quote was last observed |
| `node` | string | Display label (`provider · host`) for the winning node/catalog |
| `kind` | string | `chat` \| `embedding` \| `image` \| `audio` \| `rerank` \| `other`, from catalog `architecture.output_modalities`, else id heuristics, else `chat` |
| `age_secs` | number | Seconds since `observed_at` |
| `stale` | bool | `age_secs` > `TOKENSTATS_STALE_AFTER_SECS` (default 600) |
| `reliability_1h` | number \| null | Endpoint's catalog-fetch success rate over the last hour (reference catalogs included); null until polled since restart |
| `reliability_24h`, `reliability_7d` | number \| null | Endpoint fetch success rate over the last 24 / 168 clock hours (current hour included), persisted in SQLite |
| `latency_p50_ms`, `latency_p95_ms` | number \| null | Endpoint response time over 24h, successful samples only: the Routstr `/v1/info` probe when it has samples, else the catalog fetch (see `latency_source`). **Bucketed** (~25% bins, value = bin upper edge, max 20000). Endpoint responsiveness, not inference speed or TTFT |
| `latency_source` | string \| null | `probe` \| `catalog` \| null (no samples) |
| `identity_verified` | bool \| null | Node's `/v1/info` `npub` matches the Nostr key that announced it. `false` = mismatch or unparsable npub; null = nothing to compare (reference catalogs, `--node` seeds, not probed since restart) |
| `onion` | string \| null | Node's onion address, from the announcement or `/v1/info` |
| `outlier` | bool | Blend is >10× above or below the median for the same model id across ≥3 quotes. Low outliers often mean a unit bug on the node |

#### Example

```bash
# Cheapest frontier-ish board, 1:4 blend, top 50
curl -sS 'http://127.0.0.1:8080/api/quotes?preset=frontier&ratio=4&limit=50'

# DeepSeek-only across all sources
curl -sS 'http://127.0.0.1:8080/api/quotes?model=deepseek&limit=100'

# Routstr nodes only
curl -sS 'http://127.0.0.1:8080/api/quotes?source=routstr&limit=500'
```

Example element (illustrative):

```json
{
  "source": "openrouter",
  "provider": "OpenRouter",
  "provider_id": "openrouter",
  "model": "deepseek/deepseek-v4-flash",
  "price_in_usd": 0.098,
  "price_out_usd": 0.196,
  "price_in_sats": 148.4,
  "price_out_sats": 296.9,
  "blended_usd": 0.1715,
  "blend_ratio": 3.0,
  "delta_out_pct": null,
  "delta_in_pct": null,
  "prev_price_out_usd": null,
  "prev_price_in_usd": null,
  "endpoint": "https://openrouter.ai/api/v1",
  "region": null,
  "context_length": 1048576,
  "observed_at": "2026-07-22T23:37:15.389573Z",
  "node": "OpenRouter · openrouter.ai",
  "kind": "chat",
  "age_secs": 12,
  "stale": false,
  "reliability_1h": 1.0,
  "outlier": false
}
```

### Semantics agents must respect

1. **API units are per 1M tokens** in both USD and sats. Raw catalog rates are often **per token**; tokenstats normalizes before serving.
2. **`blended_usd` is the preferred sort/score key** for “cheapest workload” unless the consumer has a different in:out mix — pass `ratio` explicitly.
3. **Invalid / dynamic prices are `null`**, not negative. Catalog sentinels (OpenRouter `"-1"`, negative floats) are sanitized at ingest and on SQLite restore, and USD prices above `TOKENSTATS_MAX_PRICE_USD_PER_1M` (default $1000/1M) are nulled as unit errors. Do not invent costs for `null` sides.
4. **Default cheapest sort** ranks only **strictly positive** blends; free (`0`) and missing prices sort last (not first).
5. **`delta_*` may be null** until a second catalog poll changes a price; do not treat null as “flat”.
6. **Data is latest-wins in memory** (optionally restored from SQLite). There is no historical series on this endpoint yet. `observed_at` is the last successful fetch that listed the model; delisted models are removed on the next successful fetch, and quotes unseen for `TOKENSTATS_QUOTE_TTL_SECS` (default 24h) are dropped.
7. **No auth** on the local/default bind. If exposing publicly, put a reverse proxy / network policy in front; do not assume API keys exist on tokenstats.
8. **Breaking changes:** prefer additive fields. If renaming/removing fields or changing array-vs-object envelope, update this section, `docs/USAGE.md`, and call out arbstr/agent consumers.
9. **Check `/health` `status` before trusting the board.** `ok` = fresh; `degraded` = usable, read `feed.reasons`; `down` / `starting` = don't route on it; `offline` = serving a snapshot (`--no-poll`). `ok: true` is liveness only.

### Unit decision tree (ingest)

Implementation: `src/pricing.rs` (`normalize_catalog_prices`) + `src/providers/catalog.rs`.

```
catalog model row
  │
  ├─ sats_pricing.prompt/completion  (Routstr dual field)
  │     sanitize → if v < 1.0: treat as sats/token × 1e6
  │                else:       already sats/1M
  │     → price_in_sats / price_out_sats
  │
  ├─ pricing/sats_pricing .prompt_per_million / .completion_per_million
  │     explicit per-1M (OrcaRouter) → used as-is, overrides the heuristics below
  │
  ├─ pricing.prompt/completion  (OpenRouter-shaped USD)
  │     sanitize → if v < 0.01: treat as USD/token × 1e6
  │                else:        already USD/1M
  │     → price_in_usd / price_out_usd
  │
  ├─ else input_cost / output_cost (admin shapes)
  │     same USD heuristic as pricing
  │
  └─ missing side left null; oracle fills via BTC/USD on normalize tick
```

- OpenRouter and OrcaRouter usually have **USD only** (`primary = Usd`). OrcaRouter per-request (`pricing.request`) and unpriced meta-router models are kept with null prices.
- `architecture` is read loosely: `output_modalities: null` or odd shapes must never drop a model row.
- Routstr nodes that mirror OpenRouter often publish **both** (`primary = Both`).
- Sats-only nodes work without USD until the oracle has a BTC rate.

### Suggested consumer pattern (arbstr)

```
1. GET /health          → wait for status "ok" (or "degraded" + acceptable reasons)
2. GET /api/quotes?kind=chat&exclude_stale=1&exclude_outliers=1&min_reliability_24h=0.9
                        → select candidates by blended_usd / source / model
3. Optionally GET /api/nodes for reliability before routing to a Routstr endpoint
4. Re-poll /api/quotes on an interval aligned with TOKENSTATS_POLL_INTERVAL_SECS (default 60s)
```

## Commands agents should use

CI (`.github/workflows/ci.yml`) runs on every PR and on pushes to `main`. It must stay green, so run the same checks locally before pushing:

```bash
cargo fmt --check
cargo clippy --all-targets -- -D warnings
cargo check
cargo test
cargo run -- serve --no-nostr -v
cargo run -- serve --no-nostr --no-poll --db /tmp/ts.db
curl -s localhost:8080/health
curl -s 'localhost:8080/api/quotes?preset=frontier&limit=5'
curl -s 'localhost:8080/export.csv?preset=frontier' | head
docker compose up -d --build
```

## Do not

- Commit `data/`, `target/`, or local DB files.
- Pin alpha `nostr-sdk` without an explicit upgrade task.
- Add OpenAI/Anthropic SDK deps for observation-only work.
- Rewrite the dashboard in a SPA framework without a product decision.

## Good next features (if asked)

- Price history charts / multi-sample deltas beyond last snapshot
- True inference latency / TTFT (needs paid completions; out of scope under "observe, don't pay")
- Signed oracle publish of Best Now
- Model compare (same model across OpenRouter vs Routstr nodes)
- Stale TTL for nodes (quotes expire via `--quote-ttl-secs`)

## Docs to keep in sync

When changing CLI flags, API routes, or RIP kinds, update:

- `README.md`
- `docs/USAGE.md`
- `docs/DEPLOY.md` (if Docker/env changes)
- **This file’s `GET /api/quotes` section** whenever the JSON contract changes (arbstr + agent integration)
- This file more generally if conventions change
