//! Signals in activity text: a comment saying "blocked on the auth API" or
//! "the client changed the requirements" matters even though no field moved.
//!
//! Lexicon matching over stemmed tokens, sentence by sentence, skipping
//! matches negated within the three preceding tokens of the same sentence
//! ("not blocked anymore", but "Not great. Blocked on the API." is a
//! blocker). Every category counts once per change set however many
//! comments repeat it. The lexicon lives in the policy, so teams can add
//! their own terms and languages.

use std::collections::BTreeMap;

use crate::assessment::{Signal, SignalCategory};
use crate::policy::{SignalRule, TextPolicy};
use crate::stats::{clamp01, noisy_or};
use crate::text::stem;

fn tokens(text: &str) -> Vec<String> {
    let lower = text
        .to_lowercase()
        .replace(['\u{2019}', '\u{2018}'], "'")
        .replace("can't", "can not")
        .replace("won't", "will not")
        .replace("n't", " not");
    lower
        .split(|c: char| !(c.is_alphanumeric() || c == '_'))
        .filter(|t| !t.is_empty())
        .map(stem)
        .collect()
}

/// The text's sentences (split at `.`, `!`, `?`, `;` and line breaks), each as tokens.
fn sentences(text: &str) -> Vec<Vec<String>> {
    text.split(['.', '!', '?', ';', '\n', '\r'])
        .map(tokens)
        .filter(|t| !t.is_empty())
        .collect()
}

fn excerpt(text: &str) -> String {
    let flat = text.split_whitespace().collect::<Vec<_>>().join(" ");
    if flat.chars().count() > 160 {
        let cut: String = flat.chars().take(157).collect();
        format!("{cut}…")
    } else {
        flat
    }
}

/// One category's evidence across a change set's texts.
struct Found {
    weight: f64,
    terms: Vec<String>,
    excerpt: String,
}

/// Finds the signals in one text.
#[cfg(test)]
pub(crate) fn detect(text: &str, rules: &[SignalRule], text_policy: &TextPolicy) -> Vec<Signal> {
    detect_all([text], rules, text_policy)
}

/// Finds the signals in all of a change set's texts (its comments) together:
/// each category counts once, with the distinct terms of every text, so
/// splitting the same words over several comments does not make them
/// stronger. A `?` anywhere is one uncertainty signal (the weight of the
/// policy's uncertainty terms).
pub(crate) fn detect_all<'a>(
    texts: impl IntoIterator<Item = &'a str>,
    rules: &[SignalRule],
    text_policy: &TextPolicy,
) -> Vec<Signal> {
    let negations: Vec<String> = text_policy.negation_terms.iter().map(|t| stem(t)).collect();
    let phrases: Vec<Vec<Vec<String>>> = rules
        .iter()
        .map(|r| r.terms.iter().map(|t| tokens(t)).collect())
        .collect();
    let question_weight = rules
        .iter()
        .filter(|r| r.category == SignalCategory::Uncertainty)
        .map(|r| r.weight)
        .fold(None, |acc: Option<f64>, w| {
            Some(acc.map_or(w, |a| a.max(w)))
        })
        .unwrap_or(0.3);
    let mut by_category: BTreeMap<SignalCategory, Found> = BTreeMap::new();
    let mut add = |category: SignalCategory, weight: f64, term: &str, text: &str| {
        let entry = by_category.entry(category).or_insert_with(|| Found {
            weight: 0.0,
            terms: Vec::new(),
            excerpt: excerpt(text),
        });
        entry.weight = entry.weight.max(weight);
        if !entry.terms.iter().any(|t| t == term) {
            entry.terms.push(term.to_owned());
        }
    };
    for text in texts {
        let sentences = sentences(text);
        for (rule, rule_phrases) in rules.iter().zip(&phrases) {
            for (term, phrase) in rule.terms.iter().zip(rule_phrases) {
                if phrase.is_empty() {
                    continue;
                }
                let hit = sentences.iter().any(|toks| {
                    toks.len() >= phrase.len()
                        && (0..=toks.len() - phrase.len()).any(|i| {
                            toks[i..i + phrase.len()] == phrase[..]
                                && !toks[i.saturating_sub(3)..i]
                                    .iter()
                                    .any(|t| negations.contains(t))
                        })
                });
                if hit {
                    add(rule.category, rule.weight, term, text);
                }
            }
        }
        if text.contains('?') {
            add(SignalCategory::Uncertainty, question_weight, "?", text);
        }
    }
    by_category
        .into_iter()
        .map(|(category, found)| {
            // A few distinct terms of one category are a little stronger than one.
            let boost = 1.0 + 0.15 * (found.terms.len().saturating_sub(1).min(2) as f64);
            Signal {
                category,
                weight: clamp01((found.weight * boost).min(found.weight * 1.3)),
                terms: found.terms,
                excerpt: found.excerpt,
            }
        })
        .collect()
}

/// The activity magnitude: noisy-OR of the risk signals, damped by resolution signals.
pub(crate) fn magnitude(signals: &[Signal]) -> f64 {
    let risk = noisy_or(
        signals
            .iter()
            .filter(|s| s.category != SignalCategory::Resolution)
            .map(|s| s.weight),
    );
    let resolution = signals
        .iter()
        .filter(|s| s.category == SignalCategory::Resolution)
        .map(|s| s.weight)
        .fold(0.0, f64::max);
    clamp01(risk * (1.0 - 0.5 * resolution))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::policy::Policy;

    fn detect_default(text: &str) -> Vec<Signal> {
        let p = Policy::default();
        detect(text, &p.signals, &p.text)
    }

    fn detect_many(texts: &[&str]) -> Vec<Signal> {
        let p = Policy::default();
        detect_all(texts.iter().copied(), &p.signals, &p.text)
    }

    fn categories(signals: &[Signal]) -> Vec<SignalCategory> {
        signals.iter().map(|s| s.category).collect()
    }

    #[test]
    fn finds_blockers_and_scope() {
        let s = detect_default(
            "We're blocked waiting on the auth team. Also the client changed scope again.",
        );
        let cats = categories(&s);
        assert!(cats.contains(&SignalCategory::Blocker));
        assert!(cats.contains(&SignalCategory::Scope));
        assert!(magnitude(&s) > 0.7);
    }

    #[test]
    fn negated_matches_are_skipped() {
        let s = detect_default("Good news, this is not blocked anymore");
        assert!(!categories(&s).contains(&SignalCategory::Blocker));
    }

    #[test]
    fn stems_match_inflections() {
        let s = detect_default("The rollout keeps failing on staging");
        assert!(categories(&s).contains(&SignalCategory::Risk));
    }

    #[test]
    fn resolution_damps() {
        let worried = detect_default("There is a regression in the export");
        let resolved = detect_default("There was a regression in the export, fixed and merged");
        assert!(magnitude(&resolved) < magnitude(&worried));
    }

    #[test]
    fn questions_are_uncertainty() {
        let s = detect_default("Should this cover the admin panel too?");
        assert!(categories(&s).contains(&SignalCategory::Uncertainty));
    }

    #[test]
    fn neutral_comment_is_quiet() {
        let s = detect_default("Pushed the first draft of the migration");
        assert!(magnitude(&s) < 0.01, "{s:?}");
    }

    #[test]
    fn widgets_and_test_rewrites_are_not_scope_changes() {
        for text in [
            "Implemented the card brand drop-down and the expiry picker.",
            "I will rewrite the unit test names to match the new convention.",
            "Added the dependency to Cargo.toml.",
        ] {
            let s = detect_default(text);
            assert!(
                !categories(&s).contains(&SignalCategory::Scope)
                    && !categories(&s).contains(&SignalCategory::Blocker),
                "{text}: {s:?}"
            );
        }
        let s = detect_default("Client says PayPal is no longer needed, please drop it");
        assert!(categories(&s).contains(&SignalCategory::Scope));
    }

    #[test]
    fn repeated_comments_count_once() {
        let one = detect_many(&["Blocked on the vendor API. Still blocked. Blocked again today."]);
        let three = detect_many(&[
            "Blocked on the vendor API. Still blocked. Blocked again today.",
            "Blocked on the vendor API. Still blocked. Blocked again today.",
            "Blocked on the vendor API. Still blocked. Blocked again today.",
        ]);
        assert_eq!(magnitude(&one), magnitude(&three));
        let chatty = detect_many(&[
            "Can you take a look?",
            "Is staging up?",
            "Lunch at noon?",
            "Pairing tomorrow?",
            "Want coffee?",
        ]);
        assert_eq!(chatty.len(), 1);
        assert!((magnitude(&chatty) - 0.3).abs() < 1e-12, "{chatty:?}");
    }

    #[test]
    fn negation_does_not_cross_sentences() {
        let s = detect_default("Not great. Blocked on the vendor API.");
        assert!(categories(&s).contains(&SignalCategory::Blocker), "{s:?}");
        let s = detect_default("This is not blocked anymore.");
        assert!(!categories(&s).contains(&SignalCategory::Blocker));
    }

    #[test]
    fn mongolian_terms() {
        let s = detect_default("Энэ ажил гацсан байна");
        assert!(categories(&s).contains(&SignalCategory::Blocker));
    }
}
