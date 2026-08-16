//! G1 golden simulation (Task 17): the "client-services studio" fixture
//! (spec §11.4). This freezes the §5.8 rendering contract via insta
//! snapshots — **changing a snapshot under `tests/snapshots/` after this
//! task requires a spec edit**.
//!
//! Ten claims across three clients (Halcyon, Meridian, Vega), two scopes
//! with different weights/boosts, and four checkpoints (A-D) that exercise:
//! belief resolution's "freshness beats trust" limitation (§5.3) and its
//! healing on retraction, the entities slot dropping subject-less entities,
//! and `merge_entities` re-keying entity grouping.
//!
//! Everything here goes through the public API only (no crate internals),
//! with a `Manual` clock (INV-10) so every rendered timestamp is
//! deterministic. See `task-17-report.md` for the hand-computed score
//! tables and line-by-line snapshot verification this fixture was checked
//! against.

use clog::*;

const DAY_MS: u64 = 86_400_000;

fn day(n: u64) -> u64 {
    n * DAY_MS
}

/// Two scopes, five classification rules (the brief's four, plus `fyi` —
/// see the comment at its declaration), Manual clock.
fn fixture_config(dir: &std::path::Path) -> Config {
    let mut c = Config::default_for(dir);
    c.tick = TickConfig {
        mode: ClockMode::Manual,
        interval_ms: 60_000,
    };

    c.scopes.insert(
        "delivery-health".into(),
        Focus::uniform()
            .weight("risk", 2.5)
            .weight("question", 1.5)
            .weight("commitment", 1.5)
            .boost(
                EntityRef {
                    etype: "project".into(),
                    id: "halcyon".into(),
                    name: None,
                },
                1.5,
            ),
    );
    c.scopes.insert(
        "cash-and-collections".into(),
        Focus::uniform()
            .weight("risk", 2.0)
            .weight("fyi", 0.5)
            .weight("opportunity", 1.5),
    );

    for kd in &mut c.kinds.kinds {
        match kd.name.as_str() {
            "risk" => kd.rules.push(Rule {
                any_of: vec![
                    Matcher::BodyContains("overdue".into()),
                    Matcher::BodyContains("slipping".into()),
                ],
            }),
            "question" => kd.rules.push(Rule {
                any_of: vec![Matcher::BodyRegex(r"\?$".into())],
            }),
            "fact" => kd.rules.push(Rule {
                any_of: vec![Matcher::ObserverIs("bank-feed".into())],
            }),
            "opportunity" => kd.rules.push(Rule {
                any_of: vec![Matcher::BodyContains("inbound".into())],
            }),
            // Beyond the brief's four required rules: without this, claims
            // 5/6/11 (a kickoff move, a PTO note, a moved 1:1) all land in
            // `Unclassified`, and cash-and-collections' `fyi = 0.5` damping
            // — part of this fixture's required story — would never be
            // exercised by any live claim. Documented in task-17-report.md.
            "fyi" => kd.rules.push(Rule {
                any_of: vec![
                    Matcher::BodyContains("moved".into()),
                    Matcher::BodyContains("PTO".into()),
                ],
            }),
            _ => {}
        }
    }
    c
}

#[allow(clippy::too_many_arguments)]
fn claim(
    key: &str,
    subject: Option<&str>,
    body: &str,
    observer: &str,
    reliability: Reliability,
    credibility: Credibility,
    occurred_day: u64,
    entities: Vec<EntityRef>,
) -> Claim {
    let occ = day(occurred_day);
    Claim {
        claim_key: key.into(),
        subject_key: subject.map(Into::into),
        source_ref: format!("{observer}:{key}"),
        observer: ObserverId::from(observer),
        schema_v: 1,
        occurred_at: occ,
        observed_at: occ,
        reliability,
        credibility,
        entities,
        body: body.into(),
    }
}

fn halcyon() -> EntityRef {
    EntityRef {
        etype: "project".into(),
        id: "halcyon".into(),
        name: Some("Halcyon".into()),
    }
}
fn samuel() -> EntityRef {
    EntityRef {
        etype: "person".into(),
        id: "samuel".into(),
        name: Some("Samuel".into()),
    }
}
fn sam() -> EntityRef {
    EntityRef {
        etype: "person".into(),
        id: "sam".into(),
        name: Some("Sam".into()),
    }
}

fn unclassified_keys(c: &Clog) -> String {
    c.select(View::Unclassified, Filter::default())
        .unwrap()
        .iter()
        .map(|r| r.claim.claim_key.clone())
        .collect::<Vec<_>>()
        .join(", ")
}

#[test]
fn g1_agency_simulation() {
    let dir = tempfile::tempdir().unwrap();
    let c = Clog::open(fixture_config(dir.path())).unwrap();

    // ======================================================================
    // Checkpoint A: claims 1-2 and 5-9 (the brief's numbering; 3, 4, 10, 11
    // arrive later). Establishes the slipping-deliverable + overdue-invoice
    // risk pair, a kickoff-moved fyi, a PTO fyi on `person:samuel`, an open
    // question, an inbound opportunity, and one claim (9) that no rule
    // matches (the fixture's one permanent Unclassified item).
    // ======================================================================
    c.advance(day(20_003)).unwrap();
    c.observe(
        vec![
            claim(
                "halcyon:deliverable:slip",
                Some("halcyon:deliverable:status"),
                "Halcyon deliverable is slipping by a week",
                "twist",
                Reliability::B,
                Credibility::Three,
                20_000,
                vec![halcyon()],
            ),
            claim(
                "halcyon:inv-1042:v1",
                Some("halcyon:inv-1042:status"),
                "Invoice 1042 is 30 days overdue",
                "gmail",
                Reliability::B,
                Credibility::Two,
                20_000,
                vec![halcyon()],
            ),
            claim(
                "meridian:kickoff:moved",
                None,
                "Meridian kickoff moved to Thursday",
                "gmail",
                Reliability::B,
                Credibility::Two,
                20_000,
                vec![],
            ),
            claim(
                "meridian:pto:sam",
                Some("meridian:pto:samuel:status"),
                "Sam is on PTO next week",
                "slack",
                Reliability::C,
                Credibility::Two,
                20_000,
                vec![samuel()],
            ),
            claim(
                "meridian:question:sow",
                Some("meridian:sow"),
                "Did Meridian sign the SOW?",
                "gmail",
                Reliability::B,
                Credibility::Two,
                20_001,
                vec![],
            ),
            claim(
                "vega:lead:inbound",
                None,
                "Inbound lead from Vega Labs",
                "hubspot",
                Reliability::B,
                Credibility::Two,
                20_001,
                vec![],
            ),
            claim(
                "vega:upsell:maybe",
                None,
                "Vega mentioned maybe expanding scope",
                "twist",
                Reliability::C,
                Credibility::Three,
                20_001,
                vec![],
            ),
        ],
        ObserveOpts::default(),
    )
    .unwrap();

    insta::assert_snapshot!(
        "g1_a_delivery",
        c.situation(Some("delivery-health"), None).unwrap().text
    );
    insta::assert_snapshot!(
        "g1_a_cash",
        c.situation(Some("cash-and-collections"), None)
            .unwrap()
            .text
    );
    insta::assert_snapshot!("g1_a_unclassified", unclassified_keys(&c));

    // ======================================================================
    // Checkpoint B: claim 3 (a lower-trust "still overdue" restatement,
    // later occurred_at) then claim 4 (the bank-feed payment, earlier
    // occurred_at but far better trust). Belief goes to claim 3 — the
    // spec §5.3 known limitation: only `occurred_at` orders belief, so a
    // stale-but-fresher-dated low-trust claim beats a well-trusted one.
    // ======================================================================
    c.advance(day(3)).unwrap(); // now = day 20_006
    c.observe(
        vec![claim(
            "halcyon:inv-1042:v2",
            Some("halcyon:inv-1042:status"),
            "Invoice 1042 still overdue per bookkeeper",
            "twist",
            Reliability::C,
            Credibility::Three,
            20_004,
            vec![halcyon()],
        )],
        ObserveOpts::default(),
    )
    .unwrap();
    c.observe(
        vec![claim(
            "halcyon:inv-1042:paid",
            Some("halcyon:inv-1042:status"),
            "Payment received for invoice 1042",
            "bank-feed",
            Reliability::A,
            Credibility::One,
            20_002,
            vec![halcyon()],
        )],
        ObserveOpts::default(),
    )
    .unwrap();

    insta::assert_snapshot!(
        "g1_b_delivery",
        c.situation(Some("delivery-health"), None).unwrap().text
    );
    insta::assert_snapshot!(
        "g1_b_cash",
        c.situation(Some("cash-and-collections"), None)
            .unwrap()
            .text
    );
    insta::assert_snapshot!("g1_b_unclassified", unclassified_keys(&c));

    // `EntityState` reports only believed winners: the deliverable (its
    // subject's only claim) and claim 3 (the invoice's current winner).
    let entity_state_rows = c
        .select(
            View::EntityState,
            Filter {
                entities: Some(vec![halcyon()]),
                ..Filter::default()
            },
        )
        .unwrap();
    assert_eq!(
        entity_state_rows
            .iter()
            .map(|r| (r.claim.claim_key.as_str(), r.believed))
            .collect::<Vec<_>>(),
        vec![
            ("halcyon:deliverable:slip", Some(true)),
            ("halcyon:inv-1042:v2", Some(true))
        ],
    );
    // `Live` shows every competitor for the invoice subject with its flag:
    // claim 3 (later occurred_at) beats both claim 2 and the far-better-
    // trusted claim 4.
    let live_rows = c
        .select(
            View::Live,
            Filter {
                subject_prefix: Some("halcyon:inv-1042:".into()),
                ..Filter::default()
            },
        )
        .unwrap();
    assert_eq!(
        live_rows
            .iter()
            .map(|r| (r.claim.claim_key.as_str(), r.believed))
            .collect::<Vec<_>>(),
        vec![
            ("halcyon:inv-1042:paid", Some(false)),
            ("halcyon:inv-1042:v1", Some(false)),
            ("halcyon:inv-1042:v2", Some(true)),
        ],
    );

    // ======================================================================
    // Checkpoint C: an extra small fyi claim on `person:sam` (needed so
    // checkpoint D's merge visibly consolidates two distinct entities), then
    // retract claim 7 (the client answered — self-resolved) and retract
    // claim 3 (the erroneous "still overdue" restatement, once the payment
    // is confirmed). Both heal: the question drops out of open loops, and
    // belief on the invoice subject flips to the bank-feed claim.
    // ======================================================================
    c.advance(day(2)).unwrap(); // now = day 20_008
    c.observe(
        vec![claim(
            "meridian:sam:oneone",
            Some("meridian:sam:oneone:status"),
            "Sam moved his 1:1 to Friday",
            "slack",
            Reliability::B,
            Credibility::Two,
            20_005,
            vec![sam()],
        )],
        ObserveOpts::default(),
    )
    .unwrap();
    c.retract("meridian:question:sow").unwrap();
    c.retract("halcyon:inv-1042:v2").unwrap();

    insta::assert_snapshot!(
        "g1_c_delivery",
        c.situation(Some("delivery-health"), None).unwrap().text
    );
    insta::assert_snapshot!(
        "g1_c_cash",
        c.situation(Some("cash-and-collections"), None)
            .unwrap()
            .text
    );
    insta::assert_snapshot!("g1_c_unclassified", unclassified_keys(&c));

    let healed = c
        .select(
            View::EntityState,
            Filter {
                entities: Some(vec![halcyon()]),
                ..Filter::default()
            },
        )
        .unwrap();
    assert_eq!(
        healed
            .iter()
            .map(|r| r.claim.claim_key.as_str())
            .collect::<Vec<_>>(),
        vec!["halcyon:deliverable:slip", "halcyon:inv-1042:paid"],
        "belief must flip to the bank-feed claim once the fresher-but-wrong claim 3 is retracted",
    );

    // ======================================================================
    // Checkpoint D: merge person:samuel (claim 6's PTO note, named
    // "Samuel") into person:sam (claim 11's note, named "Sam"). Registry
    // proof: the two separate entity rows consolidate into one.
    // ======================================================================
    c.advance(day(2)).unwrap(); // now = day 20_010
    c.merge_entities(&samuel(), &sam()).unwrap();

    insta::assert_snapshot!(
        "g1_d_delivery",
        c.situation(Some("delivery-health"), None).unwrap().text
    );
    insta::assert_snapshot!(
        "g1_d_cash",
        c.situation(Some("cash-and-collections"), None)
            .unwrap()
            .text
    );
    insta::assert_snapshot!("g1_d_unclassified", unclassified_keys(&c));

    // `EntityState` orders rows (canonical entity, subject) ascending:
    // "meridian:pto:samuel:status" < "meridian:sam:oneone:status" (p < s),
    // so claim 6 (the PTO note) sorts before claim 11 even though claim 11
    // occurred later — this is `select`'s subject-key order, not the
    // newest-first order the *rendered* entities slot uses.
    let merged = c
        .select(
            View::EntityState,
            Filter {
                entities: Some(vec![sam()]),
                ..Filter::default()
            },
        )
        .unwrap();
    assert_eq!(
        merged
            .iter()
            .map(|r| r.claim.claim_key.as_str())
            .collect::<Vec<_>>(),
        vec!["meridian:pto:sam", "meridian:sam:oneone"],
        "both person:samuel's and person:sam's believed claims now group under one canonical entity"
    );
}
