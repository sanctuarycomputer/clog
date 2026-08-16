//! Property tests P1-P4, P6, P7 (Task 16).
//!
//! Everything here goes through the public surface only, with a Manual
//! clock (INV-10), mirroring `tests/api.rs`'s patterns.

use clog::*;
use proptest::prelude::*;

// -- generators ---------------------------------------------------------

fn arb_claim() -> impl Strategy<Value = Claim> {
    (
        0..8u8,
        prop::option::of(0..3u8),
        0..2u8,
        0..6u8,
        0..6u8,
        1..6u64,
        prop::collection::vec(0..6u8, 1..4),
        prop::bool::ANY,
    )
        .prop_map(|(k, s, o, rel, cred, occ, words, with_ent)| {
            let vocab = ["invoice", "overdue", "kickoff", "moved", "question", "paid"];
            Claim {
                claim_key: format!("k{k}"),
                subject_key: s.map(|s| format!("s{s}")),
                source_ref: "prop:1".into(),
                observer: ObserverId::from(if o == 0 { "o0" } else { "o1" }),
                schema_v: 1,
                occurred_at: occ,
                observed_at: occ,
                reliability: [
                    Reliability::A,
                    Reliability::B,
                    Reliability::C,
                    Reliability::D,
                    Reliability::E,
                    Reliability::F,
                ][rel as usize],
                credibility: [
                    Credibility::One,
                    Credibility::Two,
                    Credibility::Three,
                    Credibility::Four,
                    Credibility::Five,
                    Credibility::Six,
                ][cred as usize],
                entities: if with_ent {
                    vec![EntityRef { etype: "p".into(), id: "a".into(), name: None }]
                } else {
                    vec![]
                },
                body: words.iter().map(|w| vocab[*w as usize]).collect::<Vec<_>>().join(" "),
            }
        })
}

fn open_manual(dir: &std::path::Path) -> Clog {
    let mut c = Config::default_for(dir);
    c.tick = TickConfig { mode: ClockMode::Manual, interval_ms: 60_000 };
    let h = Clog::open(c).unwrap();
    h.advance(1_000_000).unwrap();
    h
}

/// Normalizes a situation's text for equality comparisons that are about
/// *world state*, not about counters, clocks or the render-diff log.
///
/// CONTROLLER RULING (amends the task-16 brief's rev-only strip, matching
/// `tests/api.rs`'s `norm`): strips the entire header line (it carries the
/// rev and `as_of`, both bookkeeping) and the whole `## changes since last
/// brief` section (it legitimately echoes membership deltas per §5.7, and
/// INV-3 equality is about world content, not that echo).
fn norm(s: &Situation) -> String {
    let body = s.text.split_once('\n').map_or("", |(_, rest)| rest);
    match body.find("\n## changes since last brief\n") {
        Some(i) => body[..i].to_string(),
        None => body.to_string(),
    }
}

fn live_keys(c: &Clog) -> Vec<String> {
    c.select(View::Live, Filter::default()).unwrap().into_iter().map(|r| r.claim.claim_key).collect()
}

proptest! {
    #![proptest_config(ProptestConfig { cases: 64, ..ProptestConfig::default() })]

    // P1 (INV-3): observe everything, retract all -> empty-world text
    #[test]
    fn p1_retract_all_heals(claims in prop::collection::vec(arb_claim(), 1..12)) {
        let d = tempfile::tempdir().unwrap();
        let c = open_manual(d.path());
        let empty = norm(&c.situation(None, None).unwrap());
        for cl in &claims { let _ = c.observe(vec![cl.clone()], ObserveOpts::default()); }
        for k in live_keys(&c) { c.retract(&k).unwrap(); }
        prop_assert_eq!(norm(&c.situation(None, None).unwrap()), empty);
    }

    // P2 (INV-4): only the last version per key matters
    #[test]
    fn p2_last_writer_wins(claims in prop::collection::vec(arb_claim(), 1..12)) {
        let d1 = tempfile::tempdir().unwrap();
        let full = open_manual(d1.path());
        for cl in &claims { let _ = full.observe(vec![cl.clone()], ObserveOpts::default()); }

        let mut last: std::collections::BTreeMap<String, Claim> = Default::default();
        for cl in &claims { last.insert(cl.claim_key.clone(), cl.clone()); }
        let d2 = tempfile::tempdir().unwrap();
        let compact = open_manual(d2.path());
        for cl in last.values() { let _ = compact.observe(vec![cl.clone()], ObserveOpts::default()); }

        prop_assert_eq!(norm(&full.situation(None, None).unwrap()), norm(&compact.situation(None, None).unwrap()));
        prop_assert_eq!(live_keys(&full), live_keys(&compact));
    }

    // P3 (INV-5): duplicating a prefix changes nothing, incl. scope revs
    #[test]
    fn p3_duplicates_invisible(claims in prop::collection::vec(arb_claim(), 1..8), cut in 0..8usize) {
        let cut = cut.min(claims.len());
        let d1 = tempfile::tempdir().unwrap();
        let a = open_manual(d1.path());
        for cl in &claims { let _ = a.observe(vec![cl.clone()], ObserveOpts::default()); }
        let s_a = a.situation(None, None).unwrap();

        let d2 = tempfile::tempdir().unwrap();
        let b = open_manual(d2.path());
        for cl in &claims { let _ = b.observe(vec![cl.clone()], ObserveOpts::default()); }
        for cl in claims.iter().take(cut) {
            // replay a prefix of stale versions: only claims still live in identical
            // form are true duplicates; superseded keys will upsert — so restrict to
            // claims whose key's final version is this version
            if claims.iter().rev().find(|c2| c2.claim_key == cl.claim_key).map(|c2| c2 == cl).unwrap_or(false) {
                let _ = b.observe(vec![cl.clone()], ObserveOpts::default());
            }
        }
        let s_b = b.situation(None, None).unwrap();
        prop_assert_eq!(s_a.rev, s_b.rev, "duplicate observes must not advance situation rev");
        prop_assert_eq!(s_a.text, s_b.text);
    }

    // P4 (INV-6): revoke == retract-each
    #[test]
    fn p4_revoke_equals_retract_each(claims in prop::collection::vec(arb_claim(), 1..12)) {
        let d1 = tempfile::tempdir().unwrap();
        let a = open_manual(d1.path());
        let d2 = tempfile::tempdir().unwrap();
        let b = open_manual(d2.path());
        for cl in &claims {
            let _ = a.observe(vec![cl.clone()], ObserveOpts::default());
            let _ = b.observe(vec![cl.clone()], ObserveOpts::default());
        }
        let _ = a.revoke_observer(&ObserverId::from("o0"));
        for r in b.select(View::Live, Filter { observer: Some(ObserverId::from("o0")), ..Filter::default() }).unwrap() {
            b.retract(&r.claim.claim_key).unwrap();
        }
        prop_assert_eq!(norm(&a.situation(None, None).unwrap()), norm(&b.situation(None, None).unwrap()));
        prop_assert_eq!(live_keys(&a), live_keys(&b));
    }

    // P6: belief winner is arrival-order-insensitive
    //
    // Adaptation from the brief: every generated claim here is given a fixed
    // entity (etype "p", id "shared-ent") so believed winners are readable
    // via `View::EntityState` — that view is keyed by entity, and claims
    // with no entities never appear in it. The shuffle/winner-comparison
    // logic itself is unchanged.
    #[test]
    fn p6_belief_order_insensitive(mut claims in prop::collection::vec(arb_claim(), 2..8), seed in 0..1000u64) {
        for (i, c) in claims.iter_mut().enumerate() {
            c.subject_key = Some("shared".into());
            c.claim_key = format!("k{i}"); // distinct keys, same subject
            c.entities = vec![EntityRef { etype: "p".into(), id: "shared-ent".into(), name: None }];
        }
        let d1 = tempfile::tempdir().unwrap();
        let a = open_manual(d1.path());
        for cl in &claims { a.observe(vec![cl.clone()], ObserveOpts::default()).unwrap(); }

        // deterministic shuffle
        let mut shuffled = claims.clone();
        let mut s = seed;
        for i in (1..shuffled.len()).rev() {
            s = s.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
            shuffled.swap(i, (s as usize) % (i + 1));
        }
        let d2 = tempfile::tempdir().unwrap();
        let b = open_manual(d2.path());
        for cl in &shuffled { b.observe(vec![cl.clone()], ObserveOpts::default()).unwrap(); }

        let believed = |h: &Clog| h.select(View::EntityState, Filter::default()).unwrap()
            .into_iter().map(|r| r.claim.claim_key).collect::<Vec<_>>();
        prop_assert_eq!(believed(&a), believed(&b));
    }

    // P7: merge round-trip is invisible
    #[test]
    fn p7_merge_round_trip(claims in prop::collection::vec(arb_claim(), 1..8)) {
        let d1 = tempfile::tempdir().unwrap();
        let a = open_manual(d1.path());
        for cl in &claims { let _ = a.observe(vec![cl.clone()], ObserveOpts::default()); }
        let before = norm(&a.situation(None, None).unwrap());
        let al = EntityRef { etype: "p".into(), id: "a".into(), name: None };
        let ca = EntityRef { etype: "p".into(), id: "b".into(), name: None };
        a.merge_entities(&al, &ca).unwrap();
        a.retract("clog:merge:p:a->p:b").unwrap();
        prop_assert_eq!(norm(&a.situation(None, None).unwrap()), before);
    }
}
