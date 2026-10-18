//! Token cost estimation.
//!
//! Prices are data, not code: each model gets an entry in the user's config.
//! The engine only needs the four per-million-token rates to turn a provider
//! `usage` block into a USD estimate, which `--max-cost` and `turn_ended`
//! report. Nothing here is provider-specific.

use serde::{Deserialize, Serialize};

use crate::event::Usage;

/// USD per one million tokens.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize, Default)]
pub struct Pricing {
    pub input_usd_per_mtok: f64,
    pub output_usd_per_mtok: f64,
    #[serde(default)]
    pub cache_read_usd_per_mtok: f64,
    #[serde(default)]
    pub cache_write_usd_per_mtok: f64,
}

impl Pricing {
    /// Estimate one provider call. Missing counters count as zero; cache rates
    /// default to zero, i.e. cached tokens are free unless configured.
    pub fn cost_usd(&self, usage: &Usage) -> f64 {
        const MTOK: f64 = 1_000_000.0;
        let tokens = |count: Option<u64>| count.unwrap_or(0) as f64;
        let input = tokens(usage.input_tokens) * self.input_usd_per_mtok;
        let output = tokens(usage.output_tokens) * self.output_usd_per_mtok;
        let cache_read = tokens(usage.cache_read_tokens) * self.cache_read_usd_per_mtok;
        let cache_write = tokens(usage.cache_write_tokens) * self.cache_write_usd_per_mtok;
        (input + output + cache_read + cache_write) / MTOK
    }

    /// Reject NaN, infinity and negative rates before they reach the engine.
    pub fn validate(&self) -> bool {
        [
            self.input_usd_per_mtok,
            self.output_usd_per_mtok,
            self.cache_read_usd_per_mtok,
            self.cache_write_usd_per_mtok,
        ]
        .iter()
        .all(|rate| rate.is_finite() && *rate >= 0.0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prices_input_and_output_tokens() {
        let pricing = Pricing {
            input_usd_per_mtok: 1.0,
            output_usd_per_mtok: 2.0,
            ..Pricing::default()
        };
        let usage = Usage {
            input_tokens: Some(1_000_000),
            output_tokens: Some(500_000),
            ..Usage::default()
        };
        assert!((pricing.cost_usd(&usage) - 2.0).abs() < 1e-12);
    }

    #[test]
    fn missing_counters_are_free() {
        let pricing = Pricing {
            input_usd_per_mtok: 1.0,
            ..Pricing::default()
        };
        assert_eq!(pricing.cost_usd(&Usage::default()), 0.0);
    }

    #[test]
    fn rejects_negative_and_non_finite_rates() {
        assert!(
            !Pricing {
                input_usd_per_mtok: -1.0,
                ..Pricing::default()
            }
            .validate()
        );
        assert!(
            !Pricing {
                output_usd_per_mtok: f64::NAN,
                ..Pricing::default()
            }
            .validate()
        );
        assert!(Pricing::default().validate());
    }
}
