//! Observe-time validation rules (spec §10, test U-VAL-1).
//!
//! These functions are internal: hosts never call them directly, they run
//! inside `Clog::observe` (and friends) before a batch is committed. The
//! whole batch is rejected on the first invalid claim (atomic, no partial
//! commit).

use crate::types::{ClogError, Claim, Focus, KindTaxonomy};

/// Builds a `Claim` with every field satisfying §10, for tests to mutate.
///
/// Defined at module level (not inside `mod tests`) so later task's test
/// modules can reuse it via `crate::validate::tests_base_claim`.
#[cfg(test)]
pub(crate) fn tests_base_claim() -> Claim {
    use crate::types::{Credibility, ObserverId, Reliability};
    Claim {
        claim_key: "k1".into(),
        subject_key: None,
        source_ref: "src:1".into(),
        observer: ObserverId::from("o1"),
        schema_v: 1,
        occurred_at: 1,
        observed_at: 1,
        reliability: Reliability::A,
        credibility: Credibility::One,
        entities: vec![],
        body: "b".into(),
    }
}

/// Validates a single observed `Claim` against §10's rules.
///
/// `index` is the claim's position within its batch, echoed back in
/// `ClogError::InvalidClaim` for host-side error reporting. `allow_reserved`
/// is set only by internal writers (e.g. merge claims), which are permitted
/// to use the `clog:` namespace reserved from hosts by INV-8.
pub(crate) fn validate_claim(index: usize, c: &Claim, allow_reserved: bool) -> Result<(), ClogError> {
    fn has_control(s: &str) -> bool {
        s.chars().any(|ch| ch.is_control())
    }
    fn invalid(index: usize, reason: impl Into<String>) -> ClogError {
        ClogError::InvalidClaim { index, reason: reason.into() }
    }

    if c.claim_key.trim().is_empty() {
        return Err(invalid(index, "claim_key must be non-empty after trim"));
    }
    if c.claim_key.len() > 256 {
        return Err(invalid(index, "claim_key must be <= 256 bytes"));
    }
    if has_control(&c.claim_key) {
        return Err(invalid(index, "claim_key must not contain control characters"));
    }
    if c.claim_key.starts_with("clog:") && !allow_reserved {
        return Err(ClogError::ReservedNamespace);
    }

    if let Some(subject_key) = &c.subject_key {
        if subject_key.len() > 256 {
            return Err(invalid(index, "subject_key must be <= 256 bytes"));
        }
        if has_control(subject_key) {
            return Err(invalid(index, "subject_key must not contain control characters"));
        }
    }

    if c.source_ref.trim().is_empty() {
        return Err(invalid(index, "source_ref must be non-empty"));
    }
    if c.source_ref.len() > 1024 {
        return Err(invalid(index, "source_ref must be <= 1024 bytes"));
    }
    if has_control(&c.source_ref) {
        return Err(invalid(index, "source_ref must not contain control characters"));
    }

    if c.observer.0.trim().is_empty() {
        return Err(invalid(index, "observer must be non-empty"));
    }
    if c.observer.0.len() > 128 {
        return Err(invalid(index, "observer must be <= 128 bytes"));
    }

    if c.body.trim().is_empty() {
        return Err(invalid(index, "body must be non-empty after trim"));
    }
    if c.body.len() > 16 * 1024 {
        return Err(invalid(index, "body must be <= 16 KiB"));
    }

    if c.entities.len() > 32 {
        return Err(invalid(index, "entities must be <= 32"));
    }
    for e in &c.entities {
        if e.etype.trim().is_empty() {
            return Err(invalid(index, "entity etype must be non-empty"));
        }
        if e.etype.len() > 128 {
            return Err(invalid(index, "entity etype must be <= 128 bytes"));
        }
        // Entity keys are joined with the ASCII unit separator in reserved
        // merge-claim bodies (see `engine::naive`); banning control chars
        // here is what makes that encoding unambiguous.
        if has_control(&e.etype) {
            return Err(invalid(index, "entity etype must not contain control characters"));
        }
        if e.id.trim().is_empty() {
            return Err(invalid(index, "entity id must be non-empty"));
        }
        if e.id.len() > 128 {
            return Err(invalid(index, "entity id must be <= 128 bytes"));
        }
        if has_control(&e.id) {
            return Err(invalid(index, "entity id must not contain control characters"));
        }
    }

    if c.occurred_at == 0 {
        return Err(invalid(index, "occurred_at must be > 0"));
    }
    if c.observed_at == 0 {
        return Err(invalid(index, "observed_at must be > 0"));
    }
    // occurred_at > observed_at is allowed (predictions/backdated corrections).

    Ok(())
}

/// Validates a `Focus` against §10's rules, given the active `KindTaxonomy`.
///
/// Every weight key must name a defined kind; weight values and boost
/// factors must be finite and strictly positive; `half_life_days` must lie
/// in the open interval `(0.01, 3650)`.
pub(crate) fn validate_focus(f: &Focus, taxonomy: &KindTaxonomy) -> Result<(), ClogError> {
    fn valid_factor(v: f32) -> bool {
        v.is_finite() && v > 0.0
    }

    for (kind, w) in &f.weights {
        if !taxonomy.contains(kind) {
            return Err(ClogError::UnknownKind);
        }
        if !valid_factor(*w) {
            return Err(ClogError::InvalidFilter { reason: format!("focus weight for {kind} must be finite and > 0") });
        }
    }

    for (_, boost) in &f.boosts {
        if !valid_factor(*boost) {
            return Err(ClogError::InvalidFilter { reason: "focus boost factor must be finite and > 0".into() });
        }
    }

    if !(0.01 < f.half_life_days && f.half_life_days < 3650.0) {
        return Err(ClogError::InvalidFilter { reason: "half_life_days must be in (0.01, 3650)".into() });
    }

    Ok(())
}

/// Clamps a timestamp for scoring purposes only (§10): values more than 24h
/// beyond `now` are clamped to `now`. Storage always keeps the verbatim
/// value; only scoring consumes this clamped result.
pub(crate) fn scoring_clamp(ts: u64, now: u64) -> u64 {
    if ts > now + 86_400_000 {
        now
    } else {
        ts
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::*;

    #[test]
    #[allow(clippy::type_complexity)] // table-driven cases, per the brief's spec verbatim
    fn u_val_1_claim_rules() {
        // (mutation, should_pass, reason-substring)
        let cases: Vec<(Box<dyn Fn(&mut Claim)>, bool, &str)> = vec![
            (Box::new(|_| {}), true, ""),
            (Box::new(|c| c.claim_key = "  ".into()), false, "claim_key"),
            (Box::new(|c| c.claim_key = "x".repeat(257)), false, "claim_key"),
            (Box::new(|c| c.claim_key = "clog:evil".into()), false, "reserved"),
            (Box::new(|c| c.claim_key = "has\u{0007}bell".into()), false, "control"),
            (Box::new(|c| c.subject_key = Some("x".repeat(257))), false, "subject_key"),
            (Box::new(|c| c.subject_key = Some("has\u{0007}bell".into())), false, "control"),
            (Box::new(|c| c.source_ref = "".into()), false, "source_ref"),
            (Box::new(|c| c.source_ref = "x".repeat(1025)), false, "source_ref"),
            (Box::new(|c| c.source_ref = "has\u{0007}bell".into()), false, "control"),
            (Box::new(|c| c.observer = ObserverId(String::new())), false, "observer"),
            (Box::new(|c| c.observer = ObserverId("x".repeat(129))), false, "observer"),
            (Box::new(|c| c.body = "   ".into()), false, "body"),
            (Box::new(|c| c.body = "x".repeat(16 * 1024 + 1)), false, "body"),
            (Box::new(|c| c.entities = vec![EntityRef { etype: "p".into(), id: "i".into(), name: None }; 33]), false, "entities"),
            (Box::new(|c| c.entities = vec![EntityRef { etype: "".into(), id: "i".into(), name: None }]), false, "etype"),
            (Box::new(|c| c.entities = vec![EntityRef { etype: "x".repeat(129), id: "i".into(), name: None }]), false, "etype"),
            (Box::new(|c| c.entities = vec![EntityRef { etype: "p".into(), id: "x".repeat(129), name: None }]), false, "id"),
            (Box::new(|c| c.entities = vec![EntityRef { etype: "p".into(), id: "".into(), name: None }]), false, "id"),
            (Box::new(|c| c.entities = vec![EntityRef { etype: "has\u{0007}bell".into(), id: "i".into(), name: None }]), false, "control"),
            // U+001F is the merge-claim body separator: it must never reach an entity key.
            (Box::new(|c| c.entities = vec![EntityRef { etype: "p".into(), id: "a\u{001f}b".into(), name: None }]), false, "control"),
            (Box::new(|c| c.occurred_at = 0), false, "occurred_at"),
            (Box::new(|c| c.observed_at = 0), false, "observed_at"),
            // occurred_at > observed_at is ALLOWED (predictions)
            (Box::new(|c| { c.occurred_at = 10; c.observed_at = 5; }), true, ""),
            // valid subject_key + non-empty entities vec
            (Box::new(|c| {
                c.subject_key = Some("valid-subject".into());
                c.entities = vec![EntityRef { etype: "project".into(), id: "halcyon".into(), name: None }];
            }), true, ""),
        ];
        for (i, (mutate, ok, why)) in cases.iter().enumerate() {
            let mut c = tests_base_claim();
            mutate(&mut c);
            let r = validate_claim(7, &c, false);
            assert_eq!(r.is_ok(), *ok, "case {i}: {r:?}");
            if !ok {
                match r.unwrap_err() {
                    ClogError::InvalidClaim { index, reason } => {
                        assert_eq!(index, 7);
                        assert!(reason.to_lowercase().contains(why), "case {i}: {reason} !~ {why}");
                    }
                    ClogError::ReservedNamespace => assert_eq!(*why, "reserved"),
                    e => panic!("case {i}: wrong error {e:?}"),
                }
            }
        }
        // reserved allowed when internal
        let mut c = tests_base_claim();
        c.claim_key = "clog:merge:a->b".into();
        assert!(validate_claim(0, &c, true).is_ok());
    }

    #[test]
    fn u_val_1_focus_rules() {
        let tax = KindTaxonomy::default_taxonomy();
        assert!(validate_focus(&Focus::uniform().weight("risk", 2.0), &tax).is_ok());
        assert!(matches!(validate_focus(&Focus::uniform().weight("nope", 1.0), &tax), Err(ClogError::UnknownKind)));
        assert!(validate_focus(&Focus::uniform().weight("risk", f32::NAN), &tax).is_err());
        assert!(validate_focus(&Focus::uniform().weight("risk", 0.0), &tax).is_err());
        assert!(validate_focus(&Focus::uniform().half_life_days(0.005), &tax).is_err());
        assert!(validate_focus(&Focus::uniform().half_life_days(4000.0), &tax).is_err());
        // half-life bounds are exclusive: exactly the endpoints must fail.
        assert!(validate_focus(&Focus::uniform().half_life_days(0.01), &tax).is_err());
        assert!(validate_focus(&Focus::uniform().half_life_days(3650.0), &tax).is_err());
        // boost factors: valid, NaN, zero.
        let e = EntityRef { etype: "p".into(), id: "x".into(), name: None };
        assert!(validate_focus(&Focus::uniform().boost(e.clone(), 1.5), &tax).is_ok());
        assert!(validate_focus(&Focus::uniform().boost(e.clone(), f32::NAN), &tax).is_err());
        assert!(validate_focus(&Focus::uniform().boost(e, 0.0), &tax).is_err());
    }

    #[test]
    fn scoring_clamp_only_beyond_24h() {
        assert_eq!(scoring_clamp(100, 1_000_000), 100);
        assert_eq!(scoring_clamp(1_000_000 + 86_400_000, 1_000_000), 1_000_000 + 86_400_000);
        assert_eq!(scoring_clamp(1_000_000 + 86_400_001, 1_000_000), 1_000_000);
    }
}
