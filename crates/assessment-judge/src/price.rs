//! Prices, for the spend limit. They are estimates for a safety cap, not
//! an invoice.

use crate::Usage;

/// Input-side cache tokens are counted at this multiple of the input price.
/// It is the cache-write rate, so the estimate is never too low.
pub const CACHE_INPUT_FACTOR: f64 = 1.25;

/// The safety margin on the estimated input tokens of a reservation.
pub const RESERVE_INPUT_FACTOR: f64 = 1.25;

/// Bytes per token assumed for a reservation. Low on purpose.
pub const RESERVE_BYTES_PER_TOKEN: f64 = 3.0;

/// The price of a model in US dollars per million tokens.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Price {
    pub input_per_mtok: f64,
    pub output_per_mtok: f64,
}

/// The price used for a model that is not in the table, such as a fallback
/// model. It is higher than any known price.
pub const UNKNOWN_PRICE: Price = Price {
    input_per_mtok: 10.0,
    output_per_mtok: 50.0,
};

const TABLE: [(&str, Price); 3] = [
    (
        "claude-opus-5-5",
        Price {
            input_per_mtok: 4.0,
            output_per_mtok: 20.0,
        },
    ),
    (
        "claude-sonnet-5-5",
        Price {
            input_per_mtok: 2.0,
            output_per_mtok: 10.0,
        },
    ),
    (
        "claude-haiku-5-5",
        Price {
            input_per_mtok: 0.10,
            output_per_mtok: 0.50,
        },
    ),
];

/// The price of `model`. A dated name such as `claude-opus-5-5-20261001`
/// has the price of its base name.
pub fn price_for(model: &str) -> Price {
    let model = model.strip_prefix("anthropic/").unwrap_or(model);
    TABLE
        .iter()
        .find(|(name, _)| {
            model
                .strip_prefix(name)
                .is_some_and(|rest| rest.is_empty() || rest.starts_with('-'))
        })
        .map_or(UNKNOWN_PRICE, |(_, price)| *price)
}

/// What a call with this usage cost, in US dollars.
pub fn cost_usd(model: &str, usage: &Usage) -> f64 {
    let price = price_for(model);
    let cache_tokens = usage
        .cache_creation_input_tokens
        .saturating_add(usage.cache_read_input_tokens);
    let input = usage.input_tokens as f64 + cache_tokens as f64 * CACHE_INPUT_FACTOR;
    (input * price.input_per_mtok + usage.output_tokens as f64 * price.output_per_mtok) / 1e6
}

/// What to set aside before a call: the request size as tokens with a
/// margin, plus a full answer.
pub fn reserve_usd(model: &str, request_bytes: usize, max_tokens: u32) -> f64 {
    let price = price_for(model);
    let input = request_bytes as f64 / RESERVE_BYTES_PER_TOKEN * RESERVE_INPUT_FACTOR;
    (input * price.input_per_mtok + f64::from(max_tokens) * price.output_per_mtok) / 1e6
}

#[cfg(test)]
mod tests {
    use super::*;

    fn close(a: f64, b: f64) -> bool {
        (a - b).abs() < 1e-12
    }

    #[test]
    fn known_models_have_their_price() {
        assert_eq!(price_for("claude-opus-5-5").input_per_mtok, 4.0);
        assert_eq!(price_for("claude-opus-5-5").output_per_mtok, 20.0);
        assert_eq!(price_for("claude-sonnet-5-5").input_per_mtok, 2.0);
        assert_eq!(price_for("claude-sonnet-5-5").output_per_mtok, 10.0);
        assert_eq!(price_for("claude-haiku-5-5").input_per_mtok, 0.10);
        assert_eq!(price_for("claude-haiku-5-5").output_per_mtok, 0.50);
        assert_eq!(
            price_for("claude-opus-5-5-20261001"),
            price_for("claude-opus-5-5")
        );
        assert_eq!(
            price_for("anthropic/claude-sonnet-5-5"),
            price_for("claude-sonnet-5-5")
        );
    }

    #[test]
    fn unknown_models_have_the_highest_price() {
        for model in ["claude-fable-5-1", "claude-opus-5-55", "claude-opus-5", ""] {
            assert_eq!(price_for(model), UNKNOWN_PRICE, "{model}");
        }
    }

    #[test]
    fn the_cost_counts_cache_tokens_at_the_higher_rate() {
        let usage = Usage {
            input_tokens: 1_000_000,
            output_tokens: 100_000,
            cache_creation_input_tokens: 400_000,
            cache_read_input_tokens: 400_000,
        };
        // 1.0 * 4 + 0.8 * 1.25 * 4 + 0.1 * 20 = 4 + 4 + 2.
        assert!(close(cost_usd("claude-opus-5-5", &usage), 10.0));
        assert!(close(cost_usd("claude-opus-5-5", &Usage::default()), 0.0));
    }

    #[test]
    fn the_reservation_covers_the_request_and_a_full_answer() {
        // 3000 bytes -> 1000 tokens * 1.25 * $4 + 4096 * $20, per million.
        let reserve = reserve_usd("claude-opus-5-5", 3000, 4096);
        assert!(close(reserve, (1250.0 * 4.0 + 4096.0 * 20.0) / 1e6));
        assert!(reserve_usd("other", 3000, 4096) > reserve);
    }
}
