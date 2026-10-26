# Checkpoint

_Last updated: 2026-10-26 · Oct 2026 round shipped; arbstr v2.4.0 consumes the new feed_

## Where I am

Live on tokenstats.ai (DigitalOcean Droplet, pulls the CI-built GHCR image, `docs/DEPLOY.md` §6). Since July:

- **Feed freshness/health** (#1, #2): concurrent node polling, pruning of delisted models, quote TTL; `kind`, `age_secs`, `stale`, `outlier`, quality filters; `/health` `status` + `feed`
- **Sources** (#5): OrcaRouter alongside OpenRouter and Routstr; explicit per-1M prices win over unit heuristics (#12); pooled-connection fix + one retry for reference catalogs and BTC/USD (#15)
- **Trust/safety**: SSRF guard for Nostr-announced nodes (#3); reflected-XSS fix, app-level CSP + nosniff, opt-in Plausible (#11); node cap + TTL, BTC/USD sanity range + age (#12)
- **Reliability** (#8, #9): 1h/24h/7d reliability and p50/p95 latency persisted in SQLite; `/v1/info` probe, node identity check, data-driven Fastest / Most Private presets
- **API**: `/api/quotes` row cap raised to 20000 (#15); consumers narrow with `source` / `kind`
- **Hygiene**: CI (fmt, clippy `-D warnings`, tests, MSRV 1.86, Docker build + smoke, GHCR publish on main) (#6, #13, #14); deploy hardening docs (#10)
- 92 unit tests
- **Consumers**: arbstr v2.4.0 fetches only routable sources with `kind=chat`, `max_age_secs`, `exclude_outliers`, gates on `/health` `status`, trusts `reliability_24h`; arbstr-node v1.2.0 pins it

## Decisions made

- tokenstats observes, arbstr executes: keep them decoupled; probes stay free ("observe, don't pay")
- Additive API changes only; `AGENTS.md` holds the `/api/quotes` contract (arbstr depends on it)
- Analytics off unless `TOKENSTATS_PLAUSIBLE_SCRIPT` is set; no inline scripts (CSP)
- Deploys are manual and pull-based: CI publishes `ghcr.io/johnzilla/tokenstats` on green `main`, the Droplet pulls it
- tokenstats is unversioned (deployed by image SHA); arbstr / arbstr-node release via a release PR + signed tag

## Open questions

- `.github/dependabot.yml` for weekly version PRs (only security updates arrive today)

## Next step

None queued. Candidates: Dependabot config; watch OrcaRouter `reliability_24h` recover toward 1.0 after #15; model-compare view (same model across sources).

## Anchors

- `AGENTS.md` (API contract, conventions), `docs/DEPLOY.md`, `docs/USAGE.md`
- `src/store/` (quotes, nodes, history), `src/providers/` (catalogs, probes, netguard), `src/web/` (API, dashboard, CSP)
- `cargo test`; CI in `.github/workflows/ci.yml`
