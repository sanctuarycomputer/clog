//! Engine contract: the WAL wire format (`Event`/`Batch`), the in-memory
//! materialized-view snapshot (`WorldViews`), and the `Engine` trait that
//! the naive engine (next task) and the WAL both build on (spec §5).
//!
//! `Event`/`Batch`/`StoredClaim` derive `Serialize`/`Deserialize`: these
//! *are* the WAL's on-disk wire format (postcard), so their shapes must stay
//! stable. `WorldViews` is not serialized in P1 (`EngineDump` is deferred to
//! a later phase) and derives `Clone` only.

use std::collections::BTreeMap;

use imbl::{OrdMap, OrdSet};
use serde::{Deserialize, Serialize};

use crate::alias::{AliasMap, EntityKey};
use crate::types::{Claim, Focus, JudgeSource, KindLabel, ObserverId, Rev};

pub(crate) mod naive;

/// A claim as stored by the engine, alongside when clog recorded it.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub(crate) struct StoredClaim {
    /// The observed claim.
    pub claim: Claim,
    /// When clog recorded this claim, in epoch millis.
    pub recorded_at: u64,
}

/// A single WAL-durable state transition. This is the write-ahead log's wire
/// format: every variant must round-trip losslessly through postcard.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub(crate) enum Event {
    /// A claim was observed and recorded.
    Observe(StoredClaim),
    /// A claim was retracted by its `claim_key`.
    Retract {
        /// The retracted claim's key.
        claim_key: String,
    },
    /// All of an observer's claims were revoked.
    Revoke {
        /// The revoked observer.
        observer: ObserverId,
    },
    /// A scope's `Focus` was set or replaced.
    SetFocus {
        /// The scope whose focus changed.
        scope: String,
        /// The new focus.
        focus: Focus,
    },
    /// A kind judgment was recorded against a claim.
    Judge {
        /// The judged claim's key.
        claim_key: String,
        /// The asserted kind.
        kind: String,
        /// Confidence in `[0, 1]`.
        confidence: f32,
        /// The judge that produced this label.
        source: JudgeSource,
    },
    /// The internal clock advanced to `epoch`.
    Tick {
        /// The new epoch.
        epoch: u64,
    },
}

/// A batch of events committed together at a single revision.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub(crate) struct Batch {
    /// The revision this batch was committed at.
    pub rev: Rev,
    /// The events applied in this batch, in order.
    pub events: Vec<Event>,
}

/// The engine's in-memory materialized-view snapshot.
///
/// `claims` holds **all** live claims, including reserved `clog:*` ones;
/// every view accessor and `select` filters reserved keys out (INV-8).
/// `believed` maps `subject_key` to the winning `claim_key` (`None` means
/// an all-floored group: every candidate fell below the belief threshold).
/// `urgent` vectors are sorted score-desc, tie `claim_key`-asc, truncated to
/// the scope's `top_k`.
#[derive(Clone, Default)]
pub(crate) struct WorldViews {
    /// All live claims, keyed by `claim_key`, including reserved `clog:*` ones.
    pub claims: OrdMap<String, StoredClaim>,
    /// Kind classifications, keyed by `claim_key`.
    pub kinds: OrdMap<String, KindLabel>,
    /// Claims with no kind classification yet, keyed by `claim_key`.
    pub unclassified: OrdSet<String>,
    /// Claim keys grouped by `subject_key`.
    pub by_subject: OrdMap<String, OrdSet<String>>,
    /// Claim keys grouped by the observer's inner string.
    pub by_observer: OrdMap<String, OrdSet<String>>,
    /// Claim keys grouped by mentioned entity, after alias resolution.
    pub by_entity: OrdMap<EntityKey, OrdSet<String>>,
    /// The current entity alias map.
    pub aliases: AliasMap,
    /// Display names for entities, keyed by resolved entity key, alongside
    /// the epoch-millis timestamp the name was last set.
    pub names: OrdMap<EntityKey, (u64, String)>,
    /// The winning claim key per subject, or `None` for an all-floored group.
    pub believed: OrdMap<String, Option<String>>,
    /// Claim keys whose kind is one of the configured loop kinds (§5.7).
    pub open_loops: OrdSet<String>,
    /// Per-scope urgent rows: `(score, claim_key)`, sorted score-desc, tie
    /// `claim_key`-asc, truncated to the scope's `top_k`.
    pub urgent: OrdMap<String, Vec<(f32, String)>>,
}

/// Whether an `Engine::apply` call had any observable effect.
///
/// P1's naive re-render recomputes every scope's slot inputs per batch (an
/// auditable oracle; see the build design §5); `touched=false` short-circuits
/// when a batch applied zero effective events. Richer per-view diffs arrive
/// in P2 when wakes need them.
// `touched` is not read by production code yet: P1's actor re-renders every
// scope after every committed batch (and a batch with no effective events is
// never committed at all, so `touched` would always be true there). The
// short-circuit earns its keep in P2, when a tick can apply zero effective
// events. Exercised by `naive`'s tests meanwhile.
#[allow(dead_code)]
pub(crate) struct ApplyResult {
    /// Whether the batch had any observable effect on `WorldViews`.
    pub touched: bool,
}

/// The engine contract: applies WAL events to the materialized views and
/// exposes the current snapshot for reading.
pub(crate) trait Engine: Send {
    /// Applies `events` to the materialized views, using `scopes` to
    /// recompute per-scope derived state (e.g. `urgent`) and `now_ms` as the
    /// clock for recency-sensitive computations.
    fn apply(&mut self, events: &[Event], scopes: &BTreeMap<String, Focus>, now_ms: u64) -> ApplyResult;
    /// Borrows the current materialized-view snapshot.
    fn views(&self) -> &WorldViews;
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::validate::tests_base_claim;

    #[test]
    fn batch_round_trips_postcard() {
        let b = Batch {
            rev: 3,
            events: vec![
                Event::Observe(StoredClaim { claim: tests_base_claim(), recorded_at: 9 }),
                Event::Retract { claim_key: "k1".into() },
                Event::Judge { claim_key: "k1".into(), kind: "risk".into(), confidence: 1.0, source: crate::JudgeSource::Rule },
                Event::Tick { epoch: 4 },
            ],
        };
        let bytes = postcard::to_allocvec(&b).unwrap();
        let b2: Batch = postcard::from_bytes(&bytes).unwrap();
        assert_eq!(b2.rev, 3);
        assert_eq!(b2.events.len(), 4);
    }
}
