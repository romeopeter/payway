use rust_decimal::Decimal;
use std::collections::HashMap;
use std::str::FromStr;

use crate::error::AppError;

// SimulatedFxProvider returns deterministic rates from a hardcoded table.
// In a real system this would talk to a market data feed (Bloomberg, Reuters,
// or a settlement bank's quote API). For tests we want determinism, so this
// has no jitter — the rate for (NGN, USD) is always the same.
//
// Adding a new pair = adding an entry to `default_rates()` plus the matching
// `currencies` row + clearing accounts (see learn/schema-design.md §3).
pub struct SimulatedFxProvider {
    rates: HashMap<(String, String), Decimal>,
}

impl SimulatedFxProvider {
    pub fn new() -> Self {
        Self {
            rates: default_rates(),
        }
    }

    pub fn quote(&self, from: &str, to: &str) -> Result<Decimal, AppError> {
        self.rates
            .get(&(from.to_string(), to.to_string()))
            .copied()
            .ok_or_else(|| AppError::UnsupportedFxPair(from.to_string(), to.to_string()))
    }
}

impl Default for SimulatedFxProvider {
    fn default() -> Self {
        Self::new()
    }
}

fn default_rates() -> HashMap<(String, String), Decimal> {
    let mut m = HashMap::new();

    // 8-decimal precision matches fx_quotes.rate column.
    let pairs: &[(&str, &str, &str)] = &[
        // NGN as base
        ("NGN", "USD", "0.00062500"),
        ("NGN", "EUR", "0.00057800"),
        ("NGN", "GBP", "0.00049700"),
        // Inverse pairs (for reversal flows or future use)
        ("USD", "NGN", "1600.00000000"),
        ("EUR", "NGN", "1730.00000000"),
        ("GBP", "NGN", "2012.00000000"),
        // Cross rates
        ("USD", "EUR", "0.92500000"),
        ("USD", "GBP", "0.79500000"),
        ("EUR", "USD", "1.08100000"),
        ("EUR", "GBP", "0.85900000"),
        ("GBP", "USD", "1.25800000"),
        ("GBP", "EUR", "1.16400000"),
    ];

    for (from, to, rate) in pairs {
        m.insert(
            (from.to_string(), to.to_string()),
            Decimal::from_str(rate).expect("hardcoded rate must parse"),
        );
    }
    m
}
