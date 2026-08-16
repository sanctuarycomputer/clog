//! The naive engine: the semantic oracle (spec §5).
//!
//! This engine is deliberately the simplest thing that is *correct*. Its
//! behaviour **is** the specification of what every later, faster engine must
//! reproduce, so every recompute below is a small named function that can be
//! audited by eye (spec §6.2). Where a choice exists between "incremental and
//! clever" and "recompute from the live claims", this file always picks the
//! latter: alias re-keying, the name registry, and `urgent` are all full
//! rebuilds. Only [`imbl`] ordered structures are used, so every view is
//! deterministic (INV-11).
//!
//! Reserved-namespace claims (`clog:*`) live in `views.claims` like any other
//! claim, but are filtered out of `unclassified`, `open_loops`, `believed`
//! and `urgent` (INV-8). They still *drive* state: a merge claim's body is
//! the alias edge.
//!
//! **Merge claim wire format.** A merge claim's `claim_key` starts with
//! `clog:merge:` and its `body` is exactly four fields joined with the ASCII
//! unit separator (`U+001F`):
//!
//! ```text
//! alias.etype ␟ alias.id ␟ canonical.etype ␟ canonical.id
//! ```
//!
//! Unit-separated rather than JSON so the engine needs no parser and no extra
//! dependency; §10 validation already rejects control characters in every
//! host-supplied field, so the separator cannot appear in an entity key.

use std::collections::BTreeMap;

use imbl::{OrdMap, OrdSet};

use crate::alias::EntityKey;
use crate::belief::{self, BeliefInput};
use crate::engine::{ApplyResult, Engine, Event, StoredClaim, WorldViews};
use crate::score::score_claim;
use crate::types::{Claim, Credibility, Focus, JudgeSource, KindLabel};

/// The namespace reserved for clog's own claims (INV-8).
const RESERVED_PREFIX: &str = "clog:";
/// The key prefix identifying a merge (entity alias) claim.
const MERGE_PREFIX: &str = "clog:merge:";
/// The field separator inside a merge claim's body (ASCII unit separator).
const MERGE_SEP: char = '\u{1f}';

/// Whether `claim_key` is in the reserved namespace (INV-8).
fn is_reserved(claim_key: &str) -> bool {
    claim_key.starts_with(RESERVED_PREFIX)
}

/// Parses the alias edge carried by a merge claim, or `None` if `claim` is
/// not a merge claim (or its body is malformed).
fn merge_edge(claim: &Claim) -> Option<(EntityKey, EntityKey)> {
    if !claim.claim_key.starts_with(MERGE_PREFIX) {
        return None;
    }
    let parts: Vec<&str> = claim.body.split(MERGE_SEP).collect();
    let [alias_etype, alias_id, canonical_etype, canonical_id] = parts[..] else {
        return None;
    };
    if [alias_etype, alias_id, canonical_etype, canonical_id].iter().any(|f| f.is_empty()) {
        return None;
    }
    Some((
        (alias_etype.to_string(), alias_id.to_string()),
        (canonical_etype.to_string(), canonical_id.to_string()),
    ))
}

/// Adds `claim_key` to the set indexed under `k`.
fn index_add<K: Ord + Clone>(idx: &mut OrdMap<K, OrdSet<String>>, k: K, claim_key: &str) {
    let mut set = idx.get(&k).cloned().unwrap_or_default();
    set.insert(claim_key.to_string());
    idx.insert(k, set);
}

/// Removes `claim_key` from the set indexed under `k`, dropping the entry
/// entirely once it is empty (so retraction leaves no husks; INV-3).
fn index_remove<K: Ord + Clone>(idx: &mut OrdMap<K, OrdSet<String>>, k: &K, claim_key: &str) {
    let Some(set) = idx.get_mut(k) else { return };
    set.remove(claim_key);
    if set.is_empty() {
        idx.remove(k);
    }
}

/// The knobs the naive engine needs from `Config`, extracted so the engine
/// stays independent of the (larger, host-facing) `Config` type.
// Not yet constructed by production code: the actor (a later task) builds
// this from `Config`. Exercised directly by this module's tests meanwhile.
#[allow(dead_code)]
pub(crate) struct NaiveCfg {
    /// Kinds treated as open loops.
    pub loop_kinds: Vec<String>,
    /// Default `urgent` cap, used when a scope's focus sets no `top_k`.
    pub top_k: usize,
    /// Decay buckets per half-life, passed through to scoring.
    pub buckets_per_half_life: u32,
    /// The minimum credibility a claim needs to win belief.
    pub belief_floor: Credibility,
}

/// The reference engine: applies events to [`WorldViews`] the obvious way.
// Not yet constructed by production code: the actor (a later task) owns one.
// Exercised directly by this module's tests in the meantime.
#[allow(dead_code)]
pub(crate) struct NaiveEngine {
    views: WorldViews,
    cfg: NaiveCfg,
}

#[allow(dead_code)]
impl NaiveEngine {
    /// An engine over an empty world.
    pub(crate) fn new(cfg: NaiveCfg) -> Self {
        NaiveEngine { views: WorldViews::default(), cfg }
    }

    // ---- events -------------------------------------------------------

    /// Records a claim. Re-observing a live `claim_key` is a retraction of
    /// the old version followed by an assertion of the new one (INV-4), so
    /// the old version's indexes, kind and alias edge are dropped first.
    fn observe(&mut self, sc: &StoredClaim) -> bool {
        let key = sc.claim.claim_key.clone();
        let previous_subject = self.views.claims.get(&key).and_then(|old| old.claim.subject_key.clone());
        self.remove_live(&key);

        self.views.claims.insert(key.clone(), sc.clone());
        self.reindex_claim(&sc.claim);
        if let Some((alias, canonical)) = merge_edge(&sc.claim) {
            // A cycle can only reach the engine if the write path failed to
            // reject it (U-ALIAS-2 rejects at `merge_entities`); the engine
            // has no error channel, so a cycle simply writes no edge.
            let _ = self.views.aliases.insert(alias, canonical);
            self.rekey_by_entity();
        }
        self.rebuild_names();
        // Both groups, because a replacement may move the claim between
        // subjects: the old group's winner may change, and so may the new
        // one's. Recomputing a group twice is idempotent.
        if let Some(subject) = previous_subject {
            self.recompute_belief(&subject);
        }
        if let Some(subject) = sc.claim.subject_key.clone() {
            self.recompute_belief(&subject);
        }
        self.recompute_membership(&key);
        true
    }

    /// Drops a claim and everything derived from it. Returns whether a live
    /// claim was actually removed.
    fn retract(&mut self, claim_key: &str) -> bool {
        self.remove_live(claim_key)
    }

    /// Upserts a kind classification and moves the claim between the
    /// `unclassified` and `open_loops` views. Judging a claim that is not
    /// live is a no-op: a dead claim must leave no trace behind (INV-3).
    fn judge(&mut self, claim_key: &str, kind: &str, confidence: f32, source: JudgeSource) -> bool {
        if !self.views.claims.contains_key(claim_key) {
            return false;
        }
        self.views.kinds.insert(
            claim_key.to_string(),
            KindLabel { kind: kind.to_string(), confidence, source },
        );
        self.recompute_membership(claim_key);
        true
    }

    /// The shared removal path for retraction and re-observation: unindexes
    /// the claim, drops its kind and view memberships, un-merges its alias
    /// edge if it was a merge claim (U-ALIAS-3), and heals the derived
    /// registries. Returns whether a live claim was removed.
    ///
    /// Known limitation, inherited from depth-1 flattening (§5.2): removing
    /// one merge edge does not un-flatten edges that edge re-pointed. After
    /// `a -> b` then `b -> c` (which re-points `a` at `c`), retracting
    /// `b -> c` leaves `a -> c`, not `a -> b`. Full retraction of every
    /// merge still empties the map, so INV-3's retract-all case holds.
    fn remove_live(&mut self, claim_key: &str) -> bool {
        let Some(old) = self.views.claims.remove(claim_key) else { return false };
        self.unindex_claim(&old.claim);
        self.views.kinds.remove(claim_key);
        self.views.unclassified.remove(claim_key);
        self.views.open_loops.remove(claim_key);
        if let Some((alias, _)) = merge_edge(&old.claim) {
            self.views.aliases.remove(&alias);
            self.rekey_by_entity();
        }
        self.rebuild_names();
        if let Some(subject) = old.claim.subject_key.clone() {
            self.recompute_belief(&subject);
        }
        true
    }

    // ---- indexes ------------------------------------------------------

    /// Adds a claim to `by_subject`/`by_observer`/`by_entity`. Entity keys
    /// are indexed post-alias, so `by_entity` is always keyed by canonical
    /// entity.
    fn reindex_claim(&mut self, claim: &Claim) {
        if let Some(subject) = &claim.subject_key {
            index_add(&mut self.views.by_subject, subject.clone(), &claim.claim_key);
        }
        index_add(&mut self.views.by_observer, claim.observer.0.clone(), &claim.claim_key);
        for e in &claim.entities {
            let canonical = self.views.aliases.resolve(&e.key());
            index_add(&mut self.views.by_entity, canonical, &claim.claim_key);
        }
    }

    /// The exact inverse of [`Self::reindex_claim`], using the alias map as
    /// it stands *now* (callers that also change the alias map re-key
    /// `by_entity` afterwards, so a stale resolution cannot survive).
    fn unindex_claim(&mut self, claim: &Claim) {
        if let Some(subject) = &claim.subject_key {
            index_remove(&mut self.views.by_subject, subject, &claim.claim_key);
        }
        index_remove(&mut self.views.by_observer, &claim.observer.0, &claim.claim_key);
        for e in &claim.entities {
            let canonical = self.views.aliases.resolve(&e.key());
            index_remove(&mut self.views.by_entity, &canonical, &claim.claim_key);
        }
    }

    /// Rebuilds `by_entity` from the live claims under the current alias
    /// map. Called whenever an alias edge is inserted or removed: that is
    /// the expensive retraction the spec calls out in §5.2, and rebuilding
    /// wholesale is the auditable way to guarantee every group is re-keyed.
    fn rekey_by_entity(&mut self) {
        let mut by_entity: OrdMap<EntityKey, OrdSet<String>> = OrdMap::new();
        for (key, sc) in self.views.claims.iter() {
            for e in &sc.claim.entities {
                index_add(&mut by_entity, self.views.aliases.resolve(&e.key()), key);
            }
        }
        self.views.by_entity = by_entity;
    }

    /// Rebuilds the entity display-name registry from the live claims: the
    /// latest `recorded_at` carrying a non-null name wins per canonical
    /// entity (§5.2), ties going to the larger `claim_key` since claims are
    /// visited in key order. Rebuilt rather than patched so a retraction
    /// takes its names with it (INV-3).
    fn rebuild_names(&mut self) {
        let mut names: OrdMap<EntityKey, (u64, String)> = OrdMap::new();
        for sc in self.views.claims.values() {
            for e in &sc.claim.entities {
                let Some(name) = &e.name else { continue };
                let canonical = self.views.aliases.resolve(&e.key());
                let newer = names.get(&canonical).is_none_or(|(at, _)| sc.recorded_at >= *at);
                if newer {
                    names.insert(canonical, (sc.recorded_at, name.clone()));
                }
            }
        }
        self.views.names = names;
    }

    // ---- derived views ------------------------------------------------

    /// Recomputes which claim is believed for one subject group (§5.3).
    /// `None` means an all-floored group with two or more members: nobody is
    /// believed. An empty (or entirely reserved) group drops the entry.
    fn recompute_belief(&mut self, subject: &str) {
        let members: Vec<&StoredClaim> = match self.views.by_subject.get(subject) {
            Some(keys) => keys
                .iter()
                .filter(|k| !is_reserved(k))
                .filter_map(|k| self.views.claims.get(k))
                .collect(),
            None => Vec::new(),
        };
        if members.is_empty() {
            self.views.believed.remove(subject);
            return;
        }
        let group: Vec<BeliefInput> = members
            .iter()
            .map(|sc| BeliefInput { claim: &sc.claim, recorded_at: sc.recorded_at })
            .collect();
        let winner = belief::resolve(&group, self.cfg.belief_floor).map(|c| c.claim_key.clone());
        self.views.believed.insert(subject.to_string(), winner);
    }

    /// Recomputes one claim's membership of `unclassified` and `open_loops`.
    /// Both views hold only live, non-reserved claims (INV-8); `unclassified`
    /// means "no kind yet" in P1, `open_loops` means "kind is a loop kind".
    fn recompute_membership(&mut self, claim_key: &str) {
        let visible = self.views.claims.contains_key(claim_key) && !is_reserved(claim_key);
        let kind = self.views.kinds.get(claim_key).map(|l| l.kind.clone());

        if visible && kind.is_none() {
            self.views.unclassified.insert(claim_key.to_string());
        } else {
            self.views.unclassified.remove(claim_key);
        }

        let open = visible && kind.is_some_and(|k| self.cfg.loop_kinds.contains(&k));
        if open {
            self.views.open_loops.insert(claim_key.to_string());
        } else {
            self.views.open_loops.remove(claim_key);
        }
    }

    /// Rescores every live, non-reserved claim for every scope (§5.4) and
    /// replaces `urgent` wholesale, so scopes that disappeared from `scopes`
    /// leave no stale rows. Rows are sorted score-desc, ties broken by
    /// `claim_key`-asc, then truncated to the scope's `top_k`.
    fn recompute_urgent(&mut self, scopes: &BTreeMap<String, Focus>, now_ms: u64) {
        let mut urgent: OrdMap<String, Vec<(f32, String)>> = OrdMap::new();
        for (scope, focus) in scopes {
            let mut rows: Vec<(f32, String)> = self
                .views
                .claims
                .iter()
                .filter(|(key, _)| !is_reserved(key))
                .map(|(key, sc)| {
                    let entities: Vec<EntityKey> =
                        sc.claim.entities.iter().map(|e| self.views.aliases.resolve(&e.key())).collect();
                    let kind = self.views.kinds.get(key).map(|l| l.kind.as_str());
                    let score =
                        score_claim(&sc.claim, kind, focus, &entities, now_ms, self.cfg.buckets_per_half_life);
                    (score, key.clone())
                })
                .collect();
            // `total_cmp` rather than `partial_cmp`: a NaN score must still
            // sort deterministically instead of panicking or silently
            // reordering (INV-11).
            rows.sort_by(|a, b| b.0.total_cmp(&a.0).then_with(|| a.1.cmp(&b.1)));
            rows.truncate(focus.top_k.unwrap_or(self.cfg.top_k));
            urgent.insert(scope.clone(), rows);
        }
        self.views.urgent = urgent;
    }
}

impl Engine for NaiveEngine {
    fn apply(&mut self, events: &[Event], scopes: &BTreeMap<String, Focus>, now_ms: u64) -> ApplyResult {
        let mut touched = false;
        for event in events {
            let effect = match event {
                Event::Observe(sc) => self.observe(sc),
                Event::Retract { claim_key } => self.retract(claim_key),
                Event::Judge { claim_key, kind, confidence, source } => {
                    self.judge(claim_key, kind, *confidence, *source)
                }
                // Revoke lands in the next task; until then it is inert
                // rather than silently half-applied.
                Event::Revoke { .. } => false,
                // The actor owns `scopes` and the clock, and only forwards a
                // `Tick` when a claim actually crossed a decay bucket (§5.5),
                // so both reach the engine as a rescore request.
                Event::SetFocus { .. } | Event::Tick { .. } => true,
            };
            touched |= effect;
        }
        self.recompute_urgent(scopes, now_ms);
        ApplyResult { touched }
    }

    fn views(&self) -> &WorldViews {
        &self.views
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine::{Engine, Event, StoredClaim};
    use crate::types::*;
    use crate::validate::tests_base_claim;
    use std::collections::BTreeMap;

    fn cfg() -> NaiveCfg {
        NaiveCfg { loop_kinds: vec!["question".into(), "risk".into(), "commitment".into()],
                   top_k: 12, buckets_per_half_life: 4, belief_floor: Credibility::Six }
    }
    fn scopes() -> BTreeMap<String, Focus> {
        BTreeMap::from([("default".to_string(), Focus::uniform())])
    }
    fn obs(key: &str, subject: Option<&str>, body: &str, occ: u64, rec: u64) -> Event {
        let mut c = tests_base_claim();
        c.claim_key = key.into();
        c.subject_key = subject.map(Into::into);
        c.body = body.into();
        c.occurred_at = occ; c.observed_at = occ;
        Event::Observe(StoredClaim { claim: c, recorded_at: rec })
    }

    #[test]
    fn observe_retract_round_trip_inv3() {
        let mut e = NaiveEngine::new(cfg());
        let empty = e.views().clone();
        e.apply(&[obs("a", Some("s1"), "hello", 10, 10)], &scopes(), 1000);
        assert!(e.views().claims.contains_key("a"));
        assert_eq!(e.views().believed.get("s1"), Some(&Some("a".to_string())));
        e.apply(&[Event::Retract { claim_key: "a".into() }], &scopes(), 1001);
        // INV-3: identical to never having observed (view contents, not revs)
        assert!(e.views().claims.is_empty());
        assert!(e.views().believed.is_empty());
        assert!(e.views().by_subject.is_empty());
        assert_eq!(e.views().urgent.get("default").map(Vec::len), empty.urgent.get("default").map(Vec::len).or(Some(0)));
    }

    // Beyond the brief's five: INV-3 says retraction heals *everything*, and
    // the round-trip test above only inspects claims/believed/by_subject.
    #[test]
    fn retraction_heals_every_index_inv3() {
        let mut e = NaiveEngine::new(cfg());
        let mut c = tests_base_claim();
        c.claim_key = "a".into();
        c.subject_key = Some("s1".into());
        c.observer = ObserverId::from("gmail");
        c.entities = vec![EntityRef { etype: "p".into(), id: "sam".into(), name: Some("Sam".into()) }];
        e.apply(&[Event::Observe(StoredClaim { claim: c, recorded_at: 1 })], &scopes(), 100);
        e.apply(&[Event::Judge { claim_key: "a".into(), kind: "risk".into(), confidence: 1.0, source: JudgeSource::Rule }], &scopes(), 101);
        assert_eq!(e.views().names.get(&("p".to_string(), "sam".to_string())), Some(&(1, "Sam".to_string())));
        assert!(e.views().open_loops.contains("a"));

        e.apply(&[Event::Retract { claim_key: "a".into() }], &scopes(), 102);
        assert!(e.views().by_entity.is_empty());
        assert!(e.views().by_observer.is_empty());
        assert!(e.views().names.is_empty());
        assert!(e.views().kinds.is_empty());
        assert!(e.views().open_loops.is_empty());
        assert!(e.views().unclassified.is_empty());
    }

    #[test]
    fn belief_competition_and_flags() {
        let mut e = NaiveEngine::new(cfg());
        e.apply(&[obs("old", Some("s1"), "invoice overdue", 100, 1),
                  obs("new", Some("s1"), "invoice paid", 200, 2)], &scopes(), 1000);
        assert_eq!(e.views().believed.get("s1"), Some(&Some("new".to_string())));
    }

    #[test]
    fn judge_moves_between_views() {
        let mut e = NaiveEngine::new(cfg());
        e.apply(&[obs("a", None, "x", 10, 10)], &scopes(), 1000);
        assert!(e.views().unclassified.contains("a"));
        assert!(!e.views().open_loops.contains("a"));
        e.apply(&[Event::Judge { claim_key: "a".into(), kind: "risk".into(), confidence: 1.0, source: JudgeSource::Rule }], &scopes(), 1001);
        assert!(!e.views().unclassified.contains("a"));
        assert!(e.views().open_loops.contains("a"));
        assert_eq!(e.views().kinds.get("a").unwrap().kind, "risk");
    }

    #[test]
    fn urgent_ranked_desc_tiebreak_key() {
        let mut e = NaiveEngine::new(cfg());
        // same trust/recency -> equal scores -> claim_key asc breaks tie
        e.apply(&[obs("b", None, "x", 100, 1), obs("a", None, "y", 100, 1)], &scopes(), 200);
        let u = e.views().urgent.get("default").unwrap();
        assert_eq!(u.iter().map(|(_, k)| k.as_str()).collect::<Vec<_>>(), vec!["a", "b"]);
    }

    #[test]
    fn reserved_claims_invisible_in_urgent_and_loops() {
        let mut e = NaiveEngine::new(cfg());
        let mut c = tests_base_claim();
        c.claim_key = "clog:merge:p:a->p:b".into();
        c.body = ["p", "a", "p", "b"].join("\u{1f}");
        e.apply(&[Event::Observe(StoredClaim { claim: c, recorded_at: 1 })], &scopes(), 100);
        assert!(e.views().urgent.get("default").unwrap().is_empty());
        assert!(e.views().unclassified.is_empty());
        // but the alias took effect
        assert_eq!(e.views().aliases.resolve(&("p".into(), "a".into())), ("p".into(), "b".into()));
    }
}
