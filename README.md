# Polymarket Trading Bot – BTC (&ETH) Binary Markets

**Rust trading bot for Polymarket prediction markets.** Trades BTC and ETH binary (Up/Down) markets (15m and 1h) using **outcome prices** (win probabilities from Gamma) and **token price trend**. Places batches of limit orders on the side that is both winning by outcome price and rising in price over N datapoints.

[![Rust](https://img.shields.io/badge/rust-1.70%2B-orange.svg)](https://www.rust-lang.org/)
[![Polymarket](https://img.shields.io/badge/Polymarket-CLOB-blue)](https://polymarket.com)

---

## Features

- **Dual timeframe:** Trades **15m** and **1h** binary markets for BTC and ETH on [Polymarket](https://polymarket.com).
- **Lock rule:** Buys the opposite side only when **cost per pair** stays under your cap (e.g. Up_avg + Down_ask ≤ 0.99), so you lock in edge.
- **Expansion rule:** When you can’t lock (cost would exceed max) but the other side is **rising** and its outcome PnL is worse, the bot buys that side to improve exposure (see [logic.md](logic.md)).
- **Ride the winner:** When one side is clearly winning (trend UpRising/DownRising), the bot adds to that side to grow “PnL if that side wins.”
- **PnL rebalance:** If one outcome’s PnL is negative and you’re not strongly trending the other way, it buys the weak side (within cost limits).
- **Flat = no trade:** When the short-term trend is **Flat** (no clear move), the bot only locks if conditions allow; otherwise it does not open new risk (Example 6 in [logic.md](logic.md)).
- **1h throttle:** Configurable cooldown for 1h markets (e.g. 30–45s between buys) to avoid overtrading.
- **Simulation mode:** Run with `--simulation` (default) to log trades without sending orders.
- **Market resolution:** Checks for closed markets, computes actual PnL, and logs “Total actual PnL (all time).”
- **Timestamped logs:** Price feed and history lines include `[YYYY-MM-DDTHH:MM:SS]` for easier debugging and backtesting.

---

## Strategy Overview

The bot keeps **positions** per market (Up shares, Down shares, average prices). Each tick it:

1. Updates **trend** from the last 4–5 price points (UpRising, DownRising, Flat, UpFalling, DownFalling).
2. **Lock:** If adding the underweight side keeps cost per pair ≤ `cost_per_pair_max` → buy that side (lock).
3. **Expansion:** If you *can’t* lock (cost would exceed max) but the other side is **rising** and “PnL if that side wins” is worse → buy that side (new leg / rebalance).
4. **Ride winner:** If trend is UpRising or DownRising (and not Flat) → buy the rising side (within cost and buy limits).
5. **PnL rebalance:** If one outcome’s PnL is negative and trend isn’t strongly the other way → buy the weak side (within limits).
6. **Trend fallback:** DownFalling → can buy Up; UpFalling → can buy Down (no buy on Flat except lock).

Detailed examples (no position, only Up, only Down, have both, Flat, market close) are in **[logic.md](logic.md)**.

---

## Requirements

- **Rust** 1.70+ (`rustc --version`)
- **Polymarket API** credentials (API key, secret, passphrase, and optionally private key / proxy wallet for live trading)

---

## Installation

```bash
git clone https://github.com/cakaroni/polymarket-trading-bot-btc-15m.git
cd polymarket-trading-bot-btc-15m
cargo build --release
```

---

## Configuration

Copy the example config and edit with your settings:

```bash
cp config.example.json config.json
# Edit config.json with your Polymarket API keys and trading parameters
```

**Important:** `config.json` is gitignored. Do not commit real API keys.

### Trading options (in `config.json` → `trading`)

| Option | Description | Example |
|--------|-------------|---------|
| `markets` | Assets to trade | `["btc", "eth"]` |
| `timeframes` | Periods | `["15m", "1h"]` |
| `trend_datapoints` | Number of price points to confirm "rising" trend | `7` |
| `trend_datapoints_extended` | Longer window when market unclear (reserved) | `15` |
| `batch_count` | Number of limit orders per batch | `5` |
| `shares_per_limit_order` | Shares per limit order in batch | `30` |
| `limit_order_price_offset_down` | Price offset for Down orders (e.g. −0.02) | `-0.02` |
| `cancel_unmatched_after_secs` | Cancel placed limit orders after N s (production only); 0 = never | `120` |
| `min_side_price` | Don’t place orders below this ask | `0.05` |
| `max_side_price` | Don’t place orders above this ask | `0.99` |
| `market_closure_check_interval_seconds` | How often to check for resolved markets | `20` |

---

## Usage

**Simulation (no real orders):**

```bash
cargo run -- --simulation
# or
cargo run --release -- --simulation
```

**Live trading (sends FAK orders to Polymarket):**

```bash
cargo run --release -- --production --config config.json
```

**Redeem winnings for a resolved market:**

```bash
cargo run --release -- --redeem --condition-id <CONDITION_ID>
```

Logs go to `history.toml` (and your configured log target). Price lines look like:

```text
[2026-02-04T23:17:23] BTC 15m Up Token BID:$0.52 ASK:$0.53 Down Token BID:$0.47 ASK:$0.48 remaining time:12m 34s
```

---

## Project structure

```text
.
├── Cargo.toml
├── config.json          # Your config (gitignored); use config.example.json as template
├── logic.md              # Legacy strategy examples (reference)
├── history.toml          # Append-only trade/price log (gitignored in default .gitignore)
├── src/
│   ├── main.rs           # CLI, config load, monitor + trader spawn
│   ├── config.rs         # Config and defaults
│   ├── api.rs            # Polymarket Gamma + CLOB API (markets/slug, limit orders, cancel)
│   ├── monitor.rs        # Price feed + Gamma outcome prices, snapshot, timestamps
│   ├── trader.rs         # Outcome-price + trend → batch limit orders; cancel unmatched
│   └── models.rs         # API and market data types (incl. outcome prices)
└── README.md
```

---

## Disclaimer

This bot is for **educational and research purposes**. Trading prediction markets involves risk. Past behavior in simulation or backtests does not guarantee future results. Use at your own risk. The authors are not responsible for any financial loss.

---

## Contact

- **Telegram:** [@cakaroni](https://t.me/cakaroni)  
- **Link:** https://t.me/cakaroni

For questions, support, or collaboration around this Polymarket trading bot, reach out via Telegram.

---

## Keywords (for search)

Polymarket trading bot, Polymarket bot, Polymarket automation, crypto prediction market bot, BTC prediction market, ETH binary options, Polymarket CLOB, Polymarket API, prediction market hedging, binary market bot, Rust Polymarket, Polymarket 15m, Polymarket 1h, Polymarket arbitrage, Polymarket hedging bot.
