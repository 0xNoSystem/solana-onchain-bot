# engine-rs

Rust trading engine for Solana tokens with a terminal UI (`bot` binary), per-market strategy execution, and Jupiter-based execution.

## Setup

1. Create your env file:

```bash
cp .env.exemple .env
```

2. Edit `.env` and set:

```env
RPC_URL=https://api.mainnet-beta.solana.com
WS_URL=wss://api.mainnet-beta.solana.com
BIRDEYE_API_KEY=your_birdeye_api_key
```

3. Put `keypair.json` in the project root (or set `KEYPAIR_PATH` if you use another location).

Notes:
- `keypair.json` is the wallet used to submit swaps.
- Do not commit private keys or real API keys.

## Run

### Main TUI bot

```bash
cargo run --bin bot
```

Basic controls:
- `a`: add market
- `d`: remove selected market
- `p`: pause selected market
- `j/k` or arrows: move selection
- `q`: quit

Mouse:
- Click wallet address to copy
- Click selected market CA to copy

### Other binaries

```bash
cargo run --bin nos
cargo run --bin fetch
cargo run --bin pyth
```

## Technical Overview (Brief)

- `Bot` manages global state, margin allocation, and market lifecycle.
- Each `Market` runs:
  - `SignalEngine` (strategy + indicators + intents)
  - `Executor` (Jupiter swap submit/poll/fill/close)
  - Candle stream task (`StreamManager`)
- `bot` TUI reads periodic snapshots from `Bot` and renders:
  - market state
  - realized session PnL
  - live UPNL when a position is open
