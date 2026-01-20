# 🎯 Bybit Dynamic Scalper Bot

Production-ready HFT scalping bot for Bybit (Unified Trading Account) with dynamic asset switching.

## 🏗️ Architecture

### Actor Model Design

The bot uses a concurrent actor architecture with `tokio::sync::mpsc` channels:

```
┌─────────────────┐
│  ScannerActor   │ ──┐
│  (60s interval) │   │
└─────────────────┘   │
                      ▼
                ┌──────────────────┐
                │ MarketDataActor  │ ◄── WebSocket (Hot-Swap)
                │  (WS Manager)    │
                └──────────────────┘
                      │
                      ▼
                ┌──────────────────┐
                │ StrategyEngine   │
                │  (Scalping Core) │
                └──────────────────┘
                      │
                      ▼
                ┌──────────────────┐
                │ ExecutionActor   │
                │  (Order Manager) │
                └──────────────────┘
```

## 🔑 Key Features

### 1. **The "Predator" Scanner**
- Scans top-50 coins every 60 seconds
- Scoring formula: `Turnover24h * |PriceChange24h|`
- Whitelisted coins (SUI, WIF, VIRTUAL, RENDER, SEI, PEPE) receive 30% score boost
- Automatically switches to new leader if score exceeds `current * 1.2`

### 2. **Hot-Swap WebSocket**
- Maintains persistent connection
- Seamless symbol switching: `unsubscribe old → subscribe new`
- No connection drops during asset changes
- Zero-copy JSON parsing with `serde_json`

### 3. **Liquidity-Aware Execution**

**For liquid markets (narrow spread):**
- IOC Market Orders for instant execution

**For wide spreads:**
- PostOnly Limit Orders at best bid/ask
- Captures maker rebates
- Order chasing if price moves away

### 4. **Risk Management**
- In-memory stop loss (0.5% default)
- Take profit at 1.0% (configurable)
- Automatic position closure before symbol switch
- Stale data filtering (>500ms ignored)

## 📂 Project Structure

```
bybit-scalper-bot/
├── src/
│   ├── main.rs                 # Actor initialization & startup
│   ├── lib.rs                  # Module exports
│   ├── config.rs               # Configuration from .env
│   ├── actors/
│   │   ├── mod.rs
│   │   ├── messages.rs         # Inter-actor communication types
│   │   ├── scanner.rs          # Volatility scanner
│   │   ├── websocket.rs        # Market data feed (hot-swap)
│   │   ├── strategy.rs         # Trading logic
│   │   └── execution.rs        # Order placement
│   ├── exchange/
│   │   ├── mod.rs
│   │   └── bybit_client.rs     # REST API client with retry logic
│   └── models/
│       ├── mod.rs
│       └── types.rs            # Core data structures (RingBuffer, Order, Position)
├── Cargo.toml                  # Dependencies + Release optimizations
├── .env.example
└── README.md
```

## 🚀 Quick Start

### 1. Install Rust

```bash
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh
```

### 2. Clone & Configure

```bash
git clone <your-repo>
cd bybit-scalper-bot

# Copy and edit environment variables
cp .env.example .env
nano .env  # Add your Bybit API keys
```

### 3. Build & Run

**Development mode:**
```bash
cargo run
```

**Production mode (optimized):**
```bash
cargo build --release
./target/release/bybit-scalper-bot
```

## ⚙️ Configuration

Edit `.env` file:

| Variable | Description | Default |
|----------|-------------|---------|
| `BYBIT_API_KEY` | Your API key | - |
| `BYBIT_API_SECRET` | Your API secret | - |
| `BYBIT_TESTNET` | Use testnet | `true` |
| `MAX_POSITION_SIZE_USD` | Position size in USD | `1000.0` |
| `STOP_LOSS_PERCENT` | Stop loss % | `0.5` |
| `TAKE_PROFIT_PERCENT` | Take profit % | `1.0` |
| `SCAN_INTERVAL_SECS` | Scanner frequency | `60` |
| `MIN_TURNOVER_24H_USD` | Min 24h volume filter | `10000000` |
| `SCORE_THRESHOLD_MULTIPLIER` | Switch threshold | `1.2` |
| `MAX_SPREAD_BPS` | Max allowed spread (bps) | `20.0` |
| `STALE_DATA_THRESHOLD_MS` | Max data age (ms) | `500` |

## 📊 Performance Optimizations

### Cargo.toml Release Profile

```toml
[profile.release]
opt-level = 3          # Maximum optimization
lto = "fat"            # Link-time optimization
codegen-units = 1      # Single codegen unit for better optimization
strip = true           # Strip symbols (smaller binary)
panic = "abort"        # Abort on panic (no unwinding)
```

### Zero-Copy Data Structures

- **RingBuffer**: Fixed-size circular buffer (no allocations)
- **Decimal arithmetic**: `rust_decimal` for precise financial calculations
- **Message passing**: Lock-free `mpsc` channels

## 🛡️ Error Handling

- **API 502/504 errors**: Automatic retry with exponential backoff (2s, 4s, 8s)
- **WebSocket disconnects**: Auto-reconnect with 5s delay
- **No panics**: All errors handled via `anyhow::Result`

## 🔍 Logging

Uses `tracing` for structured logs:

```bash
# Set log level
export RUST_LOG=debug
cargo run

# Available levels: trace, debug, info, warn, error
```

## 🧪 Testing

**Testnet mode** (recommended for testing):
```bash
# In .env
BYBIT_TESTNET=true
```

**Get testnet API keys**: https://testnet.bybit.com

## ⚠️ Risk Disclaimer

- **This bot is for educational purposes only**
- Trading cryptocurrencies carries significant risk
- Never trade with funds you cannot afford to lose
- Always test thoroughly on testnet first
- Past performance does not guarantee future results

## 📈 Strategy Details

### Entry Conditions

1. **Momentum calculation**: VWAP of last 20 ticks
2. **Threshold**: Momentum > 0.1%
3. **Spread check**: Must be < configured max spread
4. **No existing position**

### Exit Conditions

1. **Stop Loss**: -0.5% from entry
2. **Take Profit**: +1.0% from entry
3. **Symbol switch**: Immediate market exit

### Smart Order Routing

```rust
if orderbook.spread_bps < 10.0 && orderbook.is_liquid() {
    // Use IOC Market Order
} else {
    // Use PostOnly Limit at best bid/ask
}
```

## 📝 License

MIT License - See LICENSE file for details

## 🤝 Contributing

Contributions welcome! Please:

1. Fork the repository
2. Create a feature branch
3. Add tests for new functionality
4. Submit a pull request

## 📧 Support

For issues and questions, please open a GitHub issue.

---

**Built with ❤️ and ⚡ Rust for maximum performance**
