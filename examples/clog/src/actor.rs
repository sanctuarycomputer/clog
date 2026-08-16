//! The writer actor: the single thread that owns the engine and the WAL
//! (spec §5.1, §6.1; build design §4).
//!
//! Every write call in the public API becomes exactly one command on a
//! bounded channel and blocks on a one-shot reply, so writes are totally
//! ordered and backpressure is just a blocking send. Readers never touch
//! this thread: after each batch the writer publishes an immutable
//! [`WorldSnapshot`] through [`ArcSwap`], and `situation` (and, from Task
//! 15, `select`) read that snapshot and nothing else (INV-1).
//!
//! **Write-ahead ordering is mandatory** (§6.3): the WAL append (and its
//! fsync) completes *before* `engine.apply`, so a crash can only ever lose
//! the tail of the log, never leave the engine ahead of it (R1).
//!
//! **The WAL records effects, not intentions** (build design §4): a
//! committed batch carries the caller's events plus the classifier's
//! derived `Judge` events, exactly as applied. Replay is pure event
//! application — the classifier never runs on replay — so the same WAL
//! bytes always rebuild the same world (INV-10).

use std::collections::BTreeMap;
use std::sync::Arc;
use std::thread::JoinHandle;

use arc_swap::ArcSwap;
use crossbeam_channel::{Receiver, Sender, bounded};
use imbl::OrdMap;

use crate::clock::Clock;
use crate::engine::naive::{NaiveCfg, NaiveEngine, entity_state};
use crate::engine::{Batch, Engine, Event, StoredClaim, WorldViews};
use crate::kinds::{self, RuleSet};
use crate::render::template::{DEFAULT_TEMPLATE, Template, parse};
use crate::render::{ChangeItem, EntityItem, LoopItem, SlotInputs, UrgentItem, headline, render};
use crate::types::{Ack, Claim, ClogError, Config, Focus, ObserveOpts, Rev, Situation};
use crate::validate::{validate_claim, validate_focus};
use crate::wal::{self, Wal};

/// The scope every instance always has (build design §9): a uniform focus,
/// used whenever `situation(None, ..)` is called.
pub(crate) const DEFAULT_SCOPE: &str = "default";

// ---- commands -------------------------------------------------------------

/// One unit of work for the writer thread.
pub(crate) enum Cmd {
    /// A write that commits (at most) one batch and replies with an `Ack`.
    Write(WriteReq),
    /// Move the manual clock, replying once the new time is published.
    Advance(u64, Sender<()>),
    /// Stop the loop: flush the WAL and return so the handle can join.
    Shutdown,
}

/// A write command plus the one-shot channel its `Ack` goes back on.
pub(crate) struct WriteReq {
    /// What to write.
    pub op: WriteOp,
    /// Where the result goes.
    pub reply: Sender<Result<Ack, ClogError>>,
}

/// The write operations the public API exposes in P1.
pub(crate) enum WriteOp {
    /// `Clog::observe`.
    Observe {
        /// The claims to record, in caller order.
        claims: Vec<Claim>,
        /// Per-call options (currently just `return_situation`).
        opts: ObserveOpts,
    },
    /// `Clog::retract`.
    Retract {
        /// The key to retract.
        claim_key: String,
    },
}

// ---- published state ------------------------------------------------------

/// Everything a scope's rendered document is made of, kept alongside the
/// document itself so a custom-template read is pure string assembly (INV-1:
/// reads never compute views).
///
/// `membership` is the `urgent ∪ open_loops` key set **as of the last
/// render**, mapped to the headline each key had then. It is what the
/// `changes` slot diffs against; the headlines are stored (rather than
/// looked up later) because a removed claim is, by definition, no longer
/// live to look up.
#[derive(Clone)]
pub(crate) struct SituationState {
    /// The rendered document.
    pub situation: Situation,
    /// The slot inputs it was rendered from.
    pub inputs: SlotInputs,
    /// `claim_key -> headline` for everything the document listed.
    pub membership: OrdMap<String, String>,
}

/// The immutable world as of one committed batch, published atomically.
///
/// Every field is part of the published contract (spec §6.1), but P1's only
/// reader is `situation`, which needs `situations` alone: a document already
/// carries its own rev and as_of. The rest is read by `select` (Task 15) and
/// by wake evaluation (P2); it is published now so readers never have to ask
/// the writer a question.
pub(crate) struct WorldSnapshot {
    /// The global rev this snapshot reflects.
    #[allow(dead_code)]
    pub rev: Rev,
    /// The clock reading this snapshot was published at.
    #[allow(dead_code)]
    pub as_of: u64,
    /// The engine's materialized views.
    // Read by `select` (Task 15); the situation path reads `situations`.
    #[allow(dead_code)]
    pub views: WorldViews,
    /// The scopes in force, `"default"` always present.
    // Read by `set_focus`/`select` (Tasks 15+).
    #[allow(dead_code)]
    pub scopes: BTreeMap<String, Focus>,
    /// The rendered document per scope.
    pub situations: BTreeMap<String, SituationState>,
}

// ---- crash injection ------------------------------------------------------

/// Aborts the process immediately after the WAL append of batch `rev`, if
/// `CLOG_CRASH_AFTER_WAL` names that rev.
///
/// Compiled only under `--features test-crash`; the R1 recovery harness
/// (Task 18) re-execs itself as a child that dies here, then reopens and
/// asserts the replayed world matches.
#[cfg(feature = "test-crash")]
fn maybe_crash_after_wal(rev: Rev) {
    if let Ok(n) = std::env::var("CLOG_CRASH_AFTER_WAL")
        && n.parse::<Rev>() == Ok(rev)
    {
        std::process::abort();
    }
}

// ---- startup --------------------------------------------------------------

/// The handles `Clog` keeps after the writer thread is running.
pub(crate) struct Spawned {
    /// The command channel.
    pub tx: Sender<Cmd>,
    /// The published snapshot cell.
    pub snapshot: Arc<ArcSwap<WorldSnapshot>>,
    /// The writer thread, joined on shutdown.
    pub join: JoinHandle<()>,
    /// The clock, shared with the writer.
    pub clock: Clock,
    /// The render budget, needed by custom-template reads.
    pub budget_chars: usize,
}

/// Opens the WAL, rebuilds the world from it, publishes the first snapshot
/// and starts the writer thread.
///
/// Everything that can fail happens on the caller's thread, so `Clog::open`
/// reports it: bad regexes in the taxonomy, an invalid `Focus`, an
/// unreadable WAL directory. Replay applies each batch's events straight to
/// the engine (no validation, no classifier — the events *are* the effects)
/// and re-renders after each batch exactly as a live commit does, so scope
/// documents and the global rev come back reproducibly (INV-9).
///
/// `Config::rebuild_on_open` is accepted and, in P1, changes nothing: there
/// is no engine-state cache to drop yet (build design §5 defers snapshot
/// files to M5), so every open is already a full WAL rebuild.
pub(crate) fn spawn(cfg: Config) -> Result<Spawned, ClogError> {
    let clock = Clock::new(cfg.tick.mode);
    let scopes = resolve_scopes(&cfg)?;
    let rules = kinds::compile(&cfg.kinds)?;
    let template = parse(DEFAULT_TEMPLATE)?;
    let (wal, batches) = wal::open_dir(&cfg.path, cfg.wal_fsync)?;

    let engine = NaiveEngine::new(NaiveCfg {
        loop_kinds: cfg.loop_kinds.clone(),
        top_k: cfg.top_k,
        buckets_per_half_life: cfg.decay_buckets_per_half_life,
        belief_floor: cfg.belief_min_credibility,
    });
    let snapshot = Arc::new(ArcSwap::from_pointee(WorldSnapshot {
        rev: 0,
        as_of: 0,
        views: WorldViews::default(),
        scopes: scopes.clone(),
        situations: BTreeMap::new(),
    }));

    let mut writer = Writer {
        engine,
        wal,
        rules,
        clock: clock.clone(),
        template,
        budget_chars: cfg.budget_chars,
        scopes,
        rev: 0,
        situations: BTreeMap::new(),
        snapshot: Arc::clone(&snapshot),
    };
    writer.rebuild(&batches);

    let (tx, rx) = bounded(cfg.write_queue.max(1));
    let join = std::thread::Builder::new()
        .name("clog-writer".to_string())
        .spawn(move || run(writer, rx))?;

    Ok(Spawned { tx, snapshot, join, clock, budget_chars: cfg.budget_chars })
}

/// The configured scopes with `"default"` injected if the host did not
/// declare it (build design §9), every focus validated against the
/// taxonomy (§10) before the instance is allowed to open.
fn resolve_scopes(cfg: &Config) -> Result<BTreeMap<String, Focus>, ClogError> {
    let mut scopes = cfg.scopes.clone();
    scopes.entry(DEFAULT_SCOPE.to_string()).or_insert_with(Focus::uniform);
    for focus in scopes.values() {
        validate_focus(focus, &cfg.kinds)?;
    }
    Ok(scopes)
}

/// The writer thread's loop: one command at a time, one `match`, no shared
/// mutable state anywhere else in the crate.
fn run(mut writer: Writer, rx: Receiver<Cmd>) {
    while let Ok(cmd) = rx.recv() {
        match cmd {
            Cmd::Write(req) => {
                let result = writer.write(req.op);
                // A caller that hung up between sending and replying is not
                // an error: the batch is already committed and durable.
                let _ = req.reply.send(result);
            }
            Cmd::Advance(ms, reply) => {
                writer.advance(ms);
                let _ = reply.send(());
            }
            Cmd::Shutdown => break,
        }
    }
    writer.shutdown();
}

// ---- the writer -----------------------------------------------------------

/// The writer thread's state. Owned by one thread; never shared.
struct Writer {
    engine: NaiveEngine,
    wal: Wal,
    rules: RuleSet,
    clock: Clock,
    template: Template,
    budget_chars: usize,
    scopes: BTreeMap<String, Focus>,
    rev: Rev,
    situations: BTreeMap<String, SituationState>,
    snapshot: Arc<ArcSwap<WorldSnapshot>>,
}

impl Writer {
    /// Rebuilds the world from the replayed WAL, then publishes it.
    ///
    /// The rev-0 render happens *before* replay so that a reopened instance
    /// walks exactly the same render sequence a fresh one did: empty world
    /// at rev 0, then one render per batch. Every replay render uses a
    /// single clock reading (P1 persists no per-batch clock), so `as_of`
    /// values are as-of-open rather than as-of-original-commit; the rendered
    /// content and the global rev are reproduced exactly.
    fn rebuild(&mut self, batches: &[Batch]) {
        let now = self.clock.now_ms();
        self.engine.apply(&[], &self.scopes, now);
        self.render_all(now);
        for batch in batches {
            self.rev = batch.rev;
            self.engine.apply(&batch.events, &self.scopes, now);
            self.render_all(now);
        }
        self.publish(now);
    }

    /// Dispatches one write command.
    fn write(&mut self, op: WriteOp) -> Result<Ack, ClogError> {
        match op {
            WriteOp::Observe { claims, opts } => self.observe(claims, opts),
            WriteOp::Retract { claim_key } => self.retract(claim_key),
        }
    }

    /// `Clog::observe`: steps 1-4 of the write path, then [`Self::commit`].
    fn observe(&mut self, claims: Vec<Claim>, opts: ObserveOpts) -> Result<Ack, ClogError> {
        // 1. Validate the whole batch before anything else. The first
        //    failure rejects every claim in it (§10, atomic batches).
        for (index, claim) in claims.iter().enumerate() {
            validate_claim(index, claim, false)?;
        }
        // An unknown `return_situation` scope is a malformed request, so it
        // is rejected here rather than after committing: a caller that gets
        // an `Err` back must be able to assume nothing was written.
        if let Some(scope) = &opts.return_situation
            && !self.situations.contains_key(scope)
        {
            return Err(ClogError::UnknownScope);
        }
        // 2. One clock read for the whole batch.
        let now = self.clock.now_ms();
        // 3 + 4.
        let events = self.expand(&claims, now);
        self.commit(events, opts.return_situation.as_deref(), now)
    }

    /// `Clog::retract`. An unknown (or already retracted) key commits
    /// nothing at all — no batch, no rev bump, no WAL record.
    ///
    /// Reserved `clog:*` keys *are* retractable here: retracting the merge
    /// claim is how a merge is undone (spec §4, §5.2).
    fn retract(&mut self, claim_key: String) -> Result<Ack, ClogError> {
        if !self.engine.views().claims.contains_key(&claim_key) {
            return Err(ClogError::UnknownClaim);
        }
        let now = self.clock.now_ms();
        self.commit(vec![Event::Retract { claim_key }], None, now)
    }

    /// Write-path step 3 (upsert expansion) and step 4 (rules tier).
    ///
    /// Expansion compares each claim against the version that is live *at
    /// that point in the batch*: an identical claim is skipped entirely
    /// (INV-5 — no rev bump, no WAL record, invisible), a different one
    /// becomes `Retract(old)` + `Observe(new)` (INV-4). The comparison is
    /// full structural equality of the `Claim`; `recorded_at` is not part
    /// of a claim, so a re-send with a later arrival time is still a
    /// duplicate.
    ///
    /// The rules tier then classifies everything the batch actually
    /// observes and appends the resulting `Judge` events *after* all claim
    /// events, so a judgment never precedes the claim it judges.
    fn expand(&self, claims: &[Claim], now: u64) -> Vec<Event> {
        let mut events = Vec::new();
        // What each key holds so far *within this batch*, so a batch that
        // names the same key twice behaves like two consecutive batches.
        let mut pending: BTreeMap<&str, &Claim> = BTreeMap::new();

        for claim in claims {
            let live = pending
                .get(claim.claim_key.as_str())
                .copied()
                .or_else(|| self.engine.views().claims.get(&claim.claim_key).map(|sc| &sc.claim));
            match live {
                Some(old) if old == claim => continue,
                Some(_) => events.push(Event::Retract { claim_key: claim.claim_key.clone() }),
                None => {}
            }
            events.push(Event::Observe(StoredClaim { claim: claim.clone(), recorded_at: now }));
            pending.insert(claim.claim_key.as_str(), claim);
        }

        let judgments: Vec<Event> = events
            .iter()
            .filter_map(|event| {
                let Event::Observe(stored) = event else { return None };
                let label = kinds::classify(&self.rules, &stored.claim)?;
                Some(Event::Judge {
                    claim_key: stored.claim.claim_key.clone(),
                    kind: label.kind,
                    confidence: label.confidence,
                    source: label.source,
                })
            })
            .collect();
        events.extend(judgments);
        events
    }

    /// Write-path steps 5-9: commit one batch, or nothing.
    ///
    /// An empty event list is *not* a batch: it takes no rev, writes no WAL
    /// record and re-renders nothing, so a wholly duplicate `observe` is
    /// invisible to every reader (INV-5).
    fn commit(&mut self, events: Vec<Event>, want: Option<&str>, now: u64) -> Result<Ack, ClogError> {
        // 5. Nothing to do.
        if events.is_empty() {
            return Ok(self.ack(want));
        }
        // 6. WAL first, engine second — always (§6.3, R1). `rev` advances
        //    only once the record is durable, so a failed append leaves the
        //    world exactly where it was.
        let batch = Batch { rev: self.rev + 1, events };
        self.wal.append(&batch)?;
        #[cfg(feature = "test-crash")]
        maybe_crash_after_wal(batch.rev);
        self.rev = batch.rev;
        self.engine.apply(&batch.events, &self.scopes, now);
        // 7 + 8.
        self.render_all(now);
        self.publish(now);
        // 9.
        Ok(self.ack(want))
    }

    /// Moves the manual clock (spec §5.5 without the tick driver).
    ///
    /// P1 emits no `Tick` events, so this commits no batch and takes no rev
    /// — an immaterial clock move must never bump a rev (INV-5, B4). It
    /// does re-score and re-render: the clock is a material input to both
    /// recency decay and the header's `as_of`, and readers only ever see
    /// the published snapshot, so leaving it stale would report a time that
    /// has passed.
    fn advance(&mut self, ms: u64) {
        let now = self.clock.advance(ms);
        self.engine.apply(&[], &self.scopes, now);
        self.render_all(now);
        self.publish(now);
    }

    /// Flushes the WAL on the way out (spec §6.1: clean shutdown fsyncs).
    fn shutdown(&mut self) {
        let _ = self.wal.sync();
    }

    /// The reply for a completed (or skipped) write. `want` names the scope
    /// whose document to return; it was checked against the live scope set
    /// before the batch was assembled.
    fn ack(&self, want: Option<&str>) -> Ack {
        Ack {
            rev: self.rev,
            situation: want.and_then(|scope| self.situations.get(scope)).map(|s| s.situation.clone()),
        }
    }

    /// Write-path step 7: recompute and re-render every scope.
    ///
    /// P1 recomputes all slot inputs from the views wholesale rather than
    /// tracking which scopes a batch dirtied — the naive engine is the
    /// auditable oracle (build design §5), and targeted re-render is a P2
    /// optimization that must reproduce this result exactly.
    fn render_all(&mut self, now: u64) {
        let scopes: Vec<String> = self.scopes.keys().cloned().collect();
        for scope in scopes {
            self.render_scope(&scope, now);
        }
    }

    /// Renders one scope's default-template document, replacing the stored
    /// one **only if the text actually changed** — that is what keeps
    /// `Situation.rev` meaning "the rev at which this scope's text last
    /// changed" (§5.10) instead of just tracking the global rev.
    fn render_scope(&mut self, scope: &str, now: u64) {
        let mut inputs = slot_inputs(self.engine.views(), scope, self.rev, now);
        let membership: OrdMap<String, String> = inputs
            .urgent
            .iter()
            .map(|u| (u.claim_key.clone(), u.headline.clone()))
            .chain(inputs.open_loops.iter().map(|l| (l.claim_key.clone(), l.headline.clone())))
            .collect();

        let previous = self.situations.get(scope);
        inputs.changes = changes_since(previous.map(|s| &s.membership), &membership);
        let text = render(&self.template, &inputs, self.budget_chars);
        if previous.is_some_and(|s| s.situation.text == text) {
            return;
        }

        self.situations.insert(
            scope.to_string(),
            SituationState {
                situation: Situation { scope: scope.to_string(), text, rev: self.rev, as_of: now },
                inputs,
                membership,
            },
        );
    }

    /// Write-path step 8: publish the new snapshot. This is the moment the
    /// batch becomes visible to readers — everything before it is invisible
    /// and everything after it is committed (INV-1).
    fn publish(&self, as_of: u64) {
        self.snapshot.store(Arc::new(WorldSnapshot {
            rev: self.rev,
            as_of,
            views: self.engine.views().clone(),
            scopes: self.scopes.clone(),
            situations: self.situations.clone(),
        }));
    }
}

// ---- slot assembly --------------------------------------------------------

/// Builds one scope's slot inputs from the materialized views (§5.7, §5.8).
///
/// `changes` is left empty: only the writer knows the previous render's
/// membership, so it fills that slot in [`Writer::render_scope`].
fn slot_inputs(views: &WorldViews, scope: &str, rev: Rev, as_of_ms: u64) -> SlotInputs {
    let urgent = views
        .urgent
        .get(scope)
        .into_iter()
        .flatten()
        .filter_map(|(score, key)| {
            let stored = views.claims.get(key)?;
            Some(UrgentItem {
                score: *score,
                headline: headline(&stored.claim.body),
                reliability: stored.claim.reliability.letter(),
                credibility: stored.claim.credibility.digit(),
                claim_key: key.clone(),
            })
        })
        .collect();

    // `open_loops` is an `OrdSet`, so this is claim_key order.
    let open_loops = views
        .open_loops
        .iter()
        .filter_map(|key| {
            let stored = views.claims.get(key)?;
            let label = views.kinds.get(key)?;
            Some(LoopItem {
                kind: label.kind.clone(),
                headline: headline(&stored.claim.body),
                claim_key: key.clone(),
            })
        })
        .collect();

    // Entity rows are already (entity-key asc, then believed claims
    // newest-first). An entity nobody believes anything about contributes
    // no summaries, and a bare "Name: " line says nothing, so it is dropped
    // rather than rendered empty.
    let entities = entity_state(views)
        .into_iter()
        .filter(|(_, _, rows)| !rows.is_empty())
        .map(|(_, display, rows)| EntityItem {
            display,
            summaries: rows.iter().map(|(_, stored)| headline(&stored.claim.body)).collect(),
        })
        .collect();

    SlotInputs { scope: scope.to_string(), rev, as_of_ms, urgent, open_loops, entities, changes: Vec::new() }
}

/// The `changes` slot: the membership delta between the last two rendered
/// revs of a scope (§5.7).
///
/// Additions first, then removals, each in `claim_key` order (both maps are
/// ordered, so iteration gives that for free). A key present in both renders
/// produces nothing even if its headline changed: this slot tracks
/// membership, not content. Removed items take their headline from the
/// *previous* render, because a removed claim is no longer live to read.
fn changes_since(previous: Option<&OrdMap<String, String>>, current: &OrdMap<String, String>) -> Vec<ChangeItem> {
    let empty = OrdMap::new();
    let previous = previous.unwrap_or(&empty);
    let mut changes: Vec<ChangeItem> = current
        .iter()
        .filter(|(key, _)| !previous.contains_key(*key))
        .map(|(_, headline)| ChangeItem::Added(headline.clone()))
        .collect();
    changes.extend(
        previous
            .iter()
            .filter(|(key, _)| !current.contains_key(*key))
            .map(|(_, headline)| ChangeItem::Removed(headline.clone())),
    );
    changes
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{ClockMode, KindTaxonomy, TickConfig};

    fn manual_cfg(dir: &std::path::Path) -> Config {
        let mut cfg = Config::default_for(dir);
        cfg.tick = TickConfig { mode: ClockMode::Manual, interval_ms: 60_000 };
        cfg
    }

    #[test]
    fn default_scope_injected_and_focus_validated() {
        let dir = tempfile::tempdir().unwrap();
        let scopes = resolve_scopes(&manual_cfg(dir.path())).unwrap();
        assert_eq!(scopes.keys().collect::<Vec<_>>(), vec![DEFAULT_SCOPE]);

        let mut cfg = manual_cfg(dir.path());
        cfg.scopes.insert("bad".into(), Focus::uniform().weight("no-such-kind", 2.0));
        assert!(matches!(resolve_scopes(&cfg), Err(ClogError::UnknownKind)));
    }

    #[test]
    fn bad_taxonomy_regex_fails_open() {
        let dir = tempfile::tempdir().unwrap();
        let mut cfg = manual_cfg(dir.path());
        cfg.kinds = KindTaxonomy::default_taxonomy();
        cfg.kinds.kinds[0].rules.push(crate::types::Rule {
            any_of: vec![crate::types::Matcher::BodyRegex("(".into())],
        });
        assert!(matches!(spawn(cfg), Err(ClogError::Corrupt { .. })));
    }

    #[test]
    fn changes_are_adds_then_removes_each_key_ordered() {
        let previous: OrdMap<String, String> =
            [("b".to_string(), "bee".to_string()), ("c".to_string(), "cee".to_string())].into_iter().collect();
        let current: OrdMap<String, String> =
            [("a".to_string(), "ay".to_string()), ("c".to_string(), "cee2".to_string())].into_iter().collect();
        let rendered: Vec<String> = changes_since(Some(&previous), &current)
            .iter()
            .map(|c| match c {
                ChangeItem::Added(h) => format!("+{h}"),
                ChangeItem::Removed(h) => format!("-{h}"),
            })
            .collect();
        // "c" is in both: a changed headline is not a membership change.
        assert_eq!(rendered, vec!["+ay".to_string(), "-bee".to_string()]);
        assert!(changes_since(None, &current).len() == 2);
    }
}
