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
/// *world state*, not about counters or the render-diff log.
///
/// Two things are stripped:
/// 1. the `rev {n}` marker in the header — INV-3 is about content, not revs;
/// 2. the whole `## changes since last brief` section (the default
///    template's last slot) — the controller's INV-3 ruling: the spec
///    self-conflicts, since INV-3 demands post-retraction text identical to
///    an empty world while §5.7 defines `changes` as the membership delta
///    between the last two rendered revs, so a retraction legitimately
///    echoes there exactly once. INV-3 text equality excludes that slot.
fn norm(s: &Situation) -> String {
    let mut t = s.text.replace(&format!("rev {}", s.rev), "rev _");
    if let Some(i) = t.find("\n## changes since last brief\n") {
        t.truncate(i);
    }
    t
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
