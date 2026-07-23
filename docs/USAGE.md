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
| `TOKENSTATS_DB` | `--db` | `data/tokenstats.db` |
| `RUST_LOG` / default filter | `-v` | `info` (`debug` with `-v`) |

## Common workflows

### Observe OpenRouter only

```bash
tokenstats serve --no-nostr
```

Useful on restricted networks or when you only need a centralized reference catalog.

### Watch a specific Routstr node

```bash
tokenstats serve --node https://api.example.com --node https://other.example.com
```

Nodes are polled at `/v1/models` (with a few path fallbacks). Failures count against reliability.

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

### Blended cost

\[
\text{blend} = \frac{P_{\text{in}} + r \cdot P_{\text{out}}}{1 + r}
\]

where \(r\) is the output:input ratio (2, 3, or 4). Default \(r = 3\) (1:3 in:out).

### Filter presets

| Preset | Intent |
|--------|--------|
| **Cheapest Frontier** | Popular frontier families, non-free, sorted by blend |
| **Fastest** | Name heuristics (`flash`, `mini`, `turbo`, `haiku`, …) |
| **Most Private** | Routstr/Nostr/onion/localhost endpoints |
| **Local-first** | Ollama, vLLM, localhost, free local-style rows |

### Copy curl / OpenAI config

Hover a Best Now or price-board row → **curl** or **config**.

- **curl**: `POST {base}/chat/completions` with `Authorization: Bearer $API_KEY`
- **config**: JSON with `base_url`, `model`, and a Python OpenAI client snippet

Base URL is taken from the quote’s endpoint (OpenRouter defaults to `https://openrouter.ai/api/v1`). You still supply a real API key / Cashu token to *execute* inference.

### Export CSV

Click **Export CSV** or:

```bash
curl -OJ 'http://127.0.0.1:8080/export.csv?preset=frontier&ratio=3'
curl -OJ 'http://127.0.0.1:8080/export.csv?source=openrouter&model=claude'
```

Columns include source, provider, model, USD/sats, blended price, delta, endpoint, context, timestamps.

## API examples

```bash
# Health
curl -s localhost:8080/health | jq

# Best Now + counts
curl -s localhost:8080/api/summary | jq '.best_now[:5]'

# Filtered quotes
curl -s 'localhost:8080/api/quotes?preset=fastest&limit=20' | jq '.[].model'

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
| Empty board | Wait for first OpenRouter poll; check logs with `-v` |
| No nodes | Nostr blocked? Use `--node URL` or wait for kind 38421 events |
| Sats show `—` | BTC/USD fetch failed (Coinbase); USD columns still work |
| Deltas always `—` | Need a second catalog poll after prices change |
| Stale data after restart | Confirm `--db` path; avoid `--no-persist` if you want restore |

## Protocol references

- [RIP-02 Discovery](https://github.com/Routstr/protocol/blob/main/RIP-02.md) — kind 38421
- [RIP-05 Pricing](https://github.com/Routstr/protocol/blob/main/RIP-05.md) — units & cost model
- [Routstr docs](https://docs.routstr.com/)
