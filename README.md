# tokenstats

**Sovereign inference market observability + oracle** for [Routstr](https://routstr.com) and centralized catalogs (OpenRouter).

Rust CLI + lightweight Axum dashboard. Listens on Nostr for provider discovery (RIP-02), polls model catalogs, normalizes prices to **USD and sats**, persists to SQLite, and serves a live price board.

**License:** MPL-2.0

## Quick start

```bash
# Requires Rust 1.75+
cargo run --release -- serve

# Dashboard
open http://127.0.0.1:8080/
```

First boot pulls the OpenRouter models catalog (~300+ quotes), Coinbase BTC/USD, and (unless disabled) connects to Nostr relays for Routstr kind **38421** node announcements.

## CLI

```bash
tokenstats serve [OPTIONS]

Options:
  --bind <ADDR>       HTTP bind (default 127.0.0.1:8080)   [env: TOKENSTATS_BIND]
  --relay <URL>       Nostr relays (repeatable / comma)    [env: TOKENSTATS_RELAYS]
  --node <URL>        Seed Routstr-compatible endpoints    [env: TOKENSTATS_NODES]
  --db <PATH>         SQLite path (default data/tokenstats.db) [env: TOKENSTATS_DB]
  --no-nostr          Skip Nostr listener
  --no-poll           Skip HTTP provider polling
  --no-persist        In-memory only (no SQLite)
  -v, --verbose       Debug (-v) / trace (-vv)
```

### Examples

```bash
# Catalog + dashboard only (no Nostr)
cargo run -- serve --no-nostr

# Seed a known Routstr node
cargo run -- serve --node https://your-node.example.com

# Custom DB + bind
cargo run -- serve --bind 0.0.0.0:8080 --db ./tokenstats.db
```

## Dashboard

| Feature | Description |
|--------|-------------|
| **Best Now** | Cheapest blended price for popular families (Claude, Grok, DeepSeek, GPT, Gemini, …). Shows **node under the price**. |
| **Blend ratio** | Toggle 1:2 / 1:3 / 1:4 in:out workload (`?ratio=3`). |
| **Presets** | Cheapest Frontier · Fastest · Most Private · Local-first |
| **Δ out** | Price change vs previous catalog snapshot |
| **Hover → copy** | **curl** or OpenAI-compatible **config** JSON for that quote |
| **Export CSV** | Current filtered view → `/export.csv?...` |
| **Nodes** | Discovered RIP-02 endpoints + reliability (success rate last hour) |

Query params: `?source=openrouter&preset=frontier&ratio=4&model=deepseek`

## HTTP API

| Path | Description |
|------|-------------|
| `GET /` | HTML dashboard (auto-refresh ~15s) |
| `GET /health` | Liveness + quote/node counts + `last_updated` |
| `GET /api/quotes` | JSON quotes (`source`, `model`, `preset`, `ratio`, `limit`) |
| `GET /api/nodes` | Discovered nodes + reliability |
| `GET /api/summary` | Aggregates + Best Now strip |
| `GET /export.csv` | CSV of current filtered view |

## Architecture

```
Nostr (kind 38421) ──► store.nodes ──► poll /v1/models ──┐
                                                         ├──► store.quotes ──► dashboard / API
OpenRouter /api/v1/models ──────────────────────────────┘
Coinbase BTC-USD ──► oracle (USD ↔ sats)
SQLite snapshot every 30s
```

| Module | Role |
|--------|------|
| `src/nostr/` | RIP-02 discovery (also listens for legacy kind 40500) |
| `src/providers/` | OpenRouter + Routstr-compatible `/v1/models` |
| `src/market/` | Popular families, blend, presets |
| `src/oracle/` | BTC/USD + dual-unit normalization |
| `src/store/` | In-memory quotes/nodes + poll reliability |
| `src/persist/` | SQLite restore + periodic save |
| `src/web/` | Axum routes + HTML |

### Routstr alignment

- **RIP-02**: provider announcements are kind **38421** (`d`, `u`, `mint`, `version` tags). Models/pricing come from the node’s `/v1/models`, not the event body.
- **RIP-05**: pricing units tracked as USD/1M tokens with sats derived from live BTC/USD.

Specs: [Routstr/protocol](https://github.com/Routstr/protocol)

## Data & privacy

- Default DB: `data/tokenstats.db` (gitignored)
- No API keys required for the public OpenRouter models list or Coinbase spot price
- Routstr node inference still needs Cashu/API keys if you *call* endpoints — tokenstats only observes catalogs

## Development

```bash
cargo check
cargo run -- serve --no-nostr -v
cargo build --release
```

## Related docs

- [AGENTS.md](./AGENTS.md) — conventions for coding agents
- [docs/USAGE.md](./docs/USAGE.md) — CLI flags, env vars, API, dashboard tips
- [CHECKPOINT.md](./CHECKPOINT.md) — current session snapshot (work in progress)

## License

Mozilla Public License 2.0 — see [LICENSE](./LICENSE).
