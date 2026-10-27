# Checkpoint

_Last updated: 2026-10-27 · dashboard signals, node quality and model compare shipped (#17–#19)_

## Where I am

Live on tokenstats.ai (DigitalOcean Droplet, pulls the CI-built GHCR image, `docs/DEPLOY.md` §6). Since July:

- **Feed freshness/health** (#1, #2): concurrent node polling, pruning of delisted models, quote TTL; `kind`, `age_secs`, `stale`, `outlier`, quality filters; `/health` `status` + `feed`
- **Sources** (#5): OrcaRouter alongside OpenRouter and Routstr; explicit per-1M prices win over unit heuristics (#12); pooled-connection fix + one retry for reference catalogs and BTC/USD (#15)
- **Trust/safety**: SSRF guard for Nostr-announced nodes (#3); reflected-XSS fix, app-level CSP + nosniff, opt-in Plausible (#11); node cap + TTL, BTC/USD sanity range + age (#12)
- **Reliability** (#8, #9): 1h/24h/7d reliability and p50/p95 latency persisted in SQLite; `/v1/info` probe, node identity check, data-driven Fastest / Most Private presets
- **API**: `/api/quotes` row cap raised to 20000 (#15); consumers narrow with `source` / `kind`
- **Dashboard signals** (#17): feed-health strip, Rel 24h / latency / age columns, outlier tag, Quality pills
- **Node quality** (#18): dormant backoff after 5 failed fetches (2×…1h, reset by a fresh announcement), failure classes (`dns`, `http_4xx`, `timeout`, …), `/api/nodes` `status` / `failure_class` / `next_poll_at`, per-quote `healthy` + `healthy=1` filter; presets sideline failing endpoints
- **Model compare** (#19): `canonical_model` on quotes/CSV, `GET /api/compare` + `/compare` page (offers, spread, cheapest / cheapest healthy; outliers never cheapest); outlier medians now cross-source
- **Hygiene**: CI (fmt, clippy `-D warnings`, tests, MSRV 1.86, Docker build + smoke, GHCR publish on main) (#6, #13, #14); deploy hardening docs (#10)
- 106 unit tests; `.github/dependabot.yml` for weekly grouped version PRs (nostr-sdk and MSRV toolchain refs excluded)
- **Consumers**: arbstr v2.4.0 fetches only routable sources with `kind=chat`, `max_age_secs`, `exclude_outliers`, gates on `/health` `status`, trusts `reliability_24h`; arbstr-node v1.2.0 pins it. Next arbstr release adds `healthy=1`

## Decisions made

- tokenstats observes, arbstr executes: keep them decoupled; probes stay free ("observe, don't pay")
- Default ranking stays cheapest blend; health is a filter/field, not a re-sort. Dead nodes stay listed (backoff, not removal)
- Additive API changes only; `AGENTS.md` holds the `/api/quotes` contract (arbstr depends on it)
- Analytics off unless `TOKENSTATS_PLAUSIBLE_SCRIPT` is set; no inline scripts (CSP)
- Deploys are manual and pull-based: CI publishes `ghcr.io/johnzilla/tokenstats` on green `main`, the Droplet pulls it
- tokenstats is unversioned (deployed by image SHA); arbstr / arbstr-node release via a release PR + signed tag

## Open questions

- Whether arbstr should use `/api/compare` (`cheapest_healthy`) for per-model routing instead of its own selection

## Next step

None queued. Watch the live strip: Routstr dormant count should settle near the dead-node total, poll cycles shorter. Candidates in `AGENTS.md` "Good next features" (price history, signed Best Now publish).

## Anchors

- `AGENTS.md` (API contract, conventions), `docs/DEPLOY.md`, `docs/USAGE.md`
- `src/store/` (quotes, nodes, history), `src/providers/` (catalogs, probes, netguard), `src/web/` (API, dashboard, CSP)
- `cargo test`; CI in `.github/workflows/ci.yml`
