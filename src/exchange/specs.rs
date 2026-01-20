//! Instrument Specifications Module
//! 
//! Fetches and caches qtyStep/tickSize for each trading pair from Bybit API.
//! Automatically loads specs when a new symbol is selected.

use crate::exchange::bybit_client::InstrumentInfo;
use anyhow::Result;
use dashmap::DashMap;
use rust_decimal::Decimal;
use std::str::FromStr;
use std::sync::Arc;
use tracing::{debug, error, info, warn};

/// Cached precision specs for a symbol
#[derive(Debug, Clone)]
pub struct SymbolSpecs {
    pub symbol: String,
    pub qty_step: Decimal,
    pub min_order_qty: Decimal,
    pub max_order_qty: Decimal,
    pub tick_size: Decimal,
}

impl SymbolSpecs {
    /// Round quantity to valid step size
    pub fn round_qty(&self, qty: Decimal) -> Decimal {
        if self.qty_step.is_zero() {
            return qty;
        }
        (qty / self.qty_step).floor() * self.qty_step
    }
    
    /// Round price to valid tick size
    pub fn round_price(&self, price: Decimal) -> Decimal {
        if self.tick_size.is_zero() {
            return price;
        }
        (price / self.tick_size).floor() * self.tick_size
    }
    
    /// Ensure qty is within bounds
    pub fn clamp_qty(&self, qty: Decimal) -> Decimal {
        let rounded = self.round_qty(qty);
        if rounded < self.min_order_qty {
            self.min_order_qty
        } else if rounded > self.max_order_qty {
            self.max_order_qty
        } else {
            rounded
        }
    }
}

impl From<InstrumentInfo> for SymbolSpecs {
    fn from(info: InstrumentInfo) -> Self {
        Self {
            symbol: info.symbol,
            qty_step: Decimal::from_str(&info.lot_size_filter.qty_step).unwrap_or(Decimal::new(1, 2)),
            min_order_qty: Decimal::from_str(&info.lot_size_filter.min_order_qty).unwrap_or(Decimal::ZERO),
            max_order_qty: Decimal::from_str(&info.lot_size_filter.max_order_qty).unwrap_or(Decimal::MAX),
            tick_size: Decimal::from_str(&info.price_filter.tick_size).unwrap_or(Decimal::new(1, 4)),
        }
    }
}

/// Thread-safe specs cache with automatic fetching
#[derive(Clone)]
pub struct SpecsCache {
    cache: Arc<DashMap<String, SymbolSpecs>>,
}

impl SpecsCache {
    pub fn new() -> Self {
        Self {
            cache: Arc::new(DashMap::new()),
        }
    }
    
    /// Get specs for a symbol (returns None if not cached)
    pub fn get(&self, symbol: &str) -> Option<SymbolSpecs> {
        self.cache.get(symbol).map(|v| v.clone())
    }
    
    /// Store specs for a symbol
    pub fn insert(&self, specs: SymbolSpecs) {
        info!("📏 Cached specs for {}: qty_step={}, tick_size={}", 
              specs.symbol, specs.qty_step, specs.tick_size);
        self.cache.insert(specs.symbol.clone(), specs);
    }
    
    /// Check if symbol is cached
    pub fn contains(&self, symbol: &str) -> bool {
        self.cache.contains_key(symbol)
    }
    
    /// Get fallback specs if not cached (conservative defaults)
    pub fn get_or_default(&self, symbol: &str) -> SymbolSpecs {
        self.get(symbol).unwrap_or_else(|| {
            warn!("⚠️ Using fallback specs for {} (not cached)", symbol);
            SymbolSpecs {
                symbol: symbol.to_string(),
                qty_step: Decimal::new(1, 2),       // 0.01
                min_order_qty: Decimal::new(1, 2),  // 0.01
                max_order_qty: Decimal::MAX,
                tick_size: Decimal::new(1, 4),      // 0.0001
            }
        })
    }
}

impl Default for SpecsCache {
    fn default() -> Self {
        Self::new()
    }
}
