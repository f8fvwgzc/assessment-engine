//! The numeric building blocks every score is made of.
//!
//! All scores are in `[0, 1]` and read as "how likely is it that this needs
//! attention". They combine like independent pieces of evidence
//! ([`noisy_or`]), are scaled by context without leaving the unit interval
//! ([`amplify`]), and grow from unbounded raw measures with diminishing
//! returns ([`saturate`]).

/// Clamps to `[0, 1]`; NaN becomes 0.
pub fn clamp01(x: f64) -> f64 {
    // `+ 0.0` turns -0.0 into 0.0.
    if x.is_nan() {
        0.0
    } else {
        x.clamp(0.0, 1.0) + 0.0
    }
}

/// Noisy-OR: `1 - Π(1 - pᵢ)`. The probability that at least one of several
/// independent causes fires. Monotone in every argument, bounded by 1, equal
/// to the maximum when one cause dominates, and growing as small causes
/// accumulate. An empty input is 0.
pub fn noisy_or<I: IntoIterator<Item = f64>>(probabilities: I) -> f64 {
    let survive: f64 = probabilities
        .into_iter()
        .map(|p| 1.0 - clamp01(p))
        .product();
    clamp01(1.0 - survive)
}

/// Scales a score by a context factor `γ` without leaving `[0, 1]`:
/// `1 - (1 - s)^γ`. `γ > 1` amplifies, `γ < 1` dampens, `γ = 1` is the
/// identity; 0 and 1 are fixed points, and for small `s` it is ≈ `γ·s`.
/// `γ` is clamped to `[0.01, 10]`.
pub fn amplify(score: f64, gamma: f64) -> f64 {
    let s = clamp01(score);
    let g = if gamma.is_finite() {
        gamma.clamp(0.01, 10.0)
    } else {
        1.0
    };
    if s >= 1.0 {
        return 1.0;
    }
    clamp01(1.0 - (1.0 - s).powf(g))
}

/// `1 - e^{-x/scale}` for `x ≥ 0`: 0 at 0, ≈ 0.63 at `scale`, → 1.
pub fn saturate(x: f64, scale: f64) -> f64 {
    if !x.is_finite() {
        return if x > 0.0 { 1.0 } else { 0.0 };
    }
    if x <= 0.0 || scale <= 0.0 || !scale.is_finite() {
        return 0.0;
    }
    clamp01(1.0 - (-x / scale).exp())
}

/// `1 - (1 - base)·(1 - extra)^weight`: raises `base` by a share of `extra`.
/// Never below `base` (nor above 1), monotone in both inputs, equal to
/// `base` when `extra` is 0. The engine's overall score is
/// `escalate(severity, breadth, …)`: the worst single impact, raised by how
/// widely the rest of the affected effort is touched.
pub fn escalate(base: f64, extra: f64, weight: f64) -> f64 {
    let base = clamp01(base);
    let extra = clamp01(extra);
    let weight = if weight.is_finite() {
        weight.max(0.0)
    } else {
        0.0
    };
    if extra >= 1.0 && weight > 0.0 {
        return 1.0;
    }
    clamp01(1.0 - (1.0 - base) * (1.0 - extra).powf(weight)).max(base)
}

/// Weighted geometric combination of complements: `1 - Π(1 - sᵢ)^{wᵢ}` with
/// the weights normalized to sum to 1. A weighted mean of the inputs'
/// survivals: it always lies between the smallest and the largest input, so
/// it can never exceed the strongest one (use [`escalate`] for that).
pub fn soft_or_weighted(pairs: &[(f64, f64)]) -> f64 {
    let total: f64 = pairs.iter().map(|(_, w)| w.max(0.0)).sum();
    if total <= 0.0 {
        return 0.0;
    }
    let mut log_survive = 0.0;
    for &(s, w) in pairs {
        let w = w.max(0.0) / total;
        let s = clamp01(s);
        if w == 0.0 {
            continue;
        }
        if s >= 1.0 {
            return 1.0;
        }
        log_survive += w * (1.0 - s).ln();
    }
    clamp01(1.0 - log_survive.exp())
}

/// Mean and sample standard deviation (`n - 1`), or `None` with fewer than two values.
pub fn mean_std(values: &[f64]) -> Option<(f64, f64)> {
    let finite: Vec<f64> = values.iter().copied().filter(|v| v.is_finite()).collect();
    if finite.len() < 2 {
        return None;
    }
    let n = finite.len() as f64;
    let mean = finite.iter().sum::<f64>() / n;
    let var = finite.iter().map(|v| (v - mean).powi(2)).sum::<f64>() / (n - 1.0);
    Some((mean, var.sqrt()))
}

/// How many standard deviations `x` lies above the mean of `history`.
/// `None` with fewer than `min_samples` values, a (near-)zero spread, or
/// any non-finite intermediate.
pub fn z_score(x: f64, history: &[f64], min_samples: usize) -> Option<f64> {
    if history.len() < min_samples.max(2) || !x.is_finite() {
        return None;
    }
    let (mean, std) = mean_std(history)?;
    if !mean.is_finite() || !std.is_finite() || std < 1e-6 {
        return None;
    }
    Some((x - mean) / std).filter(|z| z.is_finite())
}

/// Exponentially decayed sum `Σ mᵢ · 2^{-ageᵢ / half_life}` over
/// `(age_days, magnitude)` pairs; future events (negative age) are ignored.
pub fn decayed_sum(points: &[(f64, f64)], half_life_days: f64) -> f64 {
    if half_life_days <= 0.0 || !half_life_days.is_finite() {
        return 0.0;
    }
    points
        .iter()
        .filter(|(age, m)| age.is_finite() && *age >= 0.0 && m.is_finite())
        .map(|(age, m)| m.max(0.0) * (-(age / half_life_days) * std::f64::consts::LN_2).exp())
        .sum()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn noisy_or_properties() {
        assert_eq!(noisy_or([]), 0.0);
        assert!((noisy_or([0.5]) - 0.5).abs() < 1e-12);
        assert!((noisy_or([0.5, 0.5]) - 0.75).abs() < 1e-12);
        assert_eq!(noisy_or([1.0, 0.1]), 1.0);
        assert_eq!(noisy_or([f64::NAN, 0.2]), noisy_or([0.2]));
        assert!(noisy_or([0.3, 0.2]) >= 0.3);
    }

    #[test]
    fn amplify_properties() {
        assert_eq!(amplify(0.0, 3.0), 0.0);
        assert_eq!(amplify(1.0, 0.2), 1.0);
        assert!((amplify(0.4, 1.0) - 0.4).abs() < 1e-12);
        assert!(amplify(0.4, 2.0) > 0.4);
        assert!(amplify(0.4, 0.5) < 0.4);
        assert!((amplify(0.01, 2.0) - 0.0199).abs() < 1e-3);
        assert_eq!(amplify(0.5, f64::NAN), 0.5);
    }

    #[test]
    fn saturate_properties() {
        assert_eq!(saturate(0.0, 1.0), 0.0);
        assert!((saturate(1.0, 1.0) - (1.0 - (-1.0f64).exp())).abs() < 1e-12);
        assert_eq!(saturate(f64::INFINITY, 1.0), 1.0);
        assert_eq!(saturate(-3.0, 1.0), 0.0);
    }

    #[test]
    fn soft_or_weighted_properties() {
        assert_eq!(soft_or_weighted(&[(0.0, 0.6), (0.0, 0.4)]), 0.0);
        assert_eq!(soft_or_weighted(&[(1.0, 0.6), (0.0, 0.4)]), 1.0);
        let s = soft_or_weighted(&[(0.5, 0.5), (0.5, 0.5)]);
        assert!((s - 0.5).abs() < 1e-12);
        assert_eq!(soft_or_weighted(&[]), 0.0);
        // A mean, never above its strongest input.
        assert!(soft_or_weighted(&[(0.6, 0.6), (0.2, 0.4)]) < 0.6);
    }

    #[test]
    fn escalate_never_falls_below_its_base() {
        assert_eq!(escalate(0.4, 0.0, 0.67), 0.4);
        assert!(escalate(0.4, 0.3, 0.67) > 0.4);
        assert!(escalate(0.4, 0.5, 0.67) > escalate(0.4, 0.3, 0.67));
        assert_eq!(escalate(0.4, 1.0, 0.67), 1.0);
        assert_eq!(escalate(0.4, 0.5, 0.0), 0.4);
        assert_eq!(escalate(f64::NAN, 0.5, f64::NAN), 0.0);
        for b in [0.0, 0.1, 0.5, 0.9, 1.0] {
            for e in [0.0, 0.2, 0.7, 1.0] {
                let s = escalate(b, e, 0.67);
                assert!((0.0..=1.0).contains(&s) && s >= b);
            }
        }
    }

    #[test]
    fn z_and_decay() {
        let hist = [0.1, 0.12, 0.09, 0.11, 0.1];
        let z = z_score(0.5, &hist, 5).expect("enough samples");
        assert!(z > 10.0);
        assert!(z_score(0.5, &hist[..3], 5).is_none());
        assert!(z_score(0.5, &[0.1; 6], 5).is_none());
        // Huge magnitudes overflow the variance: no NaN comes out.
        assert!(z_score(0.5, &[1e308, -1e308, 1e308, -1e308, 1e308], 5).is_none());
        let d = decayed_sum(&[(0.0, 1.0), (7.0, 1.0), (-1.0, 5.0)], 7.0);
        assert!((d - 1.5).abs() < 1e-12);
    }
}
