# Checkpoint

_Last updated: 2026-10-25 · Feed quality, OrcaRouter, probes, CI, security review fixes_

## Where I am

Live on tokenstats.ai (DigitalOcean Droplet, pulls the CI-built GHCR image, `docs/DEPLOY.md` §6). Since July:

- **Feed freshness/health** (#1, #2): concurrent node polling, pruning of delisted models, quote TTL; `kind`, `age_secs`, `stale`, `outlier`, quality filters; `/health` `status` + `feed`
- **Sources** (#5): OrcaRouter alongside OpenRouter and Routstr; explicit per-1M prices win over unit heuristics (#12)
- **Trust/safety**: SSRF guard for Nostr-announced nodes (#3); reflected-XSS fix, app-level CSP + nosniff, opt-in Plausible (#11); node cap + TTL, BTC/USD sanity range + age (#12)
- **Reliability** (#8, #9): 1h/24h/7d reliability and p50/p95 latency persisted in SQLite; `/v1/info` probe, node identity check, data-driven Fastest / Most Private presets
- **Hygiene**: CI (fmt, clippy `-D warnings`, tests, MSRV 1.86, Docker build + smoke) (#6 + P3); deploy hardening docs (#10)
- 88 unit tests

## Decisions made

- tokenstats observes, arbstr executes: keep them decoupled; probes stay free ("observe, don't pay")
- Additive API changes only; `AGENTS.md` holds the `/api/quotes` contract
- Analytics off unless `TOKENSTATS_PLAUSIBLE_SCRIPT` is set; no inline scripts (CSP)
- Deploys are manual and pull-based: CI publishes `ghcr.io/johnzilla/tokenstats` on green `main`, the Droplet pulls it

## Open questions

- `.github/dependabot.yml` for weekly version PRs (only security updates arrive today)

## Next step

Update arbstr's tokenstats consumer to use `kind=chat&exclude_stale=1&exclude_outliers=1&min_reliability_24h=0.9` and gate on `/health` `status`.

## Anchors

- `AGENTS.md` (API contract, conventions), `docs/DEPLOY.md`, `docs/USAGE.md`
- `src/store/` (quotes, nodes, history), `src/providers/` (catalogs, probes, netguard), `src/web/` (API, dashboard, CSP)
- `cargo test`; CI in `.github/workflows/ci.yml`
