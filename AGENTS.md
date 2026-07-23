# AGENTS.md — tokenstats

Guidance for coding agents working in this repository.

## Project

**tokenstats** is a Rust observability layer + price oracle for sovereign inference markets (Routstr) and reference catalogs (OpenRouter). Stack: Tokio, Axum 0.7, nostr-sdk 0.30, reqwest, rusqlite, clap. License: **MPL-2.0**.

## Layout

```
src/
  main.rs           CLI wiring, signals, graceful shutdown
  config.rs         Cli + Config (flags/env/intervals/sources)
  market.rs         Popular families, blend math, presets
  nostr/            RIP-02 kind 38421 listener
  providers/        openrouter.rs, routstr.rs, catalog.rs
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

- Edition 2021, MSRV 1.75.
- Errors: `anyhow` at boundaries; avoid panics in request paths.
- Logging: `tracing` with module targets; `--log-json` for containers.
- Async: Tokio `full`; background work via `tokio::spawn` + `CancellationToken`.
- Shared state: `Arc<Store>` with internal `RwLock`s.

### Config

Centralized in `src/config.rs`. Prefer adding new knobs there (CLI + `TOKENSTATS_*` env) instead of hardcoding intervals. Background loops must select on cancel.

### Store

- Quote key: `{source}:{provider_id}:{model}` (latest-wins).
- On upsert, preserve `prev_price_*` for delta indicators; oracle sats updates must **not** clobber deltas (`apply_sats_normalization`).
- Node reliability: poll success rate over the last hour (`record_poll`).

### Web / dashboard

- Server-rendered HTML is intentional (no frontend build).
- Query params: `source`, `model`, `preset`, `ratio`, `limit`.
- Export path: `/export.csv` must honor the same filters as the board.
- Escape all user/network-derived strings in HTML (`esc` / `esc_attr`).

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

- Discover cheapest / frontier models across OpenRouter and Routstr nodes
- Compare USD and sats pricing for routing or arbitrage
- Feed downstream agents (e.g. **arbstr**) without re-implementing catalog polling

Related helpers (not substitutes for the full board):

| Endpoint | Use when |
|----------|----------|
| `GET /api/quotes` | **Main integration** — full filtered quote list |
| `GET /api/summary` | Compact Best Now strip + counts + `btc_usd` |
| `GET /api/nodes` | Discovered Routstr endpoints + reliability only |
| `GET /health` | Liveness / quote count / `last_updated` |
| `GET /export.csv` | Same filters as quotes, for offline analysis |

Default base URL locally: `http://127.0.0.1:8080`.

### Request

```
GET /api/quotes?source=&model=&preset=&ratio=&limit=
```

| Query param | Type | Default | Description |
|-------------|------|---------|-------------|
| `source` | string | (all) | Exact source match (case-insensitive): `openrouter`, `routstr`, `nostr` |
| `model` | string | (all) | Case-insensitive **substring** on model id (e.g. `deepseek`, `claude-sonnet`) |
| `preset` | string | (none) | View preset (see below). When set, applies preset filter/sort instead of plain blend sort |
| `ratio` | float | `3` | Output:input workload for blended cost (`1:r`). Allowed range effectively `>0` and `≤20`; invalid falls back to `3` |
| `limit` | int | `200` | Max rows returned; capped at **2000** |

**Presets** (`preset=`):

| Value | Aliases | Behavior |
|-------|---------|----------|
| `frontier` | `cheapest-frontier`, `cheapest_frontier` | Popular frontier families, non-free, sorted by blend |
| `fastest` | `fast` | Name heuristics (`flash`, `mini`, `turbo`, …), sorted by blend |
| `private` | `most-private`, `most_private` | Routstr/Nostr/onion/localhost first |
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
| `source` | string | `openrouter` \| `routstr` \| … |
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
  "node": "OpenRouter · openrouter.ai"
}
```

### Semantics agents must respect

1. **Units are per 1M tokens**, not per token. Do not treat raw OpenRouter string prices as the API values.
2. **`blended_usd` is the preferred sort/score key** for “cheapest workload” unless the consumer has a different in:out mix — pass `ratio` explicitly.
3. **`delta_*` may be null** until a second catalog poll changes a price; do not treat null as “flat”.
4. **Data is latest-wins in memory** (optionally restored from SQLite). There is no historical series on this endpoint yet.
5. **No auth** on the local/default bind. If exposing publicly, put a reverse proxy / network policy in front; do not assume API keys exist on tokenstats.
6. **Breaking changes:** prefer additive fields. If renaming/removing fields or changing array-vs-object envelope, update this section, `docs/USAGE.md`, and call out arbstr/agent consumers.

### Suggested consumer pattern (arbstr)

```
1. GET /health          → wait until quotes > 0 (or last_updated set)
2. GET /api/quotes?...  → select candidates by blended_usd / source / model
3. Optionally GET /api/nodes for reliability before routing to a Routstr endpoint
4. Re-poll /api/quotes on an interval aligned with TOKENSTATS_POLL_INTERVAL_SECS (default 60s)
```

## Commands agents should use

```bash
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
- Latency-based “Fastest” (not only name heuristics)
- Signed oracle publish of Best Now
- Model compare (same model across OpenRouter vs Routstr nodes)
- Stale TTL for nodes/quotes

## Docs to keep in sync

When changing CLI flags, API routes, or RIP kinds, update:

- `README.md`
- `docs/USAGE.md`
- `docs/DEPLOY.md` (if Docker/env changes)
- **This file’s `GET /api/quotes` section** whenever the JSON contract changes (arbstr + agent integration)
- This file more generally if conventions change
