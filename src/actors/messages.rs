use crate::models::*;

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
    /// Symbol switched, close all positions
    SymbolChanged(Symbol),
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
