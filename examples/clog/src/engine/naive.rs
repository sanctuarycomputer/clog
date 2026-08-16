//! The naive engine: the semantic oracle (spec §5).
//!
//! This engine is deliberately the simplest thing that is *correct*. Its
//! behaviour **is** the specification of what every later, faster engine must
//! reproduce, so every recompute below is a small named function that can be
//! audited by eye (spec §6.2). Where a choice exists between "incremental and
//! clever" and "recompute from the live claims", this file always picks the
//! latter: the alias map, `by_entity`'s alias re-keying, the name registry
//! and `urgent` are all rebuilt wholesale from the live claims, so every view
//! is a pure function of `claims` and nothing can drift. Only [`imbl`]
//! ordered structures are used, so every view is deterministic (INV-11).
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

use crate::alias::{AliasMap, EntityKey};
use crate::belief::{self, BeliefInput};
use crate::engine::{ApplyResult, Engine, Event, StoredClaim, WorldViews};
use crate::score::score_claim;
use crate::types::{Claim, Credibility, Focus, JudgeSource, KindLabel, ObserverId};

/// The namespace reserved for clog's own claims (INV-8).
const RESERVED_PREFIX: &str = "clog:";
/// The key prefix identifying a merge (entity alias) claim.
const MERGE_PREFIX: &str = "clog:merge:";
/// The field separator inside a merge claim's body (ASCII unit separator).
const MERGE_SEP: char = '\u{1f}';
/// How many recent believed claims `entity_state` reports per entity
/// (spec §5.3's internal constant N).
const ENTITY_STATE_ROWS: usize = 8;

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

impl NaiveEngine {
    /// An engine over an empty world.
    // Not yet called from production code: the actor (a later task)
    // constructs one. Exercised by this module's tests in the meantime;
    // every other method here is reachable through the `Engine` impl.
    #[allow(dead_code)]
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
        if merge_edge(&sc.claim).is_some() {
            self.rebuild_aliases();
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
    /// Judging a reserved claim is likewise a no-op: `clog:*` keys stay out
    /// of every view, `kinds` included (INV-8).
    fn judge(&mut self, claim_key: &str, kind: &str, confidence: f32, source: JudgeSource) -> bool {
        if !self.views.claims.contains_key(claim_key) || is_reserved(claim_key) {
            return false;
        }
        self.views.kinds.insert(
            claim_key.to_string(),
            KindLabel { kind: kind.to_string(), confidence, source },
        );
        self.recompute_membership(claim_key);
        true
    }

    /// Expands a `Revoke` into a retraction of every live claim of
    /// `observer`, **including** its reserved `clog:*` claims (INV-6; §5.1).
    /// `by_observer`'s sets are ordered, so the retractions run in
    /// `claim_key` order and the resulting views are replay-deterministic
    /// (INV-11). Returns whether any live claim was removed.
    fn revoke(&mut self, observer: &ObserverId) -> bool {
        let Some(keys) = self.views.by_observer.get(&observer.0) else { return false };
        let keys: Vec<String> = keys.iter().cloned().collect();
        let mut touched = false;
        for key in keys {
            touched |= self.remove_live(&key);
        }
        touched
    }

    /// The shared removal path for retraction, revocation and re-observation:
    /// unindexes the claim, drops its kind and view memberships, un-merges it
    /// (by rebuilding the alias map and re-keying `by_entity`) if it was a
    /// merge claim (U-ALIAS-3), and heals the derived registries. Returns
    /// whether a live claim was removed.
    fn remove_live(&mut self, claim_key: &str) -> bool {
        let Some(old) = self.views.claims.remove(claim_key) else { return false };
        self.unindex_claim(&old.claim);
        self.views.kinds.remove(claim_key);
        self.views.unclassified.remove(claim_key);
        self.views.open_loops.remove(claim_key);
        if merge_edge(&old.claim).is_some() {
            self.rebuild_aliases();
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

    /// Rebuilds the alias map from the live merge claims, so `aliases` is a
    /// pure *view* of `claims` (§5.2: "the canonical map is a view") rather
    /// than an incrementally patched cache. Called on every merge-claim
    /// insert and removal.
    ///
    /// Patching (insert the edge on observe, drop it on retract) is wrong in
    /// two ways that this rebuild fixes:
    ///
    /// 1. depth-1 flattening is lossy: after `a -> b` then `b -> c` the
    ///    stored edge for `a` points at `c`, so dropping `b -> c` would
    ///    leave `a -> c` even though the only surviving claim says `a -> b`;
    /// 2. two live merge claims may name the same alias (`a -> b`,
    ///    `a -> c`): dropping one would drop the whole edge instead of
    ///    falling back to the other claim's.
    ///
    /// Claims are visited in `claim_key` order and edges are re-inserted
    /// through [`AliasMap::insert`], so flattening still applies and the
    /// result is a deterministic function of the live claims (INV-11).
    /// Insert errors are ignored: the only error is `AliasCycle`, which the
    /// write path rejects at `merge_entities` (U-ALIAS-2) and the engine has
    /// no channel to report — a cycling edge simply loses to the lower
    /// `claim_key` that already claimed the far end.
    fn rebuild_aliases(&mut self) {
        let mut aliases = AliasMap::default();
        for sc in self.views.claims.values() {
            if let Some((alias, canonical)) = merge_edge(&sc.claim) {
                let _ = aliases.insert(alias, canonical);
            }
        }
        self.views.aliases = aliases;
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

    /// A copy of `focus` whose boost entities are rewritten to their
    /// canonical keys, leaving weights, half-life and `top_k` untouched.
    fn resolve_focus(&self, focus: &Focus) -> Focus {
        let mut resolved = focus.clone();
        for (entity, _) in resolved.boosts.iter_mut() {
            let (etype, id) = self.views.aliases.resolve(&entity.key());
            entity.etype = etype;
            entity.id = id;
        }
        resolved
    }

    /// Rescores every live, non-reserved claim for every scope (§5.4) and
    /// replaces `urgent` wholesale, so scopes that disappeared from `scopes`
    /// leave no stale rows. Rows are sorted score-desc, ties broken by
    /// `claim_key`-asc, then truncated to the scope's `top_k`.
    fn recompute_urgent(&mut self, scopes: &BTreeMap<String, Focus>, now_ms: u64) {
        let mut urgent: OrdMap<String, Vec<(f32, String)>> = OrdMap::new();
        for (scope, focus) in scopes {
            // A focus names entities the way the *host* knows them, and
            // `score_claim` matches boosts against canonical keys, so the
            // boosts are resolved through the alias map first: a boost on an
            // entity that has since been merged away must still boost the
            // claims that mention it (§5.2 — "every view that groups or
            // filters by entity resolves through this map"). Boosts that
            // collapse onto the same canonical entity stack multiplicatively,
            // exactly as two distinct matching boosts already do (§5.4).
            let focus = &self.resolve_focus(focus);
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

/// One [`entity_state`] row: a canonical entity, its display name, and the
/// believed claims of the subjects touching it, each paired with its
/// `subject_key`.
pub(crate) type EntityStateRow = (EntityKey, String, Vec<(String, StoredClaim)>);

/// The `entity_state` view (§5.3, §5.7): one row per canonical entity, in
/// entity-key order, carrying the entity's display name and the believed
/// claims of every subject that touches it.
///
/// The display name is the registry's latest-seen name for the canonical
/// entity, falling back to `"{etype}:{id}"` when no claim ever named it.
///
/// A subject "touches" the entity when *its believed claim* mentions the
/// entity (post-alias) — so `by_entity`, which is already keyed canonically,
/// supplies the candidates and `believed` filters them. Losing claims and
/// subject-less claims produce no rows, and reserved claims are excluded
/// throughout (INV-8; `believed` already skips them).
///
/// Rows are newest-first by `occurred_at`, ties broken by `claim_key` ascending
/// (the same tiebreak `urgent` uses), then capped at [`ENTITY_STATE_ROWS`].
// Not yet called from production code: the renderer's `entities` slot (a
// later task) consumes this. Exercised by this module's tests meanwhile.
#[allow(dead_code)]
pub(crate) fn entity_state(views: &WorldViews) -> Vec<EntityStateRow> {
    let mut out = Vec::new();
    for (entity, keys) in views.by_entity.iter() {
        let display = match views.names.get(entity) {
            Some((_, name)) => name.clone(),
            None => format!("{}:{}", entity.0, entity.1),
        };
        let mut rows: Vec<(String, StoredClaim)> = keys
            .iter()
            .filter(|key| !is_reserved(key))
            .filter_map(|key| Some((key, views.claims.get(key)?)))
            .filter_map(|(key, sc)| {
                let subject = sc.claim.subject_key.clone()?;
                // only the subject's *winner* earns a row
                match views.believed.get(&subject) {
                    Some(Some(winner)) if winner == key => Some((subject, sc.clone())),
                    _ => None,
                }
            })
            .collect();
        rows.sort_by(|a, b| {
            b.1.claim.occurred_at.cmp(&a.1.claim.occurred_at).then_with(|| a.1.claim.claim_key.cmp(&b.1.claim.claim_key))
        });
        rows.truncate(ENTITY_STATE_ROWS);
        out.push((entity.clone(), display, rows));
    }
    out
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
                Event::Revoke { observer } => self.revoke(observer),
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

        // INV-8 covers `kinds` too: a reserved claim cannot be judged into a view.
        e.apply(&[Event::Judge { claim_key: "clog:merge:p:a->p:b".into(), kind: "risk".into(), confidence: 1.0, source: JudgeSource::Rule }], &scopes(), 101);
        assert!(e.views().kinds.is_empty());
        assert!(e.views().open_loops.is_empty());
    }

    /// A merge claim carrying the edge `{alias} -> {canonical}`.
    fn merge(alias: (&str, &str), canonical: (&str, &str)) -> Event {
        let mut m = tests_base_claim();
        m.claim_key = format!("clog:merge:{}:{}->{}:{}", alias.0, alias.1, canonical.0, canonical.1);
        m.observer = ObserverId::from("clog");
        m.body = [alias.0, alias.1, canonical.0, canonical.1].join("\u{1f}");
        Event::Observe(StoredClaim { claim: m, recorded_at: 1 })
    }

    #[test]
    fn revoke_retracts_all_of_observer_inv6() {
        let mut e = NaiveEngine::new(cfg());
        let mut c1 = tests_base_claim(); c1.claim_key = "a".into(); c1.observer = ObserverId::from("gmail");
        let mut c2 = tests_base_claim(); c2.claim_key = "b".into(); c2.observer = ObserverId::from("gmail");
        let mut c3 = tests_base_claim(); c3.claim_key = "c".into(); c3.observer = ObserverId::from("twist");
        e.apply(&[Event::Observe(StoredClaim { claim: c1, recorded_at: 1 }),
                  Event::Observe(StoredClaim { claim: c2, recorded_at: 1 }),
                  Event::Observe(StoredClaim { claim: c3, recorded_at: 1 })], &scopes(), 100);
        e.apply(&[Event::Revoke { observer: ObserverId::from("gmail") }], &scopes(), 101);
        assert!(!e.views().claims.contains_key("a"));
        assert!(!e.views().claims.contains_key("b"));
        assert!(e.views().claims.contains_key("c"));
        assert!(e.views().by_observer.get("gmail").is_none());
    }

    // Beyond the brief: INV-6 says revoke takes the observer's *reserved*
    // claims with it too, and a revoke of an unknown observer is inert.
    #[test]
    fn revoke_takes_reserved_claims_inv6() {
        let mut e = NaiveEngine::new(cfg());
        let mut c = tests_base_claim();
        c.claim_key = "about-a".into();
        c.observer = ObserverId::from("clog");
        c.entities = vec![EntityRef { etype: "p".into(), id: "a".into(), name: None }];
        e.apply(&[Event::Observe(StoredClaim { claim: c, recorded_at: 1 }), merge(("p", "a"), ("p", "b"))], &scopes(), 100);
        assert_eq!(e.views().aliases.resolve(&("p".into(), "a".into())), ("p".into(), "b".into()));

        assert!(!e.apply(&[Event::Revoke { observer: ObserverId::from("nobody") }], &scopes(), 101).touched);
        e.apply(&[Event::Revoke { observer: ObserverId::from("clog") }], &scopes(), 102);
        assert!(e.views().claims.is_empty());
        assert!(e.views().by_observer.is_empty());
        assert!(e.views().by_entity.is_empty());
        // the merge claim went with it, so the alias edge did too (INV-3)
        assert_eq!(e.views().aliases.resolve(&("p".into(), "a".into())), ("p".into(), "a".into()));
    }

    #[test]
    fn u_alias_3_merge_retraction_rekeys_views() {
        let mut e = NaiveEngine::new(cfg());
        let mut c = tests_base_claim();
        c.claim_key = "about-a".into();
        c.entities = vec![EntityRef { etype: "p".into(), id: "a".into(), name: Some("Aye".into()) }];
        e.apply(&[Event::Observe(StoredClaim { claim: c, recorded_at: 1 })], &scopes(), 100);

        let mut m = tests_base_claim();
        m.claim_key = "clog:merge:p:a->p:b".into();
        m.observer = ObserverId::from("clog");
        m.body = ["p", "a", "p", "b"].join("\u{1f}");
        e.apply(&[Event::Observe(StoredClaim { claim: m, recorded_at: 2 })], &scopes(), 101);
        // grouped under canonical b now
        assert!(e.views().by_entity.get(&("p".into(), "b".into())).unwrap().contains("about-a"));
        assert!(e.views().by_entity.get(&("p".into(), "a".into())).is_none());

        e.apply(&[Event::Retract { claim_key: "clog:merge:p:a->p:b".into() }], &scopes(), 102);
        // un-merged: re-keyed back under a, name registry intact
        assert!(e.views().by_entity.get(&("p".into(), "a".into())).unwrap().contains("about-a"));
        let es = entity_state(e.views());
        let (_, display, rows) = es.iter().find(|(k, _, _)| k == &("p".to_string(), "a".to_string())).unwrap();
        assert_eq!(display, "Aye");
        assert_eq!(rows.len(), 0); // no subject_key -> no believed rows
    }

    // Ruling A, case 1: the alias map is a *view* of the live merge claims,
    // not an incrementally patched cache. After `a -> b` then `b -> c`
    // (which write-time-flattens `a` onto `c`), retracting `b -> c` must
    // restore `a -> b` — the edge its own live claim still asserts.
    #[test]
    fn u_alias_3_retracting_a_flattening_merge_restores_the_earlier_edge() {
        let mut e = NaiveEngine::new(cfg());
        e.apply(&[merge(("p", "a"), ("p", "b")), merge(("p", "b"), ("p", "c"))], &scopes(), 100);
        assert_eq!(e.views().aliases.resolve(&("p".into(), "a".into())), ("p".into(), "c".into()));

        e.apply(&[Event::Retract { claim_key: "clog:merge:p:b->p:c".into() }], &scopes(), 101);
        assert_eq!(e.views().aliases.resolve(&("p".into(), "a".into())), ("p".into(), "b".into()));
        assert_eq!(e.views().aliases.resolve(&("p".into(), "b".into())), ("p".into(), "b".into()));
    }

    // Ruling A, case 2: two live merge claims can name the same alias. The
    // last one applied owns the edge; retracting it must fall back to the
    // other live claim's edge, not leave `a` unaliased.
    #[test]
    fn u_alias_3_retracting_one_of_two_merges_leaves_the_others_edge() {
        let mut e = NaiveEngine::new(cfg());
        let mut c = tests_base_claim();
        c.claim_key = "about-a".into();
        c.entities = vec![EntityRef { etype: "p".into(), id: "a".into(), name: None }];
        e.apply(&[Event::Observe(StoredClaim { claim: c, recorded_at: 1 }),
                  merge(("p", "a"), ("p", "b")), merge(("p", "a"), ("p", "c"))], &scopes(), 100);
        assert_eq!(e.views().aliases.resolve(&("p".into(), "a".into())), ("p".into(), "c".into()));

        e.apply(&[Event::Retract { claim_key: "clog:merge:p:a->p:c".into() }], &scopes(), 101);
        assert_eq!(e.views().aliases.resolve(&("p".into(), "a".into())), ("p".into(), "b".into()));
        assert!(e.views().by_entity.get(&("p".into(), "b".into())).unwrap().contains("about-a"));
        assert!(e.views().by_entity.get(&("p".into(), "a".into())).is_none());
    }

    // Ruling B: focus boosts name entities the way the *host* knows them, so
    // a boost on an alias must survive that alias being merged away.
    #[test]
    fn focus_boosts_resolve_through_aliases() {
        let mut e = NaiveEngine::new(cfg());
        let mut c = tests_base_claim();
        c.claim_key = "about-a".into();
        c.entities = vec![EntityRef { etype: "p".into(), id: "a".into(), name: None }];
        let ent = |id: &str| EntityRef { etype: "p".into(), id: id.into(), name: None };
        let scopes = BTreeMap::from([
            ("plain".to_string(), Focus::uniform()),
            ("alias".to_string(), Focus::uniform().boost(ent("a"), 3.0)),
            ("canonical".to_string(), Focus::uniform().boost(ent("b"), 3.0)),
        ]);
        e.apply(&[Event::Observe(StoredClaim { claim: c, recorded_at: 1 }), merge(("p", "a"), ("p", "b"))], &scopes, 100);

        let score = |scope: &str| {
            e.views().urgent.get(scope).unwrap().iter().find(|(_, k)| k == "about-a").unwrap().0
        };
        let plain = score("plain");
        assert!(plain > 0.0);
        assert!((score("alias") - plain * 3.0).abs() < 1e-6, "{} vs {}", score("alias"), plain * 3.0);
        assert!((score("canonical") - plain * 3.0).abs() < 1e-6);
    }

    #[test]
    fn entity_state_newest_first_capped() {
        let mut e = NaiveEngine::new(cfg());
        let ent = EntityRef { etype: "proj".into(), id: "h".into(), name: None };
        let mut evs = vec![];
        for i in 0..10 {
            let mut c = tests_base_claim();
            c.claim_key = format!("c{i}");
            c.subject_key = Some(format!("s{i}"));
            c.occurred_at = 100 + i;
            c.entities = vec![ent.clone()];
            evs.push(Event::Observe(StoredClaim { claim: c, recorded_at: 1 }));
        }
        e.apply(&evs, &scopes(), 1000);
        let es = entity_state(e.views());
        let (_, display, rows) = &es[0];
        assert_eq!(display, "proj:h");
        assert_eq!(rows.len(), 8);
        assert_eq!(rows[0].0, "s9"); // newest occurred_at first
    }

    // Beyond the brief: entity_state only reports *believed* claims, and it
    // reports them under the canonical entity after a merge.
    #[test]
    fn entity_state_reports_believed_rows_under_the_canonical_entity() {
        let mut e = NaiveEngine::new(cfg());
        let ent = |id: &str| EntityRef { etype: "p".into(), id: id.into(), name: None };
        let mut old = tests_base_claim();
        old.claim_key = "old".into(); old.subject_key = Some("s1".into()); old.occurred_at = 100;
        old.entities = vec![ent("a")];
        let mut new = tests_base_claim();
        new.claim_key = "new".into(); new.subject_key = Some("s1".into()); new.occurred_at = 200;
        new.entities = vec![ent("a")];
        e.apply(&[Event::Observe(StoredClaim { claim: old, recorded_at: 1 }),
                  Event::Observe(StoredClaim { claim: new, recorded_at: 2 })], &scopes(), 1000);
        let es = entity_state(e.views());
        assert_eq!(es.len(), 1);
        assert_eq!(es[0].0, ("p".to_string(), "a".to_string()));
        // one row per subject: the believed claim, not the losing one
        assert_eq!(es[0].2.iter().map(|(s, sc)| (s.as_str(), sc.claim.claim_key.as_str())).collect::<Vec<_>>(),
                   vec![("s1", "new")]);

        e.apply(&[merge(("p", "a"), ("p", "b"))], &scopes(), 1001);
        let es = entity_state(e.views());
        assert_eq!(es.len(), 1);
        assert_eq!(es[0].0, ("p".to_string(), "b".to_string()));
        assert_eq!(es[0].1, "p:b");
        assert_eq!(es[0].2.len(), 1);
    }
}
