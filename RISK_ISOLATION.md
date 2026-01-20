# 🛡️ Risk Isolation & Demo Trading Guide

## ⚠️ CRITICAL: Unified Trading Account (UTA) Margin Sharing

### Understanding UTA Risk

**IMPORTANT**: Bybit's Unified Trading Account (UTA) **shares margin across ALL products**:

```
┌─────────────────────────────────────────┐
│     Unified Trading Account (UTA)       │
├─────────────────────────────────────────┤
│                                         │
│  Spot ──┐                              │
│         ├──> SHARED MARGIN ◄──────┐    │
│  Futures┘                         │    │
│                                   │    │
│  Bot Trading ────────────────────┘    │
│                                         │
└─────────────────────────────────────────┘
```

**What This Means**:
- Bot losses can affect your Spot holdings
- Bot losses can affect other Futures positions
- If bot gets liquidated, it impacts entire UTA balance
- **NO ISOLATION on main account**

---

## ✅ RECOMMENDED: Use Subaccount for Isolation

### Why Subaccount is Safer

```
Main Account                    Subaccount (Bot Only)
┌────────────────┐             ┌────────────────┐
│                │             │                │
│ Spot: $10,000  │             │ Bot: $50 USDT  │ ◄── ISOLATED
│ Futures: $5,000│             │                │
│ Savings: $1,000│             │ Max Risk: $50  │
│                │             │                │
│ Total: $16,000 │             │ Main Safe: ✓   │
│                │             │                │
└────────────────┘             └────────────────┘
      ↑                                ↑
  PROTECTED                      EXPOSED
```

**Benefits**:
- ✅ Bot can only lose what you transfer to subaccount
- ✅ Main account holdings are 100% safe
- ✅ Clear separation of funds
- ✅ Easy to track bot performance
- ✅ Can disable bot API keys without affecting main account

---

## 🔧 Setup Guide

### Option 1: Demo Trading (Testing Only)

**Use Case**: Testing strategy, learning bot behavior, paper trading

**Setup**:
```bash
# In your .env file:
BYBIT_API_KEY=your_api_key
BYBIT_API_SECRET=your_api_secret
BYBIT_REST_URL=https://api-demo.bybit.com
BYBIT_WS_URL=wss://stream-demo.bybit.com/v5/public/linear
```

**Pros**:
- ✅ No real money at risk
- ✅ Same API as production
- ✅ Test without consequences

**Cons**:
- ⚠️ Still uses main account credentials
- ⚠️ Shares UTA margin (if bugs allow real trades)
- ⚠️ Demo prices may differ from real market
- ⚠️ Not 100% isolated from main account

**Recommendation**: Good for initial testing, but use Subaccount for real money

---

### Option 2: Subaccount (RECOMMENDED for Production)

**Use Case**: Real trading with isolated risk

**Step-by-Step Setup**:

#### 1. Create Subaccount on Bybit

1. Go to [Bybit](https://www.bybit.com)
2. Navigate to: `Account & Security` → `Sub-account Management`
3. Click "Create Sub-account"
4. Name it: `scalper-bot` (or any name)
5. Complete verification

#### 2. Transfer Limited Funds

1. Go to: `Assets` → `Transfer`
2. Transfer **ONLY the amount you're willing to lose**
   - Example: 50 USDT for testing
   - Example: 500 USDT for small-scale production
3. Transfer to: `scalper-bot` subaccount
4. Confirm transfer

#### 3. Create API Keys for Subaccount

1. Switch to subaccount: Top-right dropdown → Select `scalper-bot`
2. Go to: `API Management`
3. Click "Create New Key"
4. **CRITICAL**: Set permissions:
   - ✅ **Read**: Enabled
   - ✅ **Trade**: Enabled
   - ❌ **Withdraw**: **DISABLED** (very important!)
   - ❌ **Transfer**: **DISABLED** (very important!)
5. IP Whitelist (optional but recommended):
   - Add your server IP
   - Or leave blank if dynamic IP
6. Copy API Key and Secret immediately (shown only once!)

#### 4. Configure Bot

```bash
# In your .env file:
BYBIT_API_KEY=subaccount_api_key_here
BYBIT_API_SECRET=subaccount_api_secret_here

# For production (real trading)
# Leave BYBIT_REST_URL and BYBIT_WS_URL unset for mainnet

# Or for demo testing first:
BYBIT_REST_URL=https://api-demo.bybit.com
BYBIT_WS_URL=wss://stream-demo.bybit.com/v5/public/linear

# Risk parameters (adjust based on subaccount balance)
MAX_POSITION_SIZE_USD=50.0
STOP_LOSS_PERCENT=0.5
```

#### 5. Verify Isolation

Before running bot:
```bash
# 1. Check subaccount balance
# Go to Bybit → Select subaccount → Assets
# Confirm only intended amount is there

# 2. Check main account balance
# Switch to main account
# Confirm full balance is intact

# 3. Run bot
cargo run --release

# 4. Monitor both accounts
# Main account balance should NOT change
# Only subaccount balance should fluctuate
```

---

## 📊 Risk Comparison

| Scenario | Risk Exposure | Main Account Safe? | Recommendation |
|----------|---------------|--------------------|--------------------|
| **Main Account UTA** | Entire UTA balance | ❌ NO | ⚠️ NOT RECOMMENDED |
| **Demo Trading** | Simulated only | ⚠️ MOSTLY (still shares credentials) | ✅ For testing only |
| **Subaccount (50 USDT)** | Only 50 USDT | ✅ YES | ✅ RECOMMENDED |
| **Subaccount (500 USDT)** | Only 500 USDT | ✅ YES | ✅ For production |

---

## 🚨 Safety Checklist

Before running bot with real money:

### Pre-Launch Checklist

- [ ] Created separate subaccount
- [ ] Transferred LIMITED funds only (e.g., 50-500 USDT)
- [ ] API keys created for SUBACCOUNT (not main)
- [ ] **Withdraw permission: DISABLED**
- [ ] **Transfer permission: DISABLED**
- [ ] Tested on Demo Trading first
- [ ] Verified bot compiles: `cargo build --release`
- [ ] Reviewed logs: `RUST_LOG=debug cargo run`
- [ ] Checked stop loss is set: `STOP_LOSS_PERCENT=0.5`
- [ ] Position size is reasonable: `MAX_POSITION_SIZE_USD=50.0`
- [ ] Monitoring plan in place

### Post-Launch Monitoring

- [ ] Check subaccount balance every hour (first day)
- [ ] Monitor logs for errors
- [ ] Watch for unexpected behavior
- [ ] Keep main account credentials secure
- [ ] Have emergency stop plan (disable API keys)

---

## 🛑 Emergency Stop Procedures

### If Bot Misbehaves

**Option 1: Disable API Keys (FASTEST)**
1. Go to Bybit → API Management
2. Find bot's API key
3. Click "Delete" or "Disable"
4. Bot will immediately lose access

**Option 2: Kill Bot Process**
```bash
# Find bot process
ps aux | grep bybit-scalper

# Kill it
kill -9 <process_id>

# Or use Ctrl+C if running in foreground
```

**Option 3: Close Positions Manually**
1. Go to Bybit → Positions
2. Switch to subaccount
3. Close all open positions manually
4. Then disable API or kill bot

---

## 💰 Recommended Starting Amounts

| Experience Level | Subaccount Balance | Max Position Size | Risk Level |
|------------------|--------------------|--------------------|------------|
| **First Time** | 50 USDT | 10-20 USDT | Very Low |
| **Testing** | 100-200 USDT | 50 USDT | Low |
| **Confident** | 500 USDT | 100-200 USDT | Medium |
| **Production** | 1000+ USDT | 500+ USDT | Higher |

**Rule of Thumb**: Never risk more than you can afford to lose completely

---

## 📖 Environment Variables Reference

### Mainnet (Production)
```bash
BYBIT_API_KEY=subaccount_key
BYBIT_API_SECRET=subaccount_secret
# No BYBIT_REST_URL or BYBIT_WS_URL (defaults to mainnet)
```

### Demo Trading (Testing)
```bash
BYBIT_API_KEY=your_key
BYBIT_API_SECRET=your_secret
BYBIT_REST_URL=https://api-demo.bybit.com
BYBIT_WS_URL=wss://stream-demo.bybit.com/v5/public/linear
```

### Testnet (Separate Environment)
```bash
BYBIT_API_KEY=testnet_key
BYBIT_API_SECRET=testnet_secret
BYBIT_TESTNET=true
```

---

## ❓ FAQ

### Q: Can I use main account for small tests?
**A**: Not recommended. Even small bugs can lead to large losses if UTA has significant balance. Always use subaccount.

### Q: Is Demo Trading 100% safe?
**A**: No. Demo mode still uses your credentials and technically shares UTA. Use subaccount for real isolation.

### Q: How much should I start with?
**A**: Start with 50 USDT on a subaccount. If bot performs well for 1 week, consider increasing.

### Q: What if bot gets liquidated?
**A**: If using subaccount, only subaccount balance is lost. Main account is safe.

### Q: Can I transfer funds between accounts while bot runs?
**A**: Yes, but transfers are manual. Bot can't transfer funds if API permissions are set correctly.

### Q: Should I use IP whitelist?
**A**: Recommended for static IPs. Provides extra security layer. Not critical if server IP changes frequently.

---

## 🎯 Best Practice Summary

1. ✅ **ALWAYS use subaccount** for any real trading
2. ✅ **Start small** (50-100 USDT)
3. ✅ **Disable withdraw/transfer** on API keys
4. ✅ **Test on Demo first** before using real money
5. ✅ **Monitor actively** in first 24 hours
6. ✅ **Have emergency stop plan**
7. ✅ **Keep main account separate**

---

## 📞 Support

If you experience issues:
1. Check logs: `tail -f bot.log`
2. Review this guide
3. Disable API keys immediately if concerned
4. Contact Bybit support for account issues

**Remember**: This bot is experimental. Only risk what you can afford to lose completely.
