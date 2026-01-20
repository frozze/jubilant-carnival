# 📁 Project Structure

## Complete File Tree

```
bybit-scalper-bot/
├── Cargo.toml                      # Dependencies & Release optimizations
├── .env.example                    # Environment variable template
├── .gitignore                      # Git ignore patterns
├── README.md                       # User documentation
├── ARCHITECTURE.md                 # Deep dive technical docs
├── PROJECT_STRUCTURE.md            # This file
│
└── src/
    ├── main.rs                     # Entry point & actor initialization
    ├── lib.rs                      # Module exports
    ├── config.rs                   # Configuration from environment
    │
    ├── actors/                     # Actor Model implementation
    │   ├── mod.rs
    │   ├── messages.rs             # Inter-actor message types
    │   ├── scanner.rs              # ScannerActor: Volatility hunting
    │   ├── websocket.rs            # MarketDataActor: WebSocket + Hot-Swap
    │   ├── strategy.rs             # StrategyEngine: Trading logic
    │   └── execution.rs            # ExecutionActor: Order management
    │
    ├── exchange/                   # Bybit API integration
    │   ├── mod.rs
    │   └── bybit_client.rs         # REST client with auth & retries
    │
    └── models/                     # Core data structures
        ├── mod.rs
        └── types.rs                # Orders, Positions, RingBuffer, etc.
```

## File Descriptions

### Root Level

#### `Cargo.toml`
- **Purpose**: Rust project manifest
- **Key Features**:
  - Production dependencies (tokio, reqwest, serde)
  - Release profile with LTO & optimization level 3
  - Optional SIMD JSON parsing

#### `.env.example`
- **Purpose**: Environment variable template
- **Usage**: Copy to `.env` and fill in credentials
- **Contains**: API keys, trading params, risk limits

#### `README.md`
- **Purpose**: User-facing documentation
- **Sections**: Quick start, configuration, architecture overview

#### `ARCHITECTURE.md`
- **Purpose**: Technical deep dive
- **Sections**: Data structures, algorithms, performance tuning

---

### `src/main.rs`

**Lines**: ~100
**Purpose**: Application entry point

**Key Responsibilities**:
1. Initialize logging (tracing)
2. Load configuration from `.env`
3. Create Bybit API client
4. Spawn all 4 actors as independent tasks
5. Setup graceful shutdown (Ctrl+C handler)

**Actor Communication Setup**:
```rust
Scanner → [channel] → MarketData → [channel] → Strategy → [channel] → Execution
```

---

### `src/config.rs`

**Lines**: ~70
**Purpose**: Configuration management

**Key Functions**:
- `Config::from_env()`: Load from environment variables
- `rest_api_url()`: Get REST endpoint (testnet vs production)
- `ws_url()`: Get WebSocket endpoint

**Environment Variables**:
- Credentials: `BYBIT_API_KEY`, `BYBIT_API_SECRET`
- Trading: `MAX_POSITION_SIZE_USD`, `STOP_LOSS_PERCENT`
- Scanner: `SCAN_INTERVAL_SECS`, `SCORE_THRESHOLD_MULTIPLIER`
- Risk: `MAX_SPREAD_BPS`, `STALE_DATA_THRESHOLD_MS`

---

### `src/actors/messages.rs`

**Lines**: ~50
**Purpose**: Inter-actor message definitions

**Message Types**:

1. **ScannerMessage**
   - `NewCoinDetected { symbol, score }`

2. **MarketDataMessage**
   - `SwitchSymbol(Symbol)` - Hot-swap command
   - `Shutdown`

3. **StrategyMessage**
   - `OrderBook(snapshot)` - Real-time orderbook
   - `Trade(tick)` - Trade tick
   - `PositionUpdate(position)` - From execution
   - `SymbolChanged(symbol)` - Close positions

4. **ExecutionMessage**
   - `PlaceOrder(order)`
   - `ClosePosition { symbol, side }`
   - `GetPosition(symbol)`

---

### `src/actors/scanner.rs`

**Lines**: ~150
**Purpose**: "Predator" volatility scanner

**Algorithm**:
1. Fetch all tickers from Bybit REST API (every 60s)
2. Filter out stablecoins, BTC, ETH
3. Calculate score: `Turnover × |PriceChange|`
4. Apply whitelist boost (30% for SUI, WIF, VIRTUAL, etc.)
5. If top coin score > current × 1.2 → switch

**Key Functions**:
- `run()`: Main loop with 60s interval
- `scan_and_select()`: Fetch, score, decide
- Error handling: Retry logic, no panic on API errors

---

### `src/actors/websocket.rs`

**Lines**: ~300
**Purpose**: WebSocket manager with Hot-Swap

**Features**:
1. **Persistent Connection**: Single WS connection, no drops
2. **Hot-Swap**: Unsubscribe old → Subscribe new (seamless)
3. **Data Streams**:
   - `orderbook.1.{symbol}` - Top-of-book updates
   - `publicTrade.{symbol}` - Trade ticks

**Key Functions**:
- `connect_and_stream()`: Main WS loop
- `subscribe()` / `unsubscribe()`: Channel management
- `handle_orderbook()` / `handle_trade()`: Parse & forward to strategy

**Stale Data Filter**: Ignores ticks older than 500ms

---

### `src/actors/strategy.rs`

**Lines**: ~250
**Purpose**: Scalping strategy core

**Entry Logic**:
1. Calculate momentum (VWAP of last 20 ticks)
2. If |momentum| > 0.1% → entry signal
3. Check spread < max allowed
4. Route order (market vs limit)

**Exit Logic**:
- Stop loss: -0.5%
- Take profit: +1.0%
- Symbol change: Immediate close

**Smart Order Routing**:
```rust
if orderbook.is_liquid() {
    // Narrow spread → Market IOC
} else {
    // Wide spread → PostOnly Limit (maker rebate)
}
```

**Key Functions**:
- `calculate_momentum()`: VWAP-based momentum
- `execute_entry()`: Build & send order
- `handle_symbol_change()`: Close positions on switch

---

### `src/actors/execution.rs`

**Lines**: ~150
**Purpose**: Order execution & position tracking

**Responsibilities**:
1. Place orders via Bybit REST API
2. Close positions (market orders with `reduce_only`)
3. Query current positions
4. Error handling & retry logic

**Key Functions**:
- `handle_place_order()`: Submit order to exchange
- `handle_close_position()`: Query position → create reduce-only order
- `handle_get_position()`: Fetch & notify strategy

**Safety**: All close orders use `reduce_only=true` flag

---

### `src/exchange/bybit_client.rs`

**Lines**: ~200
**Purpose**: Bybit REST API client

**Features**:
1. **Authentication**: HMAC-SHA256 signatures
2. **Retry Logic**: 3 retries with exponential backoff (2s, 4s, 8s)
3. **Error Handling**: Distinguish 4xx (client) vs 5xx (server) errors

**API Methods**:
- `get_tickers()`: Fetch all market tickers
- `place_order()`: Submit order
- `get_position()`: Query position for symbol

**Security**: API key/secret loaded from environment, never hardcoded

---

### `src/models/types.rs`

**Lines**: ~300
**Purpose**: Core data structures

**Key Types**:

1. **Symbol**: Wrapper around String (`BTCUSDT`)

2. **OrderBookSnapshot**:
   - Best bid/ask with sizes
   - Computed: mid price, spread in bps
   - Method: `is_liquid()` - checks spread + depth

3. **TradeTick**:
   - Price, size, timestamp, side (Buy/Sell)

4. **Position**:
   - Entry price, current price, unrealized PnL
   - Stop loss level
   - Methods: `pnl_percent()`, `should_stop_loss()`

5. **Order**:
   - Symbol, side, type (Market/Limit), qty
   - Time-in-force (GTC/IOC/PostOnly)
   - `reduce_only` flag

6. **RingBuffer\<T\>**:
   - Fixed-size circular buffer
   - Zero allocations after init
   - O(1) push/last operations

---

## Actor Diagram

```
┌──────────────────────────────────────────────────────────────┐
│                      Actor System                            │
│                                                              │
│  ┌──────────────┐                                           │
│  │   Scanner    │  Every 60s                                │
│  │   Actor      │  ────────────┐                            │
│  └──────────────┘              │                            │
│         │                       │                            │
│         │ SwitchSymbol          ▼                            │
│         │               ┌──────────────┐                     │
│         └──────────────►│  MarketData  │                     │
│                         │    Actor     │                     │
│                         │ (WebSocket)  │                     │
│                         └──────────────┘                     │
│                                │                             │
│                                │ OrderBook / Trade           │
│                                │                             │
│                                ▼                             │
│                         ┌──────────────┐                     │
│                         │   Strategy   │                     │
│                         │    Engine    │                     │
│                         └──────────────┘                     │
│                                │                             │
│                                │ PlaceOrder / ClosePosition  │
│                                │                             │
│                                ▼                             │
│                         ┌──────────────┐                     │
│                         │  Execution   │                     │
│                         │    Actor     │                     │
│                         └──────────────┘                     │
│                                │                             │
│                                │ REST API                    │
│                                ▼                             │
│                         ┌──────────────┐                     │
│                         │    Bybit     │                     │
│                         │   Exchange   │                     │
│                         └──────────────┘                     │
└──────────────────────────────────────────────────────────────┘
```

## Channel Types & Capacity

| Channel | From → To | Type | Capacity | Purpose |
|---------|-----------|------|----------|---------|
| `market_data_cmd_tx` | Scanner → MarketData | Command | 32 | Symbol switches |
| `strategy_tx` | MarketData → Strategy | Data | 1000 | Ticks & orderbooks |
| `execution_tx` | Strategy → Execution | Command | 100 | Orders & closes |

## Performance Characteristics

### Latency Targets

- **Scanner**: 60s interval (not latency-sensitive)
- **WebSocket → Strategy**: <1ms processing
- **Strategy → Order**: <10ms decision
- **Order → Exchange**: Network latency (~50-200ms)

### Memory Profile

- **RingBuffer**: 100 ticks × ~100 bytes = 10KB
- **Channel buffers**: ~1MB total
- **Total runtime**: <50MB typical

### CPU Usage

- **Idle**: <1% (waiting for events)
- **Active trading**: 5-10% (single core)
- **Scanner tick**: Brief spike (HTTP request)

## Build & Run Commands

```bash
# Development (fast compile, debug symbols)
cargo run

# Release (optimized, production)
cargo build --release
./target/release/bybit-scalper-bot

# Check without building
cargo check

# Run tests
cargo test

# Benchmarks (if added)
cargo bench
```

## Configuration Files

### Minimal `.env`
```env
BYBIT_API_KEY=your_key
BYBIT_API_SECRET=your_secret
BYBIT_TESTNET=true
```

### Full `.env` (with all options)
```env
# Required
BYBIT_API_KEY=your_key
BYBIT_API_SECRET=your_secret

# Optional (defaults shown)
BYBIT_TESTNET=true
MAX_POSITION_SIZE_USD=1000.0
STOP_LOSS_PERCENT=0.5
TAKE_PROFIT_PERCENT=1.0
SCAN_INTERVAL_SECS=60
MIN_TURNOVER_24H_USD=10000000.0
SCORE_THRESHOLD_MULTIPLIER=1.2
MAX_SPREAD_BPS=20.0
STALE_DATA_THRESHOLD_MS=500
RUST_LOG=info
```

## Dependencies Summary

### Core Runtime
- **tokio**: Async runtime (multi-threaded)
- **tokio-tungstenite**: WebSocket client
- **reqwest**: HTTP client

### Serialization
- **serde**: Serialization framework
- **serde_json**: JSON parsing

### Crypto
- **hmac**: HMAC authentication
- **sha2**: SHA-256 hashing

### Data Structures
- **rust_decimal**: Precise decimal arithmetic
- **dashmap**: Concurrent HashMap
- **parking_lot**: Fast mutexes

### Utilities
- **anyhow**: Error handling
- **tracing**: Structured logging
- **dotenvy**: .env file loading
- **chrono**: Time handling

## Next Steps

1. **Copy `.env.example` → `.env`** and add credentials
2. **Test on testnet**: `BYBIT_TESTNET=true cargo run`
3. **Monitor logs**: Look for scanner output, symbol switches
4. **Verify orders**: Check Bybit testnet web UI for orders
5. **Production**: Set `BYBIT_TESTNET=false` when ready

---

**Questions?** See `README.md` or `ARCHITECTURE.md` for more details.
