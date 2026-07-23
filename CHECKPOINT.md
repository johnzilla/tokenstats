# Checkpoint

_Last updated: 2026-07-22 · Config + graceful shutdown + logging + Docker + unit tests_

## Where I am

MVP is solid for observation **and** ops basics:

- Central `Config` (CLI/env intervals, source toggles, URLs)
- Graceful shutdown (SIGINT/SIGTERM → cancel tasks → HTTP drain → final SQLite snapshot)
- Richer tracing (targets, JSON logs, quieter deps, HTTP trace layer)
- Dockerfile + docker-compose + `docs/DEPLOY.md` for DigitalOcean-style deploy
- 12 unit tests (config, market, catalog, store)

## Decisions made

- Config stays clap/env only (no YAML file yet — keep simple unless asked).
- `CancellationToken` for all background loops; final persist always attempted when DB open.
- Docker image binds `0.0.0.0:8080`, data volume `/data`, JSON logs on by default in container.

## Open questions

- Multi-sample price history / model-compare / signed oracle — still product choices.
- Whether to add a config file format later.

## Next step

Commit + signed push of this hardening batch; then product features if user wants (history charts, compare view, etc.).

## Anchors

- `src/config.rs`, `src/main.rs` (signals)
- `Dockerfile`, `docker-compose.yml`, `docs/DEPLOY.md`
- `cargo test` — 12 tests
