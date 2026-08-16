//! Belief resolution: the total order that decides which claim is believed
//! per subject (spec §5.3, tests U-BELIEF-1/2).
//!
//! Pure functions, no I/O. Consumed by the ranking engine when it
//! materializes the `believed` flag on rows for a subject group.

use crate::types::{Claim, Credibility};

/// One member of a subject group being resolved for belief: the claim plus
/// when clog recorded it (used as a tiebreaker in the total order).
pub(crate) struct BeliefInput<'a> {
    /// The candidate claim.
    pub claim: &'a Claim,
    /// When clog recorded this claim, in epoch millis.
    pub recorded_at: u64,
}

/// The §5.3 total order as a max-key: later `occurred_at` wins; tie goes to
/// better reliability, then better credibility, then later `recorded_at`,
/// then the lexicographically larger `claim_key`.
pub(crate) fn belief_key(
    c: &BeliefInput,
) -> (
    u64,
    std::cmp::Reverse<u8>,
    std::cmp::Reverse<u8>,
    u64,
    String,
) {
    (
        c.claim.occurred_at,
        std::cmp::Reverse(c.claim.reliability.rank()),
        std::cmp::Reverse(c.claim.credibility.rank()),
        c.recorded_at,
        c.claim.claim_key.clone(),
    )
}

/// Resolves which claim, if any, is believed among a subject group.
///
/// Only members with `credibility.rank() <= floor.rank()` are eligible to
/// win. If no member is eligible and the group has exactly one member (the
/// only-claim exception), that member wins anyway. If no member is eligible
/// and the group has two or more members, nobody is believed and `None` is
/// returned. Otherwise the winner is the max by [`belief_key`] among the
/// eligible members.
pub(crate) fn resolve<'a>(group: &[BeliefInput<'a>], floor: Credibility) -> Option<&'a Claim> {
    let eligible: Vec<&BeliefInput<'a>> = group
        .iter()
        .filter(|c| c.claim.credibility.rank() <= floor.rank())
        .collect();

    if eligible.is_empty() {
        return if group.len() == 1 {
            group.first().map(|c| c.claim)
        } else {
            None
        };
    }

    eligible
        .into_iter()
        .max_by_key(|c| belief_key(c))
        .map(|c| c.claim)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::*;
    use crate::validate::tests_base_claim;

    fn claim(key: &str, occ: u64, r: Reliability, c: Credibility) -> Claim {
        let mut cl = tests_base_claim();
        cl.claim_key = key.into();
        cl.subject_key = Some("s".into());
        cl.occurred_at = occ;
        cl.reliability = r;
        cl.credibility = c;
        cl
    }

    #[test]
    fn u_belief_1_total_order() {
        use Credibility::*;
        use Reliability::*;
        // each later claim beats all before it, per one tier of the order
        let a = claim("a", 100, F, Six); // baseline
        let b = claim("b", 100, F, Five); // better credibility
        let c = claim("c", 100, E, Six); // better reliability beats credibility tier
        let d = claim("d", 200, F, Six); // later occurred_at beats everything
        let claims = [&a, &b, &c, &d];
        // recorded_at all equal; exhaustive permutations of arrival order
        for perm in permutations(&claims) {
            let group: Vec<BeliefInput> = perm
                .iter()
                .map(|c| BeliefInput {
                    claim: c,
                    recorded_at: 1,
                })
                .collect();
            assert_eq!(resolve(&group, Credibility::Six).unwrap().claim_key, "d");
        }
        // tie on everything but recorded_at
        let g = [
            BeliefInput {
                claim: &a,
                recorded_at: 5,
            },
            BeliefInput {
                claim: &b0(&a, "a2"),
                recorded_at: 9,
            },
        ];
        assert_eq!(resolve(&g, Credibility::Six).unwrap().claim_key, "a2");
        // full tie -> lexicographically larger claim_key
        let g = [
            BeliefInput {
                claim: &a,
                recorded_at: 5,
            },
            BeliefInput {
                claim: &b0(&a, "z"),
                recorded_at: 5,
            },
        ];
        assert_eq!(resolve(&g, Credibility::Six).unwrap().claim_key, "z");
    }

    /// Tier 2 in isolation: everything else tied, only reliability differs.
    ///
    /// The better claim is given the *lexicographically smaller* key and both
    /// share a `recorded_at`, so the two tiers below reliability both favour
    /// the worse claim. Only a correctly-oriented reliability comparison can
    /// produce this answer: drop the tier and the key tiebreak picks
    /// `"z-worse"`; invert it and reliability itself picks `"z-worse"`.
    #[test]
    fn u_belief_1_tier_2_reliability_alone() {
        use Credibility::*;
        use Reliability::*;
        let better = claim("a-better", 100, B, Three);
        let worse = claim("z-worse", 100, D, Three);
        let g = [
            BeliefInput {
                claim: &better,
                recorded_at: 7,
            },
            BeliefInput {
                claim: &worse,
                recorded_at: 7,
            },
        ];
        assert_eq!(resolve(&g, Six).unwrap().claim_key, "a-better");
    }

    /// Tier 3 in isolation: `occurred_at` *and* reliability tied, only
    /// credibility differs. Same trap as tier 2 — the better claim loses
    /// every lower tiebreak.
    #[test]
    fn u_belief_1_tier_3_credibility_alone() {
        use Credibility::*;
        use Reliability::*;
        let better = claim("a-better", 100, C, Two);
        let worse = claim("z-worse", 100, C, Five);
        let g = [
            BeliefInput {
                claim: &better,
                recorded_at: 7,
            },
            BeliefInput {
                claim: &worse,
                recorded_at: 7,
            },
        ];
        assert_eq!(resolve(&g, Six).unwrap().claim_key, "a-better");
    }

    fn b0(base: &Claim, key: &str) -> Claim {
        let mut c = base.clone();
        c.claim_key = key.into();
        c
    }

    fn permutations<'a>(xs: &[&'a Claim]) -> Vec<Vec<&'a Claim>> {
        if xs.len() <= 1 {
            return vec![xs.to_vec()];
        }
        let mut out = vec![];
        for i in 0..xs.len() {
            let mut rest = xs.to_vec();
            let x = rest.remove(i);
            for mut p in permutations(&rest) {
                p.insert(0, x);
                out.push(p);
            }
        }
        out
    }

    #[test]
    fn u_belief_2_credibility_floor() {
        use Credibility::*;
        use Reliability::*;
        let good = claim("good", 100, A, Two);
        let bad = claim("bad", 200, A, Five); // newer but below floor Three
        let g = [
            BeliefInput {
                claim: &good,
                recorded_at: 1,
            },
            BeliefInput {
                claim: &bad,
                recorded_at: 2,
            },
        ];
        assert_eq!(resolve(&g, Three).unwrap().claim_key, "good");
        // only-claim exception
        let g = [BeliefInput {
            claim: &bad,
            recorded_at: 2,
        }];
        assert_eq!(resolve(&g, Three).unwrap().claim_key, "bad");
        // all floored, >= 2 members -> nobody believed
        let bad2 = claim("bad2", 300, A, Six);
        let g = [
            BeliefInput {
                claim: &bad,
                recorded_at: 2,
            },
            BeliefInput {
                claim: &bad2,
                recorded_at: 3,
            },
        ];
        assert!(resolve(&g, Three).is_none());
    }
}
