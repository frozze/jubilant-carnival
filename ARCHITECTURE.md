# 🏗️ Architecture Deep Dive

## Actor Communication Flow

### Message Flow Diagram

```
ScannerActor (every 60s)
    │
    │ 1. Fetch all tickers via REST API
    │ 2. Calculate volatility scores
    │ 3. Find top coin
    │
    └──> MarketDataMessage::SwitchSymbol(Symbol)
              │
              ▼
         MarketDataActor
              │
              │ 1. Unsubscribe from old symbol
              │ 2. Send SymbolChanged to Strategy
              │ 3. Subscribe to new symbol
              │
              ├──> StrategyMessage::SymbolChanged(Symbol)
              │         │
              │         ▼
              │    StrategyEngine
              │         │
              │         └──> ExecutionMessage::ClosePosition
              │                   │
              │                   ▼
              │              ExecutionActor
              │                   │
              │                   └──> REST: Close position via Market Order
              │
              ├──> StrategyMessage::OrderBook(snapshot)
              │         │
              │         ▼
              │    StrategyEngine (update price, check stop loss)
              │
              └──> StrategyMessage::Trade(tick)
                        │
                        ▼
                   StrategyEngine
                        │
                        │ 1. Add to RingBuffer
                        │ 2. Calculate momentum
                        │ 3. Entry signal?
                        │
                        └──> ExecutionMessage::PlaceOrder(order)
                                  │
                                  ▼
                             ExecutionActor
                                  │
                                  └──> REST: Place order on Bybit
```

## Data Structures

### RingBuffer (Zero-Allocation)

```rust
pub struct RingBuffer<T> {
    buffer: Vec<Option<T>>,  // Fixed size, pre-allocated
    capacity: usize,
    head: usize,             // Write pointer
    size: usize,             // Current number of elements
}
```

**Performance characteristics:**
- O(1) push
- O(1) last
- No heap allocations after initialization
- Cache-friendly sequential access

### OrderBookSnapshot

```rust
pub struct OrderBookSnapshot {
    pub symbol: Symbol,
    pub timestamp: i64,           // milliseconds
    pub best_bid: Decimal,
    pub best_ask: Decimal,
    pub bid_size: Decimal,
    pub ask_size: Decimal,
    pub mid_price: Decimal,       // Computed: (bid + ask) / 2
    pub spread_bps: f64,          // Basis points: (spread / mid) * 10000
}
```

**Liquidity check:**
```rust
fn is_liquid(&self) -> bool {
    self.spread_bps < 10.0 &&
    self.bid_size > Decimal::from(100) &&
    self.ask_size > Decimal::from(100)
}
```

## Scanner Algorithm

### Volatility Score Formula

```
Score = Turnover_24h × |PriceChange_24h| × Whitelist_Boost

Where:
- Turnover_24h: USD volume in last 24h
- PriceChange_24h: Percentage price change (0.05 = 5%)
- Whitelist_Boost: 1.3 for SUI/WIF/VIRTUAL/RENDER/SEI/PEPE, 1.0 otherwise
```

### Symbol Switch Logic

```rust
should_switch = if current_symbol.exists() {
    new_score > current_score × THRESHOLD_MULTIPLIER &&
    new_symbol != current_symbol
} else {
    true  // No current symbol, switch immediately
}
```

**Example:**
- Current: SUIUSDT (score: 1.5e9)
- New top: VIRTUALUSDT (score: 2.0e9)
- Threshold: 1.2
- Decision: 2.0e9 > 1.5e9 × 1.2 = 1.8e9 → **Switch!**

## WebSocket Hot-Swap Implementation

### Subscription Management

```rust
async fn hot_swap(old: Symbol, new: Symbol) {
    // Step 1: Notify strategy to close positions
    strategy_tx.send(SymbolChanged(new)).await;

    // Step 2: Unsubscribe from old (keep connection alive)
    unsubscribe(&mut ws_write, &old).await;

    // Step 3: Subscribe to new
    subscribe(&mut ws_write, &new).await;
}
```

### Bybit WebSocket Messages

**Subscribe:**
```json
{
  "op": "subscribe",
  "args": [
    "orderbook.1.SUIUSDT",
    "publicTrade.SUIUSDT"
  ]
}
```

**Unsubscribe:**
```json
{
  "op": "unsubscribe",
  "args": [
    "orderbook.1.VIRTUALUSDT",
    "publicTrade.VIRTUALUSDT"
  ]
}
```

## Strategy Details

### Momentum Calculation

```rust
fn calculate_momentum(&self) -> Option<f64> {
    let last_20_ticks = self.tick_buffer.iter().rev().take(20);

    // Calculate VWAP
    let vwap = Σ(price × volume) / Σ(volume)

    // Momentum = (last_price - vwap) / vwap
    let momentum = (last_tick.price - vwap) / vwap

    Some(momentum)
}
```

**Example:**
- VWAP = $1.000
- Last price = $1.002
- Momentum = (1.002 - 1.000) / 1.000 = 0.002 = **0.2%** → Entry if > 0.1%

### Smart Order Routing

```rust
match (spread_bps, liquidity) {
    // Narrow spread + deep book = aggressive execution
    (spread, _) if spread < 10.0 && is_liquid => {
        (OrderType::Market, TimeInForce::IOC)
    },

    // Wide spread = try to capture maker rebate
    _ => {
        let limit_price = if side == Buy {
            best_bid  // Join the bid queue
        } else {
            best_ask  // Join the ask queue
        };
        (OrderType::Limit, TimeInForce::PostOnly)
    }
}
```

### Position Sizing

```rust
// Fixed USD value per trade
let position_value = MAX_POSITION_SIZE_USD;  // e.g., $1000

// Calculate quantity
let qty = position_value / mid_price;

// Example:
// - Max position: $1000
// - Price: $5.00
// - Qty: 1000 / 5 = 200 coins
```

### Stop Loss Placement

```rust
let stop_distance = entry_price × (STOP_LOSS_PERCENT / 100.0);

let stop_loss = match side {
    Long  => entry_price - stop_distance,
    Short => entry_price + stop_distance,
};

// Example (Long):
// - Entry: $10.00
// - Stop %: 0.5%
// - Stop Loss: $10.00 - ($10.00 × 0.005) = $9.95
```

## Execution Actor

### Order Placement with Retry

```rust
async fn place_order(&self, order: Order) -> Result<()> {
    let mut retries = 0;
    const MAX_RETRIES: u32 = 3;

    loop {
        match self.client.place_order(&order).await {
            Ok(response) => return Ok(response),

            Err(e) if is_network_error(&e) && retries < MAX_RETRIES => {
                retries += 1;
                let backoff = Duration::from_secs(2_u64.pow(retries));
                tokio::time::sleep(backoff).await;
            },

            Err(e) => return Err(e),
        }
    }
}
```

### Position Closing

```rust
async fn close_position(&self, symbol: Symbol, side: PositionSide) {
    // 1. Get current position from exchange
    let positions = self.client.get_position(&symbol).await?;

    // 2. Create reduce-only market order
    let close_order = Order {
        symbol,
        side: opposite_side(side),  // Long position → Sell order
        order_type: Market,
        qty: position.size,
        reduce_only: true,          // Safety: only close, don't open new
        time_in_force: IOC,
    };

    // 3. Execute
    self.client.place_order(&close_order).await?;
}
```

## Performance Optimizations

### 1. Zero-Copy Parsing

```rust
// Bad: Clone data
let text = msg.to_text()?;  // Allocates String
let parsed: WsMessage = serde_json::from_str(&text)?;

// Good: Borrow
let parsed: WsMessage = serde_json::from_str(msg.as_text()?)?;
```

### 2. Non-Blocking Channel Sends

```rust
// In hot path (WebSocket handler)
tokio::spawn(async move {
    let _ = strategy_tx.send(message).await;
});
// Returns immediately, send happens in background
```

### 3. Decimal Arithmetic

```rust
// Bad: f64 (imprecise for money)
let price: f64 = 1.0 / 3.0;  // 0.333333... (rounding errors)

// Good: Decimal (exact)
let price = Decimal::from(1) / Decimal::from(3);  // Exact representation
```

## Error Handling Strategy

### Network Errors

```rust
if status_code >= 500 && status_code < 600 {
    // Server error - retry with exponential backoff
    retry_with_backoff(2, 4, 8 seconds);
} else if status_code >= 400 && status_code < 500 {
    // Client error - don't retry (bad request)
    return Err(anyhow!("Client error: {}", status));
}
```

### WebSocket Disconnects

```rust
loop {
    match connect_and_stream().await {
        Ok(_) => break,  // Clean shutdown
        Err(e) => {
            error!("WebSocket error: {}", e);
            sleep(Duration::from_secs(5)).await;
            // Reconnect automatically
        }
    }
}
```

### Stale Data Protection

```rust
let now = Utc::now().timestamp_millis();
let data_age = now - tick.timestamp;

if data_age > 500 {  // 500ms threshold
    debug!("Ignoring stale tick (age: {}ms)", data_age);
    return;  // Skip processing
}
```

## Concurrency Model

### Actor Isolation

Each actor runs in its own `tokio::task`:
- **Independent failure domains**: One actor crash doesn't kill others
- **CPU parallelism**: Multi-core utilization via tokio's work-stealing scheduler
- **No shared state**: Communication only via message passing

### Channel Capacity Tuning

```rust
// Command channels (low throughput)
let (cmd_tx, cmd_rx) = mpsc::channel(32);

// Data channels (high throughput)
let (data_tx, data_rx) = mpsc::channel(1000);

// Execution channels (medium throughput)
let (exec_tx, exec_rx) = mpsc::channel(100);
```

## Logging Strategy

### Log Levels

- **ERROR**: Failed to place order, WebSocket disconnect
- **WARN**: Wide spread, position close before switch
- **INFO**: Scanner results, symbol switches, order fills
- **DEBUG**: Momentum calculations, orderbook updates
- **TRACE**: Raw WebSocket messages (very verbose)

### Structured Logging Example

```rust
info!(
    symbol = %coin.symbol,
    score = %coin.score,
    turnover = %coin.turnover_24h,
    "New top coin detected"
);
```

## Security Considerations

### API Key Management

```rust
// ✅ Good: Load from environment
let api_key = env::var("BYBIT_API_KEY")?;

// ❌ Bad: Hardcoded
let api_key = "xyz123...";  // NEVER DO THIS
```

### Signature Generation

```rust
fn sign(&self, timestamp: i64, params: &str) -> String {
    let sign_str = format!("{}{}{}", timestamp, api_key, params);
    let mut mac = HmacSha256::new_from_slice(api_secret.as_bytes())?;
    mac.update(sign_str.as_bytes());
    hex::encode(mac.finalize().into_bytes())
}
```

### Rate Limiting

Bybit limits:
- **REST API**: 50 requests/second (order endpoints)
- **WebSocket**: 500 messages/second

**Mitigation**: Bot naturally stays under limits due to 60s scan interval and event-driven execution.

## Testing Strategy

### Unit Tests

```rust
#[cfg(test)]
mod tests {
    #[test]
    fn test_momentum_calculation() {
        let mut buffer = RingBuffer::new(100);
        // Add test ticks...
        assert!(engine.calculate_momentum().is_some());
    }
}
```

### Integration Tests (Testnet)

```bash
# 1. Set testnet credentials in .env
BYBIT_TESTNET=true

# 2. Run bot
cargo run

# 3. Monitor logs for:
# - Successful scanner runs
# - WebSocket subscriptions
# - Order placements (on testnet)
```

## Deployment

### Production Checklist

- [ ] Test on testnet with real API
- [ ] Verify risk parameters (stop loss, position size)
- [ ] Set `BYBIT_TESTNET=false`
- [ ] Enable production logging (`RUST_LOG=info`)
- [ ] Build release binary: `cargo build --release`
- [ ] Run in background: `nohup ./target/release/bybit-scalper-bot &`
- [ ] Monitor logs: `tail -f nohup.out`

### Monitoring

```bash
# CPU and memory usage
htop -p $(pgrep bybit-scalper)

# Real-time logs
tail -f nohup.out | grep -E "ORDER|POSITION|SWITCH"
```

---

**For questions or improvements, please open an issue on GitHub.**
