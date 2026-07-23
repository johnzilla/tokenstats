# Checkpoint

_Last updated: 2026-07-22 · MVP observability stack + dashboard polish; docs + first full commit_

## Where I am

Working **MVP is feature-complete for observation**: CLI `serve`, Nostr RIP-02 (kind 38421) discovery, OpenRouter + Routstr `/v1/models` polling, in-memory store with price deltas and node reliability, BTC/USD oracle (USD+sats), SQLite persistence, Axum dashboard with Best Now, blend ratios, presets, hover copy curl/config, and CSV export.

Code lives under `src/`; docs updated in `README.md`, `AGENTS.md`, `docs/USAGE.md`.

## Decisions made

- **RIP-02 kind 38421** for discovery (roadmap 40500 kept as legacy subscribe only).
- **Single binary crate**, server-rendered HTML (no SPA).
- **OpenRouter as reference catalog** always polled; Routstr nodes from Nostr + `--node`.
- **Blend** = weighted avg `(in + r*out)/(1+r)`; default r=3.
- **SQLite** optional default on (`data/tokenstats.db`), 30s snapshots; rusqlite bundled.
- **MPL-2.0** license retained from scaffold.

## Open questions

- Whether to publish a signed oracle feed (Nostr event) next vs price-history charts.
- How aggressive to be on “Fastest” (name heuristics vs measured latency).
- Public demo host / deploy story (none yet).

## Next step

After push: pick one of (1) multi-sample price history in SQLite + sparkline/delta window, (2) model-compare view across sources, or (3) signed Best Now oracle export — confirm with user.

## Anchors

- Branch: `main`
- Run: `cargo run -- serve` → `http://127.0.0.1:8080/`
- Key modules: `src/main.rs`, `src/web/handlers.rs`, `src/market.rs`, `src/nostr/mod.rs`, `src/persist.rs`
- Export: `/export.csv?preset=frontier&ratio=3`
- Specs: https://github.com/Routstr/protocol (RIP-02, RIP-05)
