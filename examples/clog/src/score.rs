//! Pure scoring functions (spec §5.4, test U-SCORE-1).
//!
//! These functions are internal: hosts never call them directly, they run
//! inside the ranking engine when it materializes the live view. Kept pure
//! (no I/O, no clock reads) so they're trivially unit-testable.

use crate::types::{Claim, Credibility, Focus, Reliability};
use crate::validate::scoring_clamp;

/// Admiralty trust table, indexed by `rank()`: `A`/`One` = 1.00 down to
/// `F`/`Six` = 0.10.
const TRUST_TABLE: [f32; 6] = [1.00, 0.90, 0.75, 0.50, 0.25, 0.10];

/// Combines a source's reliability and a claim's credibility into a single
/// trust multiplier in `(0, 1]`, per §5.4's fixed Admiralty table.
pub(crate) fn trust(r: Reliability, c: Credibility) -> f32 {
    TRUST_TABLE[r.rank() as usize] * TRUST_TABLE[c.rank() as usize]
}

/// Buckets an age in days into fixed-width midpoints, so scores are stable
/// across a bucket rather than continuously decaying.
///
/// Bucket width is `half_life_days / buckets_per_half_life`. A negative age
/// (an `occurred_at` in the future, after clamping) is treated as age zero,
/// clamping to the first bucket's midpoint.
pub(crate) fn bucket_age_days(
    age_days: f32,
    half_life_days: f32,
    buckets_per_half_life: u32,
) -> f32 {
    let w = half_life_days / buckets_per_half_life as f32;
    if age_days < 0.0 {
        return w / 2.0;
    }
    (age_days / w).floor() * w + w / 2.0
}

/// Exponential recency decay: `0.5 ^ (bucket_age / half_life_days)`.
pub(crate) fn recency(bucket_age: f32, half_life_days: f32) -> f32 {
    0.5f32.powf(bucket_age / half_life_days)
}

/// Computes a claim's ranking score per §5.4: kind weight (default 1.0 for
/// unclassified) times admiralty trust times bucketed recency decay times
/// the product of every focus boost whose entity key appears in
/// `canonical_entities` (the claim's entity keys, post-alias-resolution,
/// supplied by the caller).
pub(crate) fn score_claim(
    claim: &Claim,
    kind: Option<&str>,
    focus: &Focus,
    canonical_entities: &[(String, String)],
    now_ms: u64,
    buckets_per_half_life: u32,
) -> f32 {
    let age_days =
        (now_ms.saturating_sub(scoring_clamp(claim.occurred_at, now_ms))) as f32 / 86_400_000.0;
    let bucket_age = bucket_age_days(age_days, focus.half_life_days, buckets_per_half_life);
    let rec = recency(bucket_age, focus.half_life_days);
    let trust_val = trust(claim.reliability, claim.credibility);
    let kind_weight = kind
        .and_then(|k| focus.weights.get(k))
        .copied()
        .unwrap_or(1.0);
    let boost: f32 = focus
        .boosts
        .iter()
        .filter(|(e, _)| canonical_entities.contains(&e.key()))
        .map(|(_, f)| *f)
        .product();
    kind_weight * trust_val * rec * boost
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::*;

    const DAY_MS: u64 = 86_400_000;

    #[test]
    fn u_score_1_trust_table() {
        assert_eq!(trust(Reliability::A, Credibility::One), 1.00);
        assert_eq!(trust(Reliability::B, Credibility::One), 0.90);
        assert_eq!(trust(Reliability::F, Credibility::Six), 0.10 * 0.10);
        assert_eq!(trust(Reliability::C, Credibility::Four), 0.75 * 0.50);
    }

    #[test]
    fn u_score_1_bucket_midpoints() {
        // half-life 7d, 4 buckets/hl -> width 1.75d
        let w = 7.0 / 4.0;
        assert_eq!(bucket_age_days(0.0, 7.0, 4), w / 2.0); // first bucket midpoint
        assert_eq!(bucket_age_days(1.0, 7.0, 4), w / 2.0); // same bucket
        assert_eq!(bucket_age_days(1.75, 7.0, 4), 1.75 + w / 2.0); // boundary -> next bucket
        assert_eq!(bucket_age_days(-5.0, 7.0, 4), w / 2.0); // future occurred_at
    }

    #[test]
    fn u_score_1_full_formula_with_boost_stacking() {
        let focus = Focus::uniform()
            .weight("risk", 2.0)
            .boost(
                EntityRef {
                    etype: "project".into(),
                    id: "halcyon".into(),
                    name: None,
                },
                1.5,
            )
            .boost(
                EntityRef {
                    etype: "person".into(),
                    id: "sam".into(),
                    name: None,
                },
                2.0,
            );
        let mut claim = crate::validate::tests_base_claim(); // helper added in step 3
        claim.reliability = Reliability::B; // 0.90
        claim.credibility = Credibility::Three; // 0.75
        let now = 10 * DAY_MS;
        claim.occurred_at = now; // age 0 -> bucket midpoint 0.875d
        let ents = vec![
            ("project".to_string(), "halcyon".to_string()),
            ("person".to_string(), "sam".to_string()),
        ];
        let expected = 2.0 * (0.90 * 0.75) * 0.5f32.powf((0.875f32) / 7.0) * 1.5 * 2.0; // both boosts stack multiplicatively
        let got = score_claim(&claim, Some("risk"), &focus, &ents, now, 4);
        assert!((got - expected).abs() < 1e-6, "{got} vs {expected}");
        // unclassified -> weight 1.0
        let got_u = score_claim(&claim, None, &focus, &ents, now, 4);
        assert!((got_u - expected / 2.0).abs() < 1e-6);
        // no matching boost entities -> boost 1.0
        let got_n = score_claim(&claim, Some("risk"), &focus, &[], now, 4);
        assert!((got_n - expected / 3.0).abs() < 1e-6);
    }
}
