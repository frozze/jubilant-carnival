use crate::models::*;
use crate::exchange::SymbolSpecs;

/// Messages between actors

#[derive(Debug, Clone)]
pub enum ScannerMessage {
    /// New top coin detected
    NewCoinDetected { symbol: Symbol, score: f64 },
}

#[derive(Debug, Clone)]
pub enum MarketDataMessage {
    /// Switch to new symbol
    SwitchSymbol(Symbol),
    /// Shutdown command
    Shutdown,
}

#[derive(Debug, Clone)]
pub enum StrategyMessage {
    /// New orderbook snapshot
    OrderBook(OrderBookSnapshot),
    /// New trade tick
    Trade(TradeTick),
    /// Position update from execution
    PositionUpdate(Option<Position>),
    /// Symbol switched with new specs and 24h price change
    SymbolChanged {
        symbol: Symbol,
        specs: SymbolSpecs,
        price_change_24h: f64, // Daily price change percentage (e.g., 0.25 = +25%)
    },

    // ✅ CRITICAL: Feedback from execution to prevent order spam
    /// Order successfully placed and filled
    OrderFilled(Symbol),
    /// Order placement failed
    OrderFailed(String),

    // ✅ HARMONY: Live update of market stats (e.g. 24h change) without resetting state
    /// Updates market statistics for the current symbol
    UpdateMarketStats {
        symbol: Symbol,
        price_change_24h: f64,
    },
}

#[derive(Debug, Clone)]
pub enum ExecutionMessage {
    /// Place a new order
    PlaceOrder(Order),
    /// Close position immediately (market order)
    ClosePosition { symbol: Symbol, position_side: PositionSide },
    /// Request current position
    GetPosition(Symbol),
    /// Shutdown
    Shutdown,
}

#[derive(Debug, Clone)]
pub enum ExecutionResponse {
    /// Order placed successfully
    OrderPlaced { order_id: String },
    /// Position closed
    PositionClosed,
    /// Current position state
    CurrentPosition(Option<Position>),
    /// Error occurred
    Error(String),
}
