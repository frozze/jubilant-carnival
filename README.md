# 🎯 Bybit Dynamic Scalper Bot

Production-ready HFT scalping bot for Bybit Linear Perpetuals with dynamic asset switching.

## 🏗️ Architecture

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

### 1. **Dynamic Asset Scanner**
- Scans all USDT perpetual pairs every 60 seconds
- Pure scoring formula: `Turnover24h × |PriceChange24h|`
- Auto-switches to new leader if score exceeds `current × 1.2`
- Excludes stablecoins (USDC, BUSD, DAI, TUSD) and low-volatility pairs (BTC, ETH)

### 2. **Hot-Swap WebSocket**
- Seamless symbol switching without connection drops
- Stale data filtering (>500ms ignored)
- Backpressure-aware tick delivery

### 3. **Liquidity-Aware Execution**
- **Liquid markets**: IOC Market Orders for instant execution
- **Wide spreads**: PostOnly Limit Orders at best bid/ask (captures maker rebates)

### 4. **Safety Features**
- **Order Timeout**: 10s automatic unfreeze if execution hangs
- **Live Score Tracking**: Detects when current asset "dies" and forces switch
- **Stop Loss/Take Profit**: Configurable risk management

## 🚀 Quick Start

### Local Development

```bash
# Install Rust
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh

# Clone and configure
git clone <your-repo>
cd bybit-scalper-bot
cp .env.example .env
nano .env  # Add your Bybit API keys

# Run
cargo run
```

### Docker Deployment

```bash
# Build and run
docker compose up -d

# View logs
docker logs -f bybit-scalper

# Stop
docker compose down
```

### CI/CD Deployment

1. Add secrets to GitHub repository:
   - `SERVER_HOST` - Your server IP
   - `SERVER_USER` - SSH username
   - `SERVER_SSH_KEY` - Private SSH key
   - `DEPLOY_PATH` - Path to docker-compose.yml on server

2. Push to `main` branch - automatic build & deploy

## ⚙️ Configuration

| Variable | Description | Default |
|----------|-------------|---------|
| `BYBIT_API_KEY` | Your API key | - |
| `BYBIT_API_SECRET` | Your API secret | - |
| `MAX_POSITION_SIZE_USD` | Position size in USD | `1000.0` |
| `STOP_LOSS_PERCENT` | Stop loss % | `0.5` |
| `TAKE_PROFIT_PERCENT` | Take profit % | `1.0` |
| `SCAN_INTERVAL_SECS` | Scanner frequency | `60` |
| `MIN_TURNOVER_24H_USD` | Min 24h volume filter | `10000000` |
| `MAX_SPREAD_BPS` | Max allowed spread (bps) | `20.0` |

## 📊 Strategy Details

### Entry Conditions
1. **Momentum**: VWAP deviation of last 50 ticks > 0.1%
2. **Spread**: Below configured max spread
3. **No existing position**

### Exit Conditions
1. **Stop Loss**: Default -0.5% from entry
2. **Take Profit**: Default +1.0% from entry
3. **Symbol switch**: Immediate market exit

## 🔧 Project Structure

```
src/
├── main.rs              # Actor initialization
├── config.rs            # Environment configuration
├── actors/
│   ├── scanner.rs       # Volatility scanner
│   ├── websocket.rs     # Market data feed
│   ├── strategy.rs      # Trading logic
│   └── execution.rs     # Order placement
├── exchange/
│   └── bybit_client.rs  # REST API client
└── models/
    └── types.rs         # Core data structures
```

## ⚠️ Risk Disclaimer

- **Educational purposes only**
- Never trade with funds you cannot afford to lose
- Always test on testnet first
- Past performance does not guarantee future results

## 📝 License

MIT License

---

**Built with ❤️ and ⚡ Rust for maximum performance**
