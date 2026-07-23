# AGENTS.md — tokenstats

Guidance for coding agents working in this repository.

## Project

**tokenstats** is a Rust observability layer + price oracle for sovereign inference markets (Routstr) and reference catalogs (OpenRouter). Stack: Tokio, Axum 0.7, nostr-sdk 0.30, reqwest, rusqlite, clap. License: **MPL-2.0**.

## Layout

```
src/
  main.rs           CLI (serve) + task wiring
  market.rs         Popular families, blend math, presets
  nostr/            RIP-02 kind 38421 listener
  providers/        openrouter.rs, routstr.rs, catalog.rs
  oracle/           BTC/USD + sats normalization
  store/            In-memory quotes, nodes, poll history
  persist/          SQLite load/save loop
  web/              Axum router + HTML dashboard + CSV
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
- Logging: `tracing` (`info!` for lifecycle, `debug!` for per-event noise).
- Async: Tokio `full`; background work via `tokio::spawn`.
- Shared state: `Arc<Store>` with internal `RwLock`s.

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
- Snapshot ~30s; restore on boot. Use `spawn_blocking` for rusqlite.

### CLI / env

| Flag | Env |
|------|-----|
| `--bind` | `TOKENSTATS_BIND` |
| `--relay` | `TOKENSTATS_RELAYS` |
| `--node` | `TOKENSTATS_NODES` |
| `--db` | `TOKENSTATS_DB` |

## Commands agents should use

```bash
cargo check
cargo run -- serve --no-nostr -v          # fast local smoke
cargo run -- serve --no-nostr --no-poll --db /tmp/ts.db   # restore-only
curl -s localhost:8080/health
curl -s 'localhost:8080/api/summary' | head
curl -s 'localhost:8080/export.csv?preset=frontier' | head
```

## Do not

- Commit `data/`, `target/`, or local DB files.
- Pin alpha `nostr-sdk` (0.45-alpha) without an explicit upgrade task.
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
- This file if conventions change
