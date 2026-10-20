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
    /// default to zero, i.e. cached tokens are billed at the input rate unless
    /// a cheaper rate is configured.
    ///
    /// Cached tokens are a subset of `input_tokens`, so they are billed once —
    /// at the cache-read rate — instead of being counted twice.
    pub fn cost_usd(&self, usage: &Usage) -> f64 {
        const MTOK: f64 = 1_000_000.0;
        let tokens = |count: Option<u64>| count.unwrap_or(0) as f64;
        let cached = usage.cache_read_tokens.unwrap_or(0);
        let uncached = usage.input_tokens.unwrap_or(0).saturating_sub(cached);
        let input = uncached as f64 * self.input_usd_per_mtok;
        let output = tokens(usage.output_tokens) * self.output_usd_per_mtok;
        let cache_read = cached as f64 * self.cache_read_usd_per_mtok;
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

    /// The exact counters a real DeepSeek turn reported on 2026-10-20:
    /// 2718 prompt tokens of which 2560 were cache hits. Before the fix this
    /// priced the hits twice and returned $0.00119556; the true cost of the
    /// turn was $0.00042756.
    #[test]
    fn cached_tokens_are_billed_once() {
        let pricing = Pricing {
            input_usd_per_mtok: 0.30,
            output_usd_per_mtok: 1.20,
            cache_read_usd_per_mtok: 0.006,
            cache_write_usd_per_mtok: 0.0,
        };
        let usage = Usage {
            input_tokens: Some(2_718),
            output_tokens: Some(304),
            cache_read_tokens: Some(2_560),
            cache_write_tokens: None,
        };
        let cost = pricing.cost_usd(&usage);
        assert!((cost - 0.000_427_56).abs() < 1e-12, "cost was {cost}");
    }

    #[test]
    fn cache_hits_larger_than_input_do_not_go_negative() {
        let pricing = Pricing {
            input_usd_per_mtok: 1.0,
            ..Pricing::default()
        };
        let usage = Usage {
            input_tokens: Some(10),
            cache_read_tokens: Some(1_000_000),
            ..Usage::default()
        };
        // Nothing is billed at the input rate, and the cache rate is zero.
        assert_eq!(pricing.cost_usd(&usage), 0.0);
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
