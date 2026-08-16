//! Public-API integration tests for the `Clog` handle (Task 14).
//!
//! Everything here goes through the public surface only: no crate internals,
//! no test-only back doors. The clock is always `Manual` (INV-10).

use clog::*;

fn cfg(dir: &std::path::Path) -> Config {
    let mut c = Config::default_for(dir);
    c.tick = TickConfig { mode: ClockMode::Manual, interval_ms: 60_000 };
    c
}

fn claim(key: &str, body: &str, occ: u64) -> Claim {
    Claim {
        claim_key: key.into(),
        subject_key: None,
        source_ref: "t:1".into(),
        observer: ObserverId::from("test"),
        schema_v: 1,
        occurred_at: occ,
        observed_at: occ,
        reliability: Reliability::B,
        credibility: Credibility::Two,
        entities: vec![],
        body: body.into(),
    }
}

/// Normalizes a situation's text for equality comparisons that are about
/// *world state*, not about counters, clocks or the render-diff log.
///
/// Two things are stripped:
/// 1. the entire header line — it carries the rev and the `as_of`
///    timestamp, both of which are bookkeeping rather than content;
/// 2. the whole `## changes since last brief` section (the default
///    template's last slot) — the controller's INV-3 ruling: the spec
///    self-conflicts, since INV-3 demands post-retraction text identical to
///    an empty world while §5.7 defines `changes` as the membership delta
///    between the last two rendered revs, so a retraction legitimately
///    echoes there exactly once. INV-3 text equality excludes that slot.
fn norm(s: &Situation) -> String {
    let body = s.text.split_once('\n').map_or("", |(_, rest)| rest);
    match body.find("\n## changes since last brief\n") {
        Some(i) => body[..i].to_string(),
        None => body.to_string(),
    }
}

/// The handle is a cheap-clone, thread-safe port (spec §6.1).
#[test]
fn clog_handle_is_clone_send_sync() {
    fn is<T: Clone + Send + Sync>() {}
    is::<Clog>();
}

#[test]
fn observe_bumps_rev_and_renders() {
    let dir = tempfile::tempdir().unwrap();
    let c = Clog::open(cfg(dir.path())).unwrap();
    c.advance(1_000_000).unwrap();
    let ack = c.observe(vec![claim("a", "first thing", 500_000)], ObserveOpts::default()).unwrap();
    assert_eq!(ack.rev, 1);
    let s = c.situation(None, None).unwrap();
    assert_eq!(s.scope, "default");
    assert_eq!(s.rev, 1);
    assert!(s.text.contains("first thing"));
}

#[test]
fn inv5_duplicate_observe_is_invisible() {
    let dir = tempfile::tempdir().unwrap();
    let c = Clog::open(cfg(dir.path())).unwrap();
    c.advance(1_000_000).unwrap();
    let a1 = c.observe(vec![claim("a", "x", 500_000)], ObserveOpts::default()).unwrap();
    let s1 = c.situation(None, None).unwrap();
    let a2 = c.observe(vec![claim("a", "x", 500_000)], ObserveOpts::default()).unwrap();
    let s2 = c.situation(None, None).unwrap();
    assert_eq!(a2.rev, a1.rev, "duplicate batch must not commit");
    assert_eq!(s1.rev, s2.rev);
    assert_eq!(s1.text, s2.text);
}

#[test]
fn inv4_upsert_supersedes() {
    let dir = tempfile::tempdir().unwrap();
    let c = Clog::open(cfg(dir.path())).unwrap();
    c.advance(1_000_000).unwrap();
    c.observe(vec![claim("a", "old body", 500_000)], ObserveOpts::default()).unwrap();
    c.observe(vec![claim("a", "new body", 600_000)], ObserveOpts::default()).unwrap();
    let s = c.situation(None, None).unwrap();
    assert!(s.text.contains("new body"));
    assert!(!s.text.contains("old body"));
}

#[test]
fn inv3_retraction_heals_text() {
    let dir = tempfile::tempdir().unwrap();
    let c = Clog::open(cfg(dir.path())).unwrap();
    c.advance(1_000_000).unwrap();
    let empty = c.situation(None, None).unwrap();
    c.observe(vec![claim("a", "temp", 500_000)], ObserveOpts::default()).unwrap();
    c.retract("a").unwrap();
    let healed = c.situation(None, None).unwrap();
    assert_eq!(norm(&healed), norm(&empty));
    assert!(matches!(c.retract("a"), Err(ClogError::UnknownClaim)));
}

#[test]
fn inv9_rev_survives_reopen() {
    let dir = tempfile::tempdir().unwrap();
    {
        let c = Clog::open(cfg(dir.path())).unwrap();
        c.advance(1_000_000).unwrap();
        c.observe(vec![claim("a", "x", 500_000)], ObserveOpts::default()).unwrap();
        c.observe(vec![claim("b", "y", 500_000)], ObserveOpts::default()).unwrap();
    } // drop -> clean shutdown
    let c = Clog::open(cfg(dir.path())).unwrap();
    c.advance(2_000_000).unwrap();
    let ack = c.observe(vec![claim("c", "z", 500_000)], ObserveOpts::default()).unwrap();
    assert_eq!(ack.rev, 3);
    let s = c.situation(None, None).unwrap();
    assert!(s.text.contains('x') && s.text.contains('z'));
}

// Beyond the brief: INV-9 above checks the rev survives a reopen; the
// document has to survive it too, byte for byte, which is why each batch
// carries the clock reading it was committed at.
#[test]
fn reopen_reproduces_situation_text_byte_identically() {
    let dir = tempfile::tempdir().unwrap();
    let before = {
        let c = Clog::open(cfg(dir.path())).unwrap();
        c.advance(1_000_000).unwrap();
        c.observe(vec![claim("a", "first", 500_000)], ObserveOpts::default()).unwrap();
        // a different clock reading for the second batch: replaying both
        // against one reopen-time reading would render a different header
        c.advance(500_000).unwrap();
        c.observe(vec![claim("b", "second", 900_000)], ObserveOpts::default()).unwrap();
        c.situation(None, None).unwrap()
    };
    let c = Clog::open(cfg(dir.path())).unwrap();
    let after = c.situation(None, None).unwrap();
    assert_eq!(after.text, before.text);
    assert_eq!(after.rev, before.rev);
    assert_eq!(after.as_of, before.as_of);
}

#[test]
fn reserved_namespace_rejected_and_batch_atomic() {
    let dir = tempfile::tempdir().unwrap();
    let c = Clog::open(cfg(dir.path())).unwrap();
    c.advance(1_000_000).unwrap();
    let r = c.observe(
        vec![claim("ok", "fine", 500_000), claim("clog:sneaky", "no", 500_000)],
        ObserveOpts::default(),
    );
    assert!(r.is_err());
    // atomic: the valid claim must not have landed either
    assert!(!c.situation(None, None).unwrap().text.contains("fine"));
}

#[test]
fn rules_tier_classifies_at_commit() {
    let dir = tempfile::tempdir().unwrap();
    let mut config = cfg(dir.path());
    for kd in &mut config.kinds.kinds {
        if kd.name == "risk" {
            kd.rules.push(clog::Rule { any_of: vec![clog::Matcher::BodyContains("overdue".into())] });
        }
    }
    let c = Clog::open(config).unwrap();
    c.advance(1_000_000).unwrap();
    c.observe(vec![claim("inv", "invoice 1042 is overdue", 500_000)], ObserveOpts::default()).unwrap();
    let s = c.situation(None, None).unwrap();
    assert!(s.text.contains("- RISK invoice 1042"), "open loops slot should show it:\n{}", s.text);
}

// Beyond the brief: a batch may carry two versions of one key. Only the
// version that survives the batch may be classified — judging every observed
// version would durably label the survivor with a superseded version's kind.
#[test]
fn only_the_surviving_version_of_a_key_is_classified() {
    let dir = tempfile::tempdir().unwrap();
    let mut config = cfg(dir.path());
    for kd in &mut config.kinds.kinds {
        if kd.name == "risk" {
            kd.rules.push(clog::Rule { any_of: vec![clog::Matcher::BodyContains("overdue".into())] });
        }
    }
    let c = Clog::open(config).unwrap();
    c.advance(1_000_000).unwrap();
    // one batch, two versions of "inv": only the first matches the risk rule
    c.observe(
        vec![claim("inv", "invoice 1042 is overdue", 500_000), claim("inv", "invoice 1042 is paid", 600_000)],
        ObserveOpts::default(),
    )
    .unwrap();

    let s = c.situation(None, None).unwrap();
    assert!(s.text.contains("invoice 1042 is paid"), "{}", s.text);
    // the survivor never matched the rule, so it is unclassified: no open loop
    assert!(!s.text.contains("RISK"), "surviving claim must not inherit the superseded version's kind:\n{}", s.text);
    assert!(s.text.contains("## open loops\n(none)"), "{}", s.text);
}

#[test]
fn custom_template_and_errors() {
    let dir = tempfile::tempdir().unwrap();
    let c = Clog::open(cfg(dir.path())).unwrap();
    c.advance(1_000_000).unwrap();
    c.observe(vec![claim("a", "hello world", 500_000)], ObserveOpts::default()).unwrap();
    let s = c.situation(None, Some("URGENT ONLY\n%{urgent limit=1}")).unwrap();
    assert!(s.text.starts_with("URGENT ONLY\n1."), "{}", s.text);
    assert!(matches!(c.situation(None, Some("%{bogus}")), Err(ClogError::TemplateError(_))));
    assert!(matches!(c.situation(Some("nope"), None), Err(ClogError::UnknownScope)));
}

// Beyond the brief: a change confined to items the template's `limit=` hides
// is invisible in the document but must still refresh what a custom-template
// read assembles from — and must not move the scope's rev (spec §5.10).
#[test]
fn hidden_item_change_keeps_the_document_but_refreshes_its_inputs() {
    let dir = tempfile::tempdir().unwrap();
    let c = Clog::open(cfg(dir.path())).unwrap();
    c.advance(1_000_000).unwrap();
    // Ten claims with identical trust and occurred_at: equal scores, so the
    // claim_key tiebreak orders them k00..k09 and the default template's
    // `%{urgent limit=8}` hides the last two.
    let batch: Vec<Claim> =
        (0..10).map(|i| claim(&format!("k{i:02}"), &format!("body {i:02}"), 500_000)).collect();
    c.observe(batch, ObserveOpts::default()).unwrap();
    // Rewrite one hidden item, so that this render and the next both sit on
    // an unchanged membership and an empty `changes` slot — isolating the
    // hidden-item edit as the only difference between them.
    let settled = c.observe(vec![claim("k09", "rewritten tail", 500_000)], ObserveOpts::default()).unwrap();
    let before = c.situation(None, None).unwrap();
    assert!(before.text.contains("… (2 more)"), "{}", before.text);

    let ack = c.observe(vec![claim("k08", "second rewrite", 500_000)], ObserveOpts::default()).unwrap();
    assert_eq!(ack.rev, settled.rev + 1, "the upsert did commit a batch");

    let after = c.situation(None, None).unwrap();
    assert_eq!(after.text, before.text, "a hidden item's body never reaches the document");
    assert_eq!(after.rev, before.rev, "unchanged text must keep its rev (rev skew is the signal)");
    assert_eq!(after.as_of, before.as_of, "and its as_of: nothing material changed");
    assert!(after.rev < ack.rev, "the scope's rev now lags the global rev, as it should");

    // The stored slot inputs did move, though: lift the cap and both new
    // bodies are there, under the same rev the default document reports.
    let wide = c.situation(None, Some("%{header}\n%{urgent limit=10}")).unwrap();
    assert!(wide.text.contains("rewritten tail"), "{}", wide.text);
    assert!(wide.text.contains("second rewrite"), "{}", wide.text);
    assert!(!wide.text.contains("body 08") && !wide.text.contains("body 09"), "{}", wide.text);
    assert!(wide.text.starts_with(&format!("default · rev {}", after.rev)), "{}", wide.text);
}

// Beyond the brief: nothing above renders the `entities` slot, and it is the
// one slot whose inputs come from belief resolution rather than a flat view.
#[test]
fn entities_slot_shows_believed_summaries_newest_first() {
    let dir = tempfile::tempdir().unwrap();
    let c = Clog::open(cfg(dir.path())).unwrap();
    c.advance(1_000_000).unwrap();
    let entity = EntityRef { etype: "project".into(), id: "halcyon".into(), name: Some("Halcyon".into()) };
    let mut old = claim("old", "invoice 1042 overdue", 100_000);
    old.subject_key = Some("halcyon:inv-1042:status".into());
    old.entities = vec![entity.clone()];
    let mut new = claim("new", "invoice 1042 paid", 200_000);
    new.subject_key = Some("halcyon:inv-1042:status".into());
    new.entities = vec![entity.clone()];
    let mut other = claim("kick", "kickoff moved to may", 150_000);
    other.subject_key = Some("halcyon:kickoff".into());
    other.entities = vec![entity];
    c.observe(vec![old, new, other], ObserveOpts::default()).unwrap();

    let text = c.situation(None, None).unwrap().text;
    // display name from the registry; only believed claims; newest occurred_at first
    assert!(text.contains("Halcyon: invoice 1042 paid; kickoff moved to may"), "{text}");
    assert!(!text.contains("Halcyon: invoice 1042 overdue"), "{text}");
}

#[test]
fn select_live_with_filters() {
    let dir = tempfile::tempdir().unwrap();
    let c = Clog::open(cfg(dir.path())).unwrap();
    c.advance(1_000_000).unwrap();
    let mut a = claim("a", "alpha", 100_000);
    a.observer = ObserverId::from("gmail");
    let mut b = claim("b", "beta", 900_000);
    b.observer = ObserverId::from("twist");
    c.observe(vec![a, b], ObserveOpts::default()).unwrap();

    let all = c.select(View::Live, Filter::default()).unwrap();
    assert_eq!(all.iter().map(|r| r.claim.claim_key.as_str()).collect::<Vec<_>>(), vec!["a", "b"]);
    assert!(all[0].recorded_at >= 1_000_000);

    let f = Filter { observer: Some(ObserverId::from("twist")), ..Filter::default() };
    assert_eq!(c.select(View::Live, f).unwrap().len(), 1);

    let f = Filter { occurred_after: Some(500_000), ..Filter::default() };
    assert_eq!(c.select(View::Live, f).unwrap()[0].claim.claim_key, "b");

    let f = Filter { min_score: Some(0.1), ..Filter::default() };
    assert!(matches!(c.select(View::Live, f), Err(ClogError::InvalidFilter { .. })));

    let rows = c.select(View::Urgent { scope: "default".into() }, Filter::default()).unwrap();
    assert!(rows[0].score.is_some());
}

#[test]
fn inv6_revoke_observer_one_batch() {
    let dir = tempfile::tempdir().unwrap();
    let c = Clog::open(cfg(dir.path())).unwrap();
    c.advance(1_000_000).unwrap();
    let mut a = claim("a", "alpha", 100_000); a.observer = ObserverId::from("gmail");
    let mut b = claim("b", "beta", 100_000);  b.observer = ObserverId::from("gmail");
    c.observe(vec![a, b], ObserveOpts::default()).unwrap();
    let ack = c.revoke_observer(&ObserverId::from("gmail")).unwrap();
    assert_eq!(ack.rev, 2); // one batch, one rev
    assert!(c.select(View::Live, Filter::default()).unwrap().is_empty());
}

#[test]
fn p7_shape_merge_round_trip_via_api() {
    let dir = tempfile::tempdir().unwrap();
    let c = Clog::open(cfg(dir.path())).unwrap();
    c.advance(1_000_000).unwrap();
    let mut cl = claim("about-a", "note about a", 100_000);
    cl.entities = vec![EntityRef { etype: "p".into(), id: "a".into(), name: None }];
    c.observe(vec![cl], ObserveOpts::default()).unwrap();
    let before = c.situation(None, None).unwrap();

    let a = EntityRef { etype: "p".into(), id: "a".into(), name: None };
    let b = EntityRef { etype: "p".into(), id: "b".into(), name: None };
    c.merge_entities(&a, &b).unwrap();
    // entity filter follows the alias
    let f = Filter { entities: Some(vec![b.clone()]), ..Filter::default() };
    assert_eq!(c.select(View::Live, f).unwrap().len(), 1);
    // cycle rejected
    assert!(matches!(c.merge_entities(&b, &a), Err(ClogError::AliasCycle)));
    // merge claim is invisible (INV-8)
    assert!(c.select(View::Live, Filter::default()).unwrap().iter().all(|r| !r.claim.claim_key.starts_with("clog:")));
    // un-merge by retracting the reserved key
    c.retract("clog:merge:p:a->p:b").unwrap();
    let after = c.situation(None, None).unwrap();
    // `norm` rather than the brief's rev-replace: the header also carries
    // `as_of`, which the merge/un-merge batches moved, and the `changes`
    // slot legitimately echoes them (see `norm`'s doc comment).
    assert_eq!(norm(&before), norm(&after));
}

// Beyond the brief: the filters above are each exercised alone. They are
// AND-composed, they exclude rows that *cannot* answer them, and `limit`
// has a default and a ceiling.
#[test]
fn select_filters_and_compose_and_limit_is_bounded() {
    let dir = tempfile::tempdir().unwrap();
    let mut config = cfg(dir.path());
    for kd in &mut config.kinds.kinds {
        if kd.name == "risk" {
            kd.rules.push(clog::Rule { any_of: vec![clog::Matcher::BodyContains("overdue".into())] });
        }
    }
    let c = Clog::open(config).unwrap();
    c.advance(1_000_000).unwrap();

    // One row passes everything, and one row fails each clause on its own,
    // so dropping any single conjunct must let exactly one more through.
    let row = |key: &str, body: &str, observer: &str, subject: &str, occ: u64| {
        let mut c = claim(key, body, occ);
        c.observer = ObserverId::from(observer);
        c.subject_key = Some(subject.into());
        c
    };
    c.observe(
        vec![
            row("hit", "invoice overdue", "gmail", "inv:status", 900_000),
            row("no-kind", "invoice settled", "gmail", "inv:note", 900_000),
            row("wrong-observer", "invoice overdue", "twist", "inv:other", 900_000),
            row("wrong-subject", "rent overdue", "gmail", "rent:status", 900_000),
            row("too-old", "invoice overdue", "gmail", "inv:history", 100_000),
            claim("no-subject", "chatter overdue", 900_000),
        ],
        ObserveOpts::default(),
    )
    .unwrap();

    let all = Filter {
        kinds: Some(vec!["risk".into()]),
        observer: Some(ObserverId::from("gmail")),
        subject_prefix: Some("inv:".into()),
        occurred_after: Some(500_000),
        ..Filter::default()
    };
    let rows = c.select(View::Live, all.clone()).unwrap();
    assert_eq!(rows.iter().map(|r| r.claim.claim_key.as_str()).collect::<Vec<_>>(), vec!["hit"]);
    assert_eq!(c.select(View::Live, Filter { kinds: None, ..all.clone() }).unwrap().len(), 2);
    assert_eq!(c.select(View::Live, Filter { observer: None, ..all.clone() }).unwrap().len(), 2);
    assert_eq!(c.select(View::Live, Filter { subject_prefix: None, ..all.clone() }).unwrap().len(), 2);
    assert_eq!(c.select(View::Live, Filter { occurred_after: None, ..all }).unwrap().len(), 2);

    // a claim with no subject_key cannot answer a subject_prefix filter,
    // not even the empty one every subject starts with
    let f = Filter { subject_prefix: Some(String::new()), ..Filter::default() };
    assert_eq!(c.select(View::Live, f).unwrap().len(), 5);
    // an unclassified claim cannot answer a kinds filter
    let f = Filter { kinds: Some(vec!["fyi".into()]), ..Filter::default() };
    assert!(c.select(View::Live, f).unwrap().is_empty());
    // occurred_after is strict
    let f = Filter { occurred_after: Some(900_000), ..Filter::default() };
    assert!(c.select(View::Live, f).unwrap().is_empty());

    // an empty entity list names no entity, so it matches nothing
    let f = Filter { entities: Some(vec![]), ..Filter::default() };
    assert!(c.select(View::Live, f).unwrap().is_empty());

    // limit: honoured, clamped rather than rejected, and zero means zero
    let f = Filter { limit: Some(2), ..Filter::default() };
    assert_eq!(c.select(View::Live, f).unwrap().len(), 2);
    let f = Filter { limit: Some(100_000), ..Filter::default() };
    assert_eq!(c.select(View::Live, f).unwrap().len(), 6);
    let f = Filter { limit: Some(0), ..Filter::default() };
    assert!(c.select(View::Live, f).unwrap().is_empty());

    // View::OpenLoops: the five "overdue" claims classified risk (a loop
    // kind), in claim_key order, each carrying its label; "no-kind" is not
    // a loop and does not appear.
    let loops = c.select(View::OpenLoops, Filter::default()).unwrap();
    assert_eq!(
        loops.iter().map(|r| r.claim.claim_key.as_str()).collect::<Vec<_>>(),
        vec!["hit", "no-subject", "too-old", "wrong-observer", "wrong-subject"]
    );
    assert!(loops.iter().all(|r| r.kind.as_ref().is_some_and(|k| k.kind == "risk")));
    assert!(loops.iter().all(|r| r.score.is_none()), "only Urgent ranks");
    // and the same filters compose over it
    let f = Filter { observer: Some(ObserverId::from("twist")), ..Filter::default() };
    assert_eq!(c.select(View::OpenLoops, f).unwrap().len(), 1);

    assert!(matches!(c.select(View::Urgent { scope: "nope".into() }, Filter::default()), Err(ClogError::UnknownScope)));
}

// Beyond the brief: `believed` is three-valued, and `EntityState` reports
// only the winners — the flag and the view must agree.
#[test]
fn select_believed_flag_and_entity_state_rows() {
    let dir = tempfile::tempdir().unwrap();
    let c = Clog::open(cfg(dir.path())).unwrap();
    c.advance(1_000_000).unwrap();
    let entity = EntityRef { etype: "project".into(), id: "halcyon".into(), name: None };
    let mut loser = claim("a-loser", "invoice overdue", 100_000);
    loser.subject_key = Some("inv:status".into());
    loser.entities = vec![entity.clone()];
    let mut winner = claim("b-winner", "invoice paid", 200_000);
    winner.subject_key = Some("inv:status".into());
    winner.entities = vec![entity.clone()];
    let loose = claim("c-loose", "no subject at all", 200_000);
    c.observe(vec![loser, winner, loose], ObserveOpts::default()).unwrap();

    let rows = c.select(View::Live, Filter::default()).unwrap();
    let flags: Vec<(&str, Option<bool>)> =
        rows.iter().map(|r| (r.claim.claim_key.as_str(), r.believed)).collect();
    assert_eq!(flags, vec![("a-loser", Some(false)), ("b-winner", Some(true)), ("c-loose", None)]);

    // entity_state carries only the subject's winner, flagged accordingly
    let rows = c.select(View::EntityState, Filter::default()).unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].claim.claim_key, "b-winner");
    assert_eq!(rows[0].believed, Some(true));
    // and the entity filter reaches it
    let f = Filter { entities: Some(vec![entity]), ..Filter::default() };
    assert_eq!(c.select(View::EntityState, f).unwrap().len(), 1);
}

// Beyond the brief: revoking an observer with nothing live must be as
// invisible as a duplicate observe (INV-5), not an error.
#[test]
fn revoke_of_an_unknown_observer_commits_nothing() {
    let dir = tempfile::tempdir().unwrap();
    let c = Clog::open(cfg(dir.path())).unwrap();
    c.advance(1_000_000).unwrap();
    let ack = c.observe(vec![claim("a", "x", 500_000)], ObserveOpts::default()).unwrap();
    assert_eq!(c.revoke_observer(&ObserverId::from("nobody")).unwrap().rev, ack.rev);
    let done = c.revoke_observer(&ObserverId::from("test")).unwrap();
    assert_eq!(done.rev, ack.rev + 1);
    // and again: now there is nothing left, so it is a no-op
    assert_eq!(c.revoke_observer(&ObserverId::from("test")).unwrap().rev, done.rev);
}

// Beyond the brief: a self-merge is a cycle too, and a repeated merge is an
// INV-5 no-op rather than a second revision.
#[test]
fn merge_self_is_a_cycle_and_re_merging_is_a_no_op() {
    let dir = tempfile::tempdir().unwrap();
    let c = Clog::open(cfg(dir.path())).unwrap();
    c.advance(1_000_000).unwrap();
    let a = EntityRef { etype: "p".into(), id: "a".into(), name: None };
    let b = EntityRef { etype: "p".into(), id: "b".into(), name: None };
    assert!(matches!(c.merge_entities(&a, &a), Err(ClogError::AliasCycle)));
    let first = c.merge_entities(&a, &b).unwrap();
    assert_eq!(c.merge_entities(&a, &b).unwrap().rev, first.rev, "identical merge must not commit");
    // A self-merge of an *already aliased* entity is still a cycle: the
    // flattened target of `a` is now `b`, so only an identity check catches
    // it, and it must not mint an inert `a -> a` claim.
    assert!(matches!(c.merge_entities(&a, &a), Err(ClogError::AliasCycle)));
    assert_eq!(c.select(View::Live, Filter::default()).unwrap().len(), 0);
    assert!(matches!(c.retract("clog:merge:p:a->p:a"), Err(ClogError::UnknownClaim)));
    // §10 still applies to the entity refs a merge names
    let bad = EntityRef { etype: "p".into(), id: String::new(), name: None };
    assert!(matches!(c.merge_entities(&bad, &b), Err(ClogError::InvalidClaim { .. })));
}

// The 8-row cap in §5.3 is scoped "for rendering": the rendered entities
// slot summarizes, but a structured read must enumerate everything believed
// about an entity, or a caller has no way to tell it saw a truncated world.
#[test]
fn entity_state_select_is_uncapped_while_the_rendered_slot_still_summarizes() {
    let dir = tempfile::tempdir().unwrap();
    let c = Clog::open(cfg(dir.path())).unwrap();
    c.advance(1_000_000).unwrap();
    let entity = EntityRef { etype: "proj".into(), id: "h".into(), name: Some("Halcyon".into()) };
    // Ten subjects, one believed claim each, all on the same entity.
    let batch: Vec<Claim> = (0..10)
        .map(|i| {
            let mut cl = claim(&format!("k{i:02}"), &format!("subject {i:02} update"), 100_000 + i);
            cl.subject_key = Some(format!("s{i:02}"));
            cl.entities = vec![entity.clone()];
            cl
        })
        .collect();
    c.observe(batch, ObserveOpts::default()).unwrap();

    // select: all ten, in (entity, subject) ascending order
    let rows = c.select(View::EntityState, Filter::default()).unwrap();
    assert_eq!(rows.len(), 10, "select must not inherit the render cap");
    assert_eq!(
        rows.iter().map(|r| r.claim.subject_key.as_deref().unwrap_or("")).collect::<Vec<_>>(),
        (0..10).map(|i| format!("s{i:02}")).collect::<Vec<_>>()
    );
    assert!(rows.iter().all(|r| r.believed == Some(true)));

    // the rendered slot still shows the newest 8 summaries on one line
    let text = c.situation(None, None).unwrap().text;
    let line = text.lines().find(|l| l.starts_with("Halcyon: ")).expect("entities slot");
    let summaries: Vec<&str> = line.trim_start_matches("Halcyon: ").split("; ").collect();
    assert_eq!(summaries.len(), 8, "{line}");
    assert!(summaries[0].starts_with("subject 09"), "{line}"); // newest first
    assert!(!line.contains("subject 00") && !line.contains("subject 01"), "{line}");

    // and the caller's own limit is the only cap that applies to select
    let f = Filter { limit: Some(3), ..Filter::default() };
    assert_eq!(c.select(View::EntityState, f).unwrap().len(), 3);
}

// A claim mentioning several entities is believed under each of them.
// `Row` carries no entity, so repeating it would be byte-identical noise:
// EntityState reports it once, at its lowest-ordered entity.
#[test]
fn entity_state_select_reports_a_multi_entity_claim_once() {
    let dir = tempfile::tempdir().unwrap();
    let c = Clog::open(cfg(dir.path())).unwrap();
    c.advance(1_000_000).unwrap();
    let x = EntityRef { etype: "p".into(), id: "x".into(), name: None };
    let y = EntityRef { etype: "p".into(), id: "y".into(), name: None };
    let mut both = claim("both", "concerns x and y", 100_000);
    both.subject_key = Some("s1".into());
    both.entities = vec![x.clone(), y.clone()];
    c.observe(vec![both], ObserveOpts::default()).unwrap();
    // it really is indexed under both entities
    assert_eq!(c.select(View::EntityState, Filter { entities: Some(vec![x.clone()]), ..Filter::default() }).unwrap().len(), 1);
    assert_eq!(c.select(View::EntityState, Filter { entities: Some(vec![y.clone()]), ..Filter::default() }).unwrap().len(), 1);
    // ...and still yields exactly one row, filtered or not
    assert_eq!(c.select(View::EntityState, Filter::default()).unwrap().len(), 1);
    let f = Filter { entities: Some(vec![x, y]), ..Filter::default() };
    assert_eq!(c.select(View::EntityState, f).unwrap().len(), 1);
}
