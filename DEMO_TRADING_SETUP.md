# 🚀 Quick Start: Demo Trading Setup

## ⚡ Fast Configuration for Testing

### Step 1: Copy Environment Template

```bash
cp .env.example .env
```

### Step 2: Edit `.env` File

```bash
nano .env
```

### Step 3: Configure for Demo Trading

```env
# Your Bybit API credentials
BYBIT_API_KEY=your_actual_api_key
BYBIT_API_SECRET=your_actual_api_secret

# Demo Trading URLs (KEEP THESE FOR TESTING)
BYBIT_REST_URL=https://api-demo.bybit.com
BYBIT_WS_URL=wss://stream-demo.bybit.com/v5/public/linear

# Risk parameters (safe for testing)
MAX_POSITION_SIZE_USD=50.0
STOP_LOSS_PERCENT=0.5
TAKE_PROFIT_PERCENT=1.0
```

### Step 4: Run Bot

```bash
# With debug logs
RUST_LOG=debug cargo run --release

# Or production logs
RUST_LOG=info cargo run --release
```

---

## 📋 Configuration Options

### Environment Selection (choose ONE)

#### Option A: Demo Trading (RECOMMENDED FOR FIRST RUN)
```env
BYBIT_REST_URL=https://api-demo.bybit.com
BYBIT_WS_URL=wss://stream-demo.bybit.com/v5/public/linear
```

#### Option B: Mainnet Production (REAL MONEY)
```env
# Comment out or remove BYBIT_REST_URL and BYBIT_WS_URL
# Bot will default to mainnet
```

#### Option C: Testnet (Separate Environment)
```env
BYBIT_TESTNET=true
# Testnet URLs will be used automatically
```

---

## ⚠️ URL Priority Logic

The bot selects URLs in this order:

1. **Custom URLs** (BYBIT_REST_URL / BYBIT_WS_URL) → **HIGHEST PRIORITY**
2. Testnet URLs (if BYBIT_TESTNET=true)
3. Mainnet URLs (default)

**Example**:
```env
# This configuration uses DEMO, not testnet
BYBIT_TESTNET=true
BYBIT_REST_URL=https://api-demo.bybit.com  # ← This takes priority!
```

---

## 🛡️ CRITICAL: UTA Margin Sharing Warning

### ⚠️ Important Facts

1. **Demo Trading is NOT 100% isolated**
   - Uses your main account credentials
   - Shares Unified Trading Account (UTA) margin
   - Spot + Futures + Bot = SHARED MARGIN

2. **For TRUE isolation, use a Subaccount**
   - See: [RISK_ISOLATION.md](./RISK_ISOLATION.md)
   - Transfer limited funds (e.g., 50 USDT)
   - Main account stays 100% safe

### Risk Comparison

| Method | Isolation | Recommendation |
|--------|-----------|----------------|
| Main Account + Demo | ⚠️ Shared UTA | Testing only |
| Main Account + Real | ❌ NO ISOLATION | NOT RECOMMENDED |
| Subaccount + Demo | ✅ Isolated | Good for testing |
| Subaccount + Real | ✅ Fully Isolated | **BEST FOR PRODUCTION** |

---

## 🔍 Verify Configuration

### Check What URLs Bot Will Use

```bash
cargo run --release 2>&1 | head -10
```

Look for these lines:
```
✅ Configuration loaded
   - API URL: https://api-demo.bybit.com     ← Should be demo
   - WebSocket: wss://stream-demo.bybit.com... ← Should be demo
```

---

## 📊 Expected Behavior

### First Run (Demo Mode)

```bash
🚀 Bybit Dynamic Scalper Bot - Initializing...
✅ Configuration loaded
   - API URL: https://api-demo.bybit.com
   - WebSocket: wss://stream-demo.bybit.com/v5/public/linear
   - Max Position: $50
   - Stop Loss: 0.5%
   - Scan Interval: 60s
🔧 Setting up Actor System...
✅ All actors initialized
🔍 ScannerActor started
📡 MarketDataActor started
⚡ StrategyEngine started
💼 ExecutionActor started
🎯 Bot is now LIVE and hunting for opportunities!
━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━
🎯 Starting market scan...
📊 Top coin: SUIUSDT | Score: 1.23e9 | Turnover: $4.5e8 | Change: 2.75%
🔄 Switching to new coin: SUIUSDT (score: 0.00e0 -> 1.23e9)
```

### Successful Scan Output

You should see:
- ✅ "Top coin: XXXUSDT" (finds volatile coins)
- ✅ "Switching to new coin" (every 60s if leader changes)
- ✅ WebSocket subscriptions
- ✅ Trade ticks incoming

### ❌ Common Issues

**Issue**: `"⚠️  No suitable coins found in scan"`
- **Cause**: Scanner filter bug (should be fixed in latest version)
- **Fix**: Verify you're on commit `e7038fb` or later

**Issue**: `"10003 Sign invalid"`
- **Cause**: API signature bug
- **Fix**: Verify you're on commit `c62fd51` or later

**Issue**: `"Connection refused"`
- **Cause**: Wrong API URL
- **Fix**: Check BYBIT_REST_URL is correct

---

## 🔄 Switching to Production

### When You're Ready for Real Trading

1. **Stop bot**: Ctrl+C

2. **Create subaccount** (see RISK_ISOLATION.md)

3. **Transfer limited funds** (e.g., 50 USDT)

4. **Update `.env`**:
```env
# Use subaccount API keys
BYBIT_API_KEY=subaccount_key
BYBIT_API_SECRET=subaccount_secret

# Remove demo URLs for mainnet
# BYBIT_REST_URL=https://api-demo.bybit.com  ← COMMENT OUT
# BYBIT_WS_URL=wss://stream-demo.bybit.com... ← COMMENT OUT

# Adjust risk parameters
MAX_POSITION_SIZE_USD=50.0
STOP_LOSS_PERCENT=0.5
```

5. **Run bot**:
```bash
RUST_LOG=info cargo run --release
```

6. **Monitor closely** for first 24 hours

---

## 📖 Full Documentation

- **[RISK_ISOLATION.md](./RISK_ISOLATION.md)** - Detailed risk management guide
- **[README.md](./README.md)** - Full project documentation
- **[ARCHITECTURE.md](./ARCHITECTURE.md)** - Technical deep dive

---

## ✅ Quick Checklist

Before first run:
- [ ] Copied `.env.example` to `.env`
- [ ] Added real API key/secret
- [ ] Set demo URLs for testing
- [ ] Adjusted position size to safe amount
- [ ] Compiled: `cargo build --release`
- [ ] Read RISK_ISOLATION.md

Before production:
- [ ] Tested on demo for at least 1 day
- [ ] Created subaccount
- [ ] Transferred limited funds
- [ ] Updated .env with subaccount keys
- [ ] Removed demo URLs
- [ ] Have emergency stop plan

---

## 🆘 Emergency Stop

If bot misbehaves:

1. **Ctrl+C** (if running in terminal)
2. **Or disable API keys** at bybit.com → API Management
3. **Or kill process**: `pkill bybit-scalper`

---

**Questions?** See [RISK_ISOLATION.md](./RISK_ISOLATION.md) for detailed guide
