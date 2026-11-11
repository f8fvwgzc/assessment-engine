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
/// model. No known price is higher.
pub const UNKNOWN_PRICE: Price = Price {
    input_per_mtok: 10.0,
    output_per_mtok: 50.0,
};

/// The price of a call that is not billed per token.
pub const FREE: Price = Price {
    input_per_mtok: 0.0,
    output_per_mtok: 0.0,
};

/// The id prefixes of the providers that run an agent command line on a
/// subscription login. Their calls are not billed per token, so the cost
/// of one is reported as 0 and the spend limit does not bind.
pub const SUBSCRIPTION_PREFIXES: [&str; 2] = ["claude-cli/", "codex-cli/"];

/// Whether `id` names a provider that is not billed per token.
pub fn is_subscription(id: &str) -> bool {
    SUBSCRIPTION_PREFIXES
        .iter()
        .any(|prefix| id.starts_with(prefix))
}

const fn price(input_per_mtok: f64, output_per_mtok: f64) -> Price {
    Price {
        input_per_mtok,
        output_per_mtok,
    }
}

const TABLE: [(&str, Price); 15] = [
    ("claude-opus-5-5", price(4.0, 20.0)),
    ("claude-sonnet-5-5", price(2.0, 10.0)),
    ("claude-haiku-5-5", price(0.1, 0.5)),
    ("gpt-6-astra", price(10.0, 50.0)),
    ("gpt-6.1-sol", price(2.0, 10.0)),
    ("gpt-6.1-terra", price(1.0, 5.0)),
    ("gpt-6-luna", price(0.1, 0.5)),
    ("gpt-6-sol", price(2.0, 10.0)),
    ("gpt-5.6-sol", price(4.0, 20.0)),
    ("gpt-5.6-terra", price(2.0, 12.0)),
    ("gpt-5.6-luna", price(0.2, 1.2)),
    ("gpt-5.5", price(5.0, 30.0)),
    ("gpt-5.4", price(2.5, 15.0)),
    ("gpt-5.4-mini", price(0.75, 4.5)),
    ("gpt-5.4-nano", price(0.2, 1.25)),
];

/// The id prefixes of the providers that are billed per token.
const API_PREFIXES: [&str; 2] = ["anthropic/", "openai/"];

/// Whether `rest` is the date at the end of a dated model name: a `-` and
/// then eight digits, which may be grouped with `-`, as in `-20261001` or
/// `-2026-09-01`.
fn is_date_suffix(rest: &str) -> bool {
    let Some(date) = rest.strip_prefix('-') else {
        return false;
    };
    let digits = date.bytes().filter(u8::is_ascii_digit).count();
    digits == 8
        && date
            .split('-')
            .all(|part| !part.is_empty() && part.bytes().all(|b| b.is_ascii_digit()))
}

/// The price of `model`. A dated name such as `claude-opus-5-5-20261001`
/// or `gpt-6.1-sol-2026-09-01` has the price of its base name. Any other
/// ending makes another model, so `gpt-5.5-pro` is not priced as `gpt-5.5`
/// but as an unknown model, at [`UNKNOWN_PRICE`]. When two names of the
/// table fit, the longer one counts, so `gpt-5.4-mini-2026-03-05` is not
/// priced as `gpt-5.4`. A judge id of a command line provider, such as
/// `codex-cli/default`, is [`FREE`].
pub fn price_for(model: &str) -> Price {
    if is_subscription(model) {
        return FREE;
    }
    let model = API_PREFIXES
        .iter()
        .find_map(|prefix| model.strip_prefix(prefix))
        .unwrap_or(model);
    TABLE
        .iter()
        .filter(|(name, _)| {
            model
                .strip_prefix(name)
                .is_some_and(|rest| rest.is_empty() || is_date_suffix(rest))
        })
        .max_by_key(|(name, _)| name.len())
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

/// The usage to assume for a call whose response reports none: the
/// request size as tokens with the margin of a reservation, plus a full
/// answer. It is an upper estimate, so the spend limit still binds.
pub fn assumed_usage(request_bytes: usize, max_tokens: u32) -> Usage {
    let input = request_bytes as f64 / RESERVE_BYTES_PER_TOKEN * RESERVE_INPUT_FACTOR;
    Usage {
        input_tokens: input.ceil() as u64,
        output_tokens: u64::from(max_tokens),
        ..Usage::default()
    }
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
    fn openai_models_have_their_price() {
        let table = [
            ("gpt-6-astra", 10.0, 50.0),
            ("gpt-6.1-sol", 2.0, 10.0),
            ("gpt-6.1-terra", 1.0, 5.0),
            ("gpt-6-luna", 0.10, 0.50),
            ("gpt-6-sol", 2.0, 10.0),
            ("gpt-5.6-sol", 4.0, 20.0),
            ("gpt-5.6-terra", 2.0, 12.0),
            ("gpt-5.6-luna", 0.20, 1.20),
            ("gpt-5.5", 5.0, 30.0),
            ("gpt-5.4", 2.50, 15.0),
            ("gpt-5.4-mini", 0.75, 4.50),
            ("gpt-5.4-nano", 0.20, 1.25),
        ];
        for (model, input, output) in table {
            let expected = price(input, output);
            assert_eq!(price_for(model), expected, "{model}");
            assert_eq!(price_for(&format!("openai/{model}")), expected, "{model}");
            // A dated name has the price of its base name.
            let dated = format!("openai/{model}-2026-09-01");
            assert_eq!(price_for(&dated), expected, "{dated}");
        }
        assert_eq!(
            price_for(crate::DEFAULT_OPENAI_MODEL),
            price(2.0, 10.0),
            "the default model"
        );
    }

    #[test]
    fn a_longer_name_is_never_priced_as_a_shorter_one() {
        // The mini and nano models are not the plain one.
        assert_ne!(price_for("gpt-5.4-mini"), price_for("gpt-5.4"));
        assert_ne!(price_for("gpt-5.4-nano"), price_for("gpt-5.4"));
        assert_eq!(price_for("gpt-5.4-mini-2026-03-05"), price(0.75, 4.50));
        assert_eq!(price_for("gpt-5.4-2026-03-05"), price(2.50, 15.0));
        // A name is matched whole, and only a date may follow it.
        for model in [
            "gpt-5.5-pro",
            "openai/gpt-6-sol-pro",
            "gpt-6-luna-ultra-max",
            "gpt-5.4-nano-pro",
            "gpt-6.1-terra-",
            "gpt-6.1-terra--",
            "gpt-6.1-sol-latest",
            "gpt-6.1-sol-2",
            "gpt-6.1-sol-2026",
            "gpt-6.1-sol-2026-09-01-pro",
            "gpt-6.1-sol-2026--09-01",
            "gpt-6.1-sol-202609011",
            "gpt-6.1-sol20260901",
            "claude-opus-5-5-1",
            "claude-opus-5-5-fast",
            "gpt-6",
            "gpt-6.1",
            "gpt-6.1-so",
            "gpt-6.1-solar",
            "gpt-6.2-sol",
            "gpt-6-terra",
            "gpt-5",
            "gpt-5.44",
            "gpt-5.5.1",
            "gpt-4o",
            "openai/",
            "openai/unknown",
            "openai/anthropic/gpt-6-sol",
            "o3",
        ] {
            assert_eq!(price_for(model), UNKNOWN_PRICE, "{model}");
        }
    }

    #[test]
    fn no_price_is_above_the_unknown_one() {
        for (name, price) in TABLE {
            assert!(
                price.input_per_mtok <= UNKNOWN_PRICE.input_per_mtok,
                "{name}"
            );
            assert!(
                price.output_per_mtok <= UNKNOWN_PRICE.output_per_mtok,
                "{name}"
            );
            assert!(price.input_per_mtok > 0.0 && price.output_per_mtok > 0.0);
            assert!(!is_subscription(name));
        }
        assert!(!is_subscription("openai/gpt-6.1-sol"));
    }

    #[test]
    fn command_line_providers_are_not_billed() {
        let usage = Usage {
            input_tokens: 1_000_000,
            output_tokens: 1_000_000,
            ..Usage::default()
        };
        for id in [
            "claude-cli/claude-opus-5-5",
            "claude-cli/default",
            "codex-cli/default",
            "codex-cli/some-model",
        ] {
            assert!(is_subscription(id), "{id}");
            assert_eq!(price_for(id), FREE, "{id}");
            assert!(close(cost_usd(id, &usage), 0.0), "{id}");
            assert!(close(reserve_usd(id, 100_000, 4096), 0.0), "{id}");
        }
        // Only the whole prefix counts.
        for id in ["claude-cli", "codex-cli-x/default", "anthropic/codex-cli/x"] {
            assert!(!is_subscription(id), "{id}");
            assert_eq!(price_for(id), UNKNOWN_PRICE, "{id}");
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
    fn a_date_suffix_is_eight_digits() {
        for rest in ["-20261001", "-2026-09-01", "-2026-0901"] {
            assert!(is_date_suffix(rest), "{rest}");
        }
        for rest in [
            "",
            "-",
            "20261001",
            "-2026100",
            "-202610011",
            "-2026-09-01-",
            "--20261001",
            "-2026.09.01",
            "-pro",
            "-2026100a",
        ] {
            assert!(!is_date_suffix(rest), "{rest}");
        }
    }

    #[test]
    fn the_assumed_usage_is_never_below_the_reservation() {
        for (bytes, max_tokens) in [(0, 1), (1, 16), (3000, 4096), (8191, 4096), (100_000, 64)] {
            let usage = assumed_usage(bytes, max_tokens);
            assert_eq!(usage.output_tokens, u64::from(max_tokens));
            assert_eq!(usage.cache_read_input_tokens, 0);
            for model in ["gpt-6.1-sol", "claude-opus-5-5", "unknown"] {
                let assumed = cost_usd(model, &usage);
                let reserve = reserve_usd(model, bytes, max_tokens);
                assert!(assumed >= reserve - 1e-12, "{model} {bytes}");
                assert!(assumed > 0.0);
            }
        }
        assert_eq!(assumed_usage(3000, 4096).input_tokens, 1250);
    }

    #[test]
    fn the_reservation_covers_the_request_and_a_full_answer() {
        // 3000 bytes -> 1000 tokens * 1.25 * $4 + 4096 * $20, per million.
        let reserve = reserve_usd("claude-opus-5-5", 3000, 4096);
        assert!(close(reserve, (1250.0 * 4.0 + 4096.0 * 20.0) / 1e6));
        assert!(reserve_usd("other", 3000, 4096) > reserve);
    }
}
