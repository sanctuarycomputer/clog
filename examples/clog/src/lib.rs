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
/// ```no_run
/// use clog::*;
///
/// let handle = Clog::open(Config::default_for("/var/lib/my-agent/clog"))?;
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
/// println!("{}", handle.situation(None, None)?.text);
/// # Ok::<(), ClogError>(())
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
        self.write(WriteOp::Retract { claim_key: claim_key.to_string() })
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
    /// changed, which may lag the global revision — that skew is meaningful
    /// (spec §5.10): it says the writes in between did not affect this
    /// scope.
    ///
    /// # Errors
    ///
    /// - `ClogError::UnknownScope` if `scope` names a scope that does not
    ///   exist;
    /// - `ClogError::TemplateError` if `template` does not parse. That
    ///   rejects the call only; nothing is written and no stored document
    ///   is affected.
    pub fn situation(&self, scope: Option<&str>, template: Option<&str>) -> Result<Situation, ClogError> {
        let snapshot = self.inner.snapshot.load();
        let scope = scope.unwrap_or(DEFAULT_SCOPE);
        let state = snapshot.situations.get(scope).ok_or(ClogError::UnknownScope)?;
        match template {
            None => Ok(state.situation.clone()),
            Some(source) => {
                let template = parse(source)?;
                let text = render(&template, &state.inputs, self.inner.budget_chars);
                Ok(Situation { text, ..state.situation.clone() })
            }
        }
    }

    /// Moves the manual clock forward by `ms` milliseconds.
    ///
    /// This is the test handle for time (INV-10): with `ClockMode::Manual`
    /// nothing else advances the clock, so recency decay, `recorded_at` and
    /// every rendered `as_of` are entirely under the caller's control.
    ///
    /// P1 emits no tick events, so advancing commits no batch and takes no
    /// revision. It does re-score and re-publish, so the situation you read
    /// afterwards reflects the new time — including a freshly computed
    /// `changes` slot, which is always the delta since the previous render.
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
        self.inner.tx.send(Cmd::Advance(ms, reply)).map_err(|_| ClogError::ShuttingDown)?;
        done.recv().map_err(|_| ClogError::ShuttingDown)
    }

    /// Sends one write to the writer thread and blocks for its reply. A
    /// full queue blocks here: backpressure is the point (spec §6.1).
    fn write(&self, op: WriteOp) -> Result<Ack, ClogError> {
        let (reply, ack) = bounded(1);
        self.inner.tx.send(Cmd::Write(WriteReq { op, reply })).map_err(|_| ClogError::ShuttingDown)?;
        ack.recv().map_err(|_| ClogError::ShuttingDown)?
    }
}
