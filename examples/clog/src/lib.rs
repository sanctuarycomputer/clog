//! Clog: an orientation engine for agentic systems.
//!
//! Hosts write structured claims; clog maintains materialized views over them
//! incrementally and renders a budgeted situation document per scope. See
//! `docs/clog-spec-v1.md` in the repository root for the full specification.
#![deny(missing_docs)]
#![cfg_attr(not(test), deny(clippy::unwrap_used, clippy::expect_used))]

use std::sync::Arc;
use std::thread::JoinHandle;

use arc_swap::ArcSwap;
use crossbeam_channel::{Sender, bounded};

use crate::actor::{Cmd, DEFAULT_SCOPE, WorldSnapshot, WriteOp, WriteReq};
use crate::clock::Clock;
use crate::render::render;
use crate::render::template::parse;

/// Public API types: the serde-only contract every later task builds on.
pub mod types;
pub use types::*;

/// Observe-time validation rules (spec §10).
pub(crate) mod validate;

/// Pure scoring functions (spec §5.4).
pub(crate) mod score;

/// Belief resolution: the total order that decides which claim is believed
/// per subject (spec §5.3).
pub(crate) mod belief;

/// Depth-1 entity alias map with write-time flattening (spec §5.2).
pub(crate) mod alias;

/// Rules-tier classifier: the free, deterministic first tier of the
/// cascade classifier (spec §5.6).
pub(crate) mod kinds;

/// Deterministic rendering: template parsing, RFC3339 timestamps, and the
/// slot renderer itself (spec §5.8).
pub(crate) mod render;

/// Engine contract: the WAL wire format, the materialized-view snapshot,
/// and the `Engine` trait the naive engine and WAL build on (spec §5).
pub(crate) mod engine;

/// The append-only write-ahead log: clog's source of truth (spec §6.3,
/// INV-11, recovery test R2).
pub(crate) mod wal;

/// The clock port: the only wall-clock read in the crate (spec §5.5).
pub(crate) mod clock;

/// The writer actor: the single thread owning the engine and the WAL,
/// and the immutable snapshot readers see (spec §5.1, §6.1).
pub(crate) mod actor;

/// A handle to a running clog instance.
///
/// Cheap to clone (`Clone + Send + Sync`): every clone talks to the same
/// writer thread and reads the same published snapshot. Writes are totally
/// ordered — each call commits at most one batch and takes at most one rev
/// — while reads never touch the writer at all, returning whatever snapshot
/// was current when they were called (INV-1).
///
/// Dropping the last handle shuts the instance down: the writer finishes
/// any in-flight batch, fsyncs the WAL and exits before `drop` returns.
///
/// The full host loop — open, observe, read a situation, retract a
/// correction, read again — also lives as a runnable example at
/// `examples/quickstart.rs` (`cargo run -p clog --example quickstart`):
///
/// ```
/// use clog::*;
///
/// let dir = tempfile::tempdir()?;
/// let handle = Clog::open(Config::default_for(dir.path()))?;
/// handle.observe(
///     vec![Claim {
///         claim_key: "halcyon:inv-1042".into(),
///         subject_key: Some("halcyon:inv-1042:status".into()),
///         source_ref: "gmail:msg/123".into(),
///         observer: ObserverId::from("gmail-v3"),
///         schema_v: 1,
///         occurred_at: 1_786_752_000_000,
///         observed_at: 1_786_752_000_000,
///         reliability: Reliability::B,
///         credibility: Credibility::Two,
///         entities: vec![],
///         body: "Invoice 1042 is 30 days overdue".into(),
///     }],
///     ObserveOpts::default(),
/// )?;
/// let before = handle.situation(None, None)?;
/// assert!(before.text.contains("Invoice 1042"));
///
/// // A correction flows back as a retract, not a mutation.
/// handle.retract("halcyon:inv-1042")?;
/// let after = handle.situation(None, None)?;
/// assert!(after.rev > before.rev);
/// assert!(!after.text.contains("## urgent\n1."));
/// # Ok::<(), Box<dyn std::error::Error>>(())
/// ```
#[derive(Clone)]
pub struct Clog {
    inner: Arc<Inner>,
}

/// The shared guts of a `Clog`, and the shutdown latch: the writer thread is
/// joined when the last handle drops this.
struct Inner {
    tx: Sender<Cmd>,
    snapshot: Arc<ArcSwap<WorldSnapshot>>,
    clock: Clock,
    budget_chars: usize,
    writer: Option<JoinHandle<()>>,
}

impl Drop for Inner {
    fn drop(&mut self) {
        // A send error means the writer already died; either way the join
        // below is what guarantees the WAL is flushed before we return.
        let _ = self.tx.send(Cmd::Shutdown);
        if let Some(writer) = self.writer.take() {
            let _ = writer.join();
        }
    }
}

impl Clog {
    /// Opens (creating if needed) the instance rooted at `cfg.path`.
    ///
    /// Replays the write-ahead log into a fresh engine, rebuilds every
    /// scope's situation document batch by batch so revisions come back
    /// exactly as they were written (INV-9), publishes the first snapshot
    /// and starts the writer thread. The `"default"` scope is injected if
    /// the config does not declare it.
    ///
    /// # Errors
    ///
    /// - `ClogError::Storage` if the WAL directory cannot be created, read
    ///   or opened;
    /// - `ClogError::Corrupt` if a classification rule's regex fails to
    ///   compile;
    /// - `ClogError::UnknownKind` / `ClogError::InvalidFilter` if a
    ///   configured `Focus` is invalid (spec §10).
    ///
    /// A torn or corrupt WAL tail is *not* an error: it is quarantined and
    /// truncated, and the surviving prefix is replayed (recovery test R2).
    pub fn open(cfg: Config) -> Result<Clog, ClogError> {
        let spawned = actor::spawn(cfg)?;
        Ok(Clog {
            inner: Arc::new(Inner {
                tx: spawned.tx,
                snapshot: spawned.snapshot,
                clock: spawned.clock,
                budget_chars: spawned.budget_chars,
                writer: Some(spawned.join),
            }),
        })
    }

    /// Records a batch of claims, returning the revision it committed at.
    ///
    /// The batch is atomic: if any claim fails validation (spec §10) the
    /// whole call is rejected and nothing is written. Re-observing a key
    /// with an identical claim is invisible — no revision, no log record
    /// (INV-5); re-observing it with any difference supersedes the previous
    /// version (INV-4). Claims that a configured rule matches are
    /// classified as part of the same batch.
    ///
    /// A batch in which *every* claim was a duplicate commits nothing and
    /// returns the current revision, so `Ack.rev` is unchanged from the
    /// previous call.
    ///
    /// # Errors
    ///
    /// - `ClogError::InvalidClaim` / `ClogError::ReservedNamespace` if any
    ///   claim is invalid or uses the reserved `clog:` namespace;
    /// - `ClogError::UnknownScope` if `opts.return_situation` names a scope
    ///   that does not exist (nothing is written in that case);
    /// - `ClogError::Storage` / `ClogError::Corrupt` if the log append
    ///   fails;
    /// - `ClogError::ShuttingDown` if the instance is stopping.
    pub fn observe(&self, claims: Vec<Claim>, opts: ObserveOpts) -> Result<Ack, ClogError> {
        self.write(WriteOp::Observe { claims, opts })
    }

    /// Retracts a claim, healing every view that mentioned it (INV-3).
    ///
    /// Retracting the reserved claim a merge wrote un-merges those entities
    /// (spec §5.2).
    ///
    /// # Errors
    ///
    /// - `ClogError::UnknownClaim` if the key is not live — including a key
    ///   that was already retracted. Nothing is written in that case, so
    ///   idempotent hosts can ignore the error;
    /// - `ClogError::Storage` / `ClogError::Corrupt` if the log append
    ///   fails;
    /// - `ClogError::ShuttingDown` if the instance is stopping.
    pub fn retract(&self, claim_key: &str) -> Result<Ack, ClogError> {
        self.write(WriteOp::Retract {
            claim_key: claim_key.to_string(),
        })
    }

    /// Withdraws everything an observer ever said, in one batch.
    ///
    /// This is the integration kill switch (spec §5.1, INV-6): a misbehaving
    /// or decommissioned source is removed wholesale, and every view it
    /// appeared in heals as if it had never written (INV-3). However many
    /// claims it had, the revocation is a single revision — hosts can rely
    /// on there being no intermediate state in which the observer is
    /// half-gone. Its reserved `clog:*` claims go with it, so a merge that
    /// observer's writes caused is undone too.
    ///
    /// An observer with nothing live commits nothing at all and returns the
    /// current revision, so revoking twice is invisible rather than an error.
    ///
    /// ```no_run
    /// # use clog::*;
    /// # let handle = Clog::open(Config::default_for("/var/lib/my-agent/clog"))?;
    /// handle.revoke_observer(&ObserverId::from("gmail-v2"))?;
    /// # Ok::<(), ClogError>(())
    /// ```
    ///
    /// # Errors
    ///
    /// - `ClogError::Storage` / `ClogError::Corrupt` if the log append
    ///   fails;
    /// - `ClogError::ShuttingDown` if the instance is stopping.
    pub fn revoke_observer(&self, observer: &ObserverId) -> Result<Ack, ClogError> {
        self.write(WriteOp::RevokeObserver {
            observer: observer.clone(),
        })
    }

    /// Declares that `alias` and `canonical` are the same entity: every view
    /// that groups or filters by entity now reports them as one (spec §5.2).
    ///
    /// The merge is itself a claim, written in the reserved namespace under
    /// the key `clog:merge:{alias.etype}:{alias.id}->{canonical.etype}:{canonical.id}`.
    /// That claim is invisible to `select` and to every rendered document
    /// (INV-8), but it is a real, logged, retractable claim: passing its key
    /// to [`Clog::retract`] un-merges the pair and re-keys every view back.
    ///
    /// Alias edges are depth-1 — merging onto an entity that is itself
    /// merged away points at the far end instead — so no chain ever forms
    /// and resolution is always a single hop. Merging the same pair twice
    /// commits nothing new.
    ///
    /// ```no_run
    /// # use clog::*;
    /// # let handle = Clog::open(Config::default_for("/var/lib/my-agent/clog"))?;
    /// let dup = EntityRef { etype: "person".into(), id: "sam.b".into(), name: None };
    /// let real = EntityRef { etype: "person".into(), id: "sam".into(), name: None };
    /// handle.merge_entities(&dup, &real)?;
    /// // ...and back again
    /// handle.retract("clog:merge:person:sam.b->person:sam")?;
    /// # Ok::<(), ClogError>(())
    /// ```
    ///
    /// # Errors
    ///
    /// - `ClogError::AliasCycle` if the merge would close a loop: merging an
    ///   entity onto itself (whether or not it is already merged elsewhere),
    ///   or reversing an existing merge without retracting it first. Nothing
    ///   is written in that case;
    /// - `ClogError::InvalidClaim` if either entity ref breaks spec §10 (an
    ///   empty or over-long `etype`/`id`, or a control character in one);
    /// - `ClogError::Storage` / `ClogError::Corrupt` if the log append
    ///   fails;
    /// - `ClogError::ShuttingDown` if the instance is stopping.
    pub fn merge_entities(
        &self,
        alias: &EntityRef,
        canonical: &EntityRef,
    ) -> Result<Ack, ClogError> {
        self.write(WriteOp::Merge {
            alias: alias.clone(),
            canonical: canonical.clone(),
        })
    }

    /// Reads rows out of one materialized view, filtered.
    ///
    /// A pure read of the current snapshot: it never touches the writer, so
    /// it neither blocks behind in-flight writes nor sees a partially
    /// applied batch, and two calls at the same revision always agree
    /// (INV-1).
    ///
    /// Each view brings its own order — `Live`, `OpenLoops` and
    /// `Unclassified` by `claim_key` ascending, `Urgent` by descending rank
    /// within the named scope, `EntityState` by (canonical entity, subject)
    /// ascending. `EntityState` reports only *believed* claims, and reports
    /// **every** believed subject of every entity: the 8-row cap in a
    /// rendered document is a rendering budget, not a limit on this API, so
    /// `limit` is the only cap here. A claim believed under several entities
    /// is reported once, at its lowest-ordered entity. Reserved `clog:*`
    /// claims never appear in any view (INV-8).
    ///
    /// Every filter is optional and they are AND-composed. In detail:
    ///
    /// - `kinds` — matches any of the named kinds. An unclassified claim
    ///   matches no `kinds` filter.
    /// - `entities` — matches a claim mentioning any of the named entities,
    ///   resolved through the merge map on both sides, so filtering on
    ///   either half of a merged pair finds the same rows. `Some(vec![])`
    ///   names no entity and so matches nothing.
    /// - `observer` — exact match.
    /// - `subject_prefix` — `starts_with` on `subject_key`. A claim with no
    ///   subject matches no `subject_prefix` filter, not even `""`.
    /// - `occurred_after` — strictly greater than.
    /// - `min_score` — inclusive (`score >= min_score`), `View::Urgent` only.
    /// - `limit` — defaults to 50 and is clamped to 500; `Some(0)` returns
    ///   no rows.
    ///
    /// ```no_run
    /// # use clog::*;
    /// # let handle = Clog::open(Config::default_for("/var/lib/my-agent/clog"))?;
    /// let rows = handle.select(
    ///     View::Urgent { scope: "default".into() },
    ///     Filter { kinds: Some(vec!["risk".into()]), min_score: Some(0.25), ..Filter::default() },
    /// )?;
    /// for row in &rows {
    ///     println!("{:?} {}", row.score, row.claim.body);
    /// }
    /// # Ok::<(), ClogError>(())
    /// ```
    ///
    /// # Errors
    ///
    /// - `ClogError::InvalidFilter` if `min_score` is set on any view but
    ///   `View::Urgent`, where no row has a score to compare;
    /// - `ClogError::UnknownScope` if `View::Urgent` names a scope that does
    ///   not exist.
    pub fn select(&self, view: View, filter: Filter) -> Result<Vec<Row>, ClogError> {
        // `load_full` rather than `load`: hydrating and filtering rows is
        // caller-sized work, and an `ArcSwap` guard must not be held across
        // it (the same reason `situation` takes a full load).
        actor::select(&self.inner.snapshot.load_full(), view, filter)
    }

    /// Reads a scope's situation document from the current snapshot.
    ///
    /// `scope` defaults to `"default"`. `template` defaults to the built-in
    /// template (spec §5.8); passing one renders the *same* materialized
    /// slot inputs through it — a read never recomputes a view, so two
    /// reads at the same revision always agree (INV-1). Templates are
    /// per-call: clog stores none.
    ///
    /// The returned `rev` is the revision at which this scope's text last
    /// *materially* changed, which may lag the global revision — that skew
    /// is meaningful (spec §5.10): it says the writes in between did not
    /// affect what this scope has to say. `as_of` moves with it, so an
    /// unchanged document never carries a freshened timestamp.
    ///
    /// # Errors
    ///
    /// - `ClogError::UnknownScope` if `scope` names a scope that does not
    ///   exist;
    /// - `ClogError::TemplateError` if `template` does not parse. That
    ///   rejects the call only; nothing is written and no stored document
    ///   is affected.
    pub fn situation(
        &self,
        scope: Option<&str>,
        template: Option<&str>,
    ) -> Result<Situation, ClogError> {
        // `load_full` rather than `load`: parsing and rendering a custom
        // template is unbounded caller-supplied work, and an `ArcSwap` guard
        // must not be held across it.
        let snapshot = self.inner.snapshot.load_full();
        let scope = scope.unwrap_or(DEFAULT_SCOPE);
        let state = snapshot
            .situations
            .get(scope)
            .ok_or(ClogError::UnknownScope)?;
        match template {
            None => Ok(state.situation.clone()),
            Some(source) => {
                let template = parse(source)?;
                let text = render(&template, &state.inputs, self.inner.budget_chars);
                Ok(Situation {
                    text,
                    ..state.situation.clone()
                })
            }
        }
    }

    /// Moves the manual clock forward by `ms` milliseconds.
    ///
    /// This is the test handle for time (INV-10): with `ClockMode::Manual`
    /// nothing else advances the clock, so recency decay, `recorded_at` and
    /// every rendered `as_of` are entirely under the caller's control.
    ///
    /// P1 emits no tick events, so advancing is pure clock movement: it
    /// commits no batch, takes no revision, and re-renders nothing. An
    /// immaterial clock move must leave every scope exactly as it was. The
    /// new time reaches the views at the next committed batch, which
    /// re-scores against it. Advancing is still ordered against in-flight
    /// writes, so a write submitted before it always sees the earlier time.
    ///
    /// # Errors
    ///
    /// - `ClogError::ManualClockRequired` if the instance was configured
    ///   with `ClockMode::System`;
    /// - `ClogError::ShuttingDown` if the instance is stopping.
    pub fn advance(&self, ms: u64) -> Result<(), ClogError> {
        if !self.inner.clock.is_manual() {
            return Err(ClogError::ManualClockRequired);
        }
        let (reply, done) = bounded(1);
        self.inner
            .tx
            .send(Cmd::Advance(ms, reply))
            .map_err(|_| ClogError::ShuttingDown)?;
        done.recv().map_err(|_| ClogError::ShuttingDown)
    }

    /// Sends one write to the writer thread and blocks for its reply. A
    /// full queue blocks here: backpressure is the point (spec §6.1).
    fn write(&self, op: WriteOp) -> Result<Ack, ClogError> {
        let (reply, ack) = bounded(1);
        self.inner
            .tx
            .send(Cmd::Write(WriteReq { op, reply }))
            .map_err(|_| ClogError::ShuttingDown)?;
        ack.recv().map_err(|_| ClogError::ShuttingDown)?
    }
}
