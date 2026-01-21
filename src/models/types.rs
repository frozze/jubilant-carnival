use rust_decimal::Decimal;
use rust_decimal::prelude::ToPrimitive;
use serde::{Deserialize, Serialize};
use std::fmt;

/// Core trading symbol representation
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct Symbol(pub String);

impl fmt::Display for Symbol {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

impl From<String> for Symbol {
    fn from(s: String) -> Self {
        Symbol(s)
    }
}

impl From<&str> for Symbol {
    fn from(s: &str) -> Self {
        Symbol(s.to_string())
    }
}

/// Market volatility score
#[derive(Debug, Clone, Copy, PartialEq, PartialOrd)]
pub struct VolatilityScore {
    pub symbol_name: &'static str,
    pub score: f64,
    pub turnover_24h: f64,
    pub price_change_24h: f64,
}

/// Real-time orderbook snapshot
#[derive(Debug, Clone)]
pub struct OrderBookSnapshot {
    pub symbol: Symbol,
    pub timestamp: i64,
    pub best_bid: Decimal,
    pub best_ask: Decimal,
    pub bid_size: Decimal,
    pub ask_size: Decimal,
    pub mid_price: Decimal,
    pub spread_bps: f64, // basis points
}

impl OrderBookSnapshot {
    pub fn new(
        symbol: Symbol,
        timestamp: i64,
        best_bid: Decimal,
        best_ask: Decimal,
        bid_size: Decimal,
        ask_size: Decimal,
    ) -> Self {
        let mid_price = (best_bid + best_ask) / Decimal::from(2);
        let spread = best_ask - best_bid;
        // ✅ FIXED: Direct ToPrimitive conversion (100x faster than .to_string().parse())
        let spread_bps = if mid_price > Decimal::ZERO {
            (spread / mid_price * Decimal::from(10000))
                .to_f64()
                .unwrap_or(0.0)
        } else {
            0.0
        };

        Self {
            symbol,
            timestamp,
            best_bid,
            best_ask,
            bid_size,
            ask_size,
            mid_price,
            spread_bps,
        }
    }

    /// Check if orderbook is liquid enough for market orders
    pub fn is_liquid(&self) -> bool {
        self.spread_bps < 10.0 && self.bid_size > Decimal::from(100) && self.ask_size > Decimal::from(100)
    }

    /// Check if orderbook is deeply liquid (safe for PostOnly with fallback)
    /// Stricter requirements: tight spread + substantial size on both sides
    pub fn is_deeply_liquid(&self) -> bool {
        // Spread < 5 bps (very tight)
        // Both sides have at least $500 worth at best price
        let min_size_usd = Decimal::from(500);
        let bid_value = self.bid_size * self.best_bid;
        let ask_value = self.ask_size * self.best_ask;

        self.spread_bps < 5.0
            && bid_value >= min_size_usd
            && ask_value >= min_size_usd
    }
}

/// Trade tick data
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TradeTick {
    pub symbol: Symbol,
    pub price: Decimal,
    pub size: Decimal,
    pub timestamp: i64,
    pub side: TradeSide,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
pub enum TradeSide {
    Buy,
    Sell,
}

/// Position state
#[derive(Debug, Clone)]
pub struct Position {
    pub symbol: Symbol,
    pub side: PositionSide,
    pub size: Decimal,
    pub entry_price: Decimal,
    pub current_price: Decimal,
    pub unrealized_pnl: Decimal,
    pub stop_loss: Option<Decimal>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum PositionSide {
    Long,
    Short,
}

impl Position {
    pub fn pnl_percent(&self) -> f64 {
        if self.entry_price == Decimal::ZERO {
            return 0.0;
        }

        let pnl_ratio = match self.side {
            PositionSide::Long => (self.current_price - self.entry_price) / self.entry_price,
            PositionSide::Short => (self.entry_price - self.current_price) / self.entry_price,
        };

        // ✅ FIXED: Direct ToPrimitive conversion (100x faster than .to_string().parse())
        (pnl_ratio * Decimal::from(100))
            .to_f64()
            .unwrap_or(0.0)
    }

    pub fn should_stop_loss(&self) -> bool {
        if let Some(sl) = self.stop_loss {
            match self.side {
                PositionSide::Long => self.current_price <= sl,
                PositionSide::Short => self.current_price >= sl,
            }
        } else {
            false
        }
    }
}

/// Order types
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Order {
    pub symbol: Symbol,
    pub side: OrderSide,
    pub order_type: OrderType,
    pub qty: Decimal,
    pub price: Option<Decimal>,
    pub time_in_force: TimeInForce,
    pub reduce_only: bool,
    /// Step size for qty rounding (e.g., "0.1" or "0.01")
    pub qty_step: Option<Decimal>,
    /// Tick size for price rounding (e.g., "0.0001")
    pub tick_size: Option<Decimal>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "PascalCase")]
pub enum OrderSide {
    Buy,
    Sell,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum OrderType {
    Market,
    Limit,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum TimeInForce {
    GTC,  // Good Till Cancel
    IOC,  // Immediate Or Cancel
    PostOnly, // Maker only
}

/// Ring buffer for tick storage (zero-allocation, fixed size)
pub struct RingBuffer<T> {
    buffer: Vec<Option<T>>,
    capacity: usize,
    head: usize,
    size: usize,
}

impl<T: Clone> RingBuffer<T> {
    pub fn new(capacity: usize) -> Self {
        let mut buffer = Vec::with_capacity(capacity);
        buffer.resize_with(capacity, || None);

        Self {
            buffer,
            capacity,
            head: 0,
            size: 0,
        }
    }

    pub fn push(&mut self, item: T) {
        self.buffer[self.head] = Some(item);
        self.head = (self.head + 1) % self.capacity;
        if self.size < self.capacity {
            self.size += 1;
        }
    }

    pub fn last(&self) -> Option<&T> {
        if self.size == 0 {
            return None;
        }
        let idx = if self.head == 0 {
            self.capacity - 1
        } else {
            self.head - 1
        };
        self.buffer[idx].as_ref()
    }

    pub fn iter(&self) -> impl Iterator<Item = &T> {
        self.buffer.iter().filter_map(|x| x.as_ref())
    }

    pub fn len(&self) -> usize {
        self.size
    }

    pub fn is_empty(&self) -> bool {
        self.size == 0
    }
}
