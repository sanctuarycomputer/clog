# Clog v1 — Implementation Specification

**Status:** ready for implementation
**Deliverable:** a Rust library crate (`clog`), no binary, no network surface
**Out of scope for this repo:** MCP server, language bindings, connectors, any LLM calls
**Intended engine:** Bog / Fold (Flower Computer). A naive reference engine ships alongside it as the semantic oracle and interim runtime (see §7).

---

## 1. Purpose

Clog is an orientation engine for agentic systems. Hosts write structured **claims** (pre-interpreted observations about the world); Clog maintains **materialized views** over them incrementally and renders a token-budgeted **situation** document per **scope** (lens). Agents read the situation as context at turn time with zero retrieval work, subscribe to **wakes** when the world changes in ways they declared relevant, and close the loop with **corrections** that improve Clog's cheap local classifier.

One sentence: connectors sense and interpret, Clog believes and ranks, agents read and act.

Clog is generic across organizations. A design agency, a household device, and an open-source project all reduce to the same primitives: entities (people, projects, agreements, assets), claims about them (facts, risks, decisions, questions, commitments), and lenses that rank what matters right now.

What Clog is not: a memory store queried at turn time (Zep, Mem0), a document/episode store, an extraction pipeline, or an agent framework. It never calls a model. It owns algebra, not judgment.

---

## 2. Vocabulary

| Term | Meaning |
|---|---|
| Claim | The unit of input. A structured, pre-interpreted assertion about the world, carrying identity, provenance, bitemporal timestamps, and graded trust. |
| claim_key | Stable identity of a claim. Writing a claim with an existing key supersedes the prior version (upsert = retract old + assert new). |
| subject_key | Optional grouping key naming *what the claim is about* (e.g. `halcyon:inv-1042:status`). Claims sharing a subject_key compete in belief resolution. |
| Entity | A typed reference (`etype`, `id`, optional display name) a claim concerns. Free-form types; suggested defaults exist. |
| Kind | The claim's classification (risk, decision, question, commitment, fact, ...). Derived, never stored on the claim. Taxonomy is config-declared. |
| Scope | A named lens. Each scope has a Focus and its own materialized ranking and situation document. Scopes are concurrent; all are warm simultaneously. |
| Focus | The ranking parameters of a scope: kind weights, entity boosts, half-life. Data, not code. |
| Situation | The rendered, budgeted orientation document for one scope. A read of it is a snapshot copy, never a computation. |
| rev | Monotonic u64. Global rev bumps per committed write batch. Each scope's situation carries the global rev at which its text last changed. |
| Wake | A push notification that a watched view changed materially, carrying the diff and the rev. |
| Exemplar | A labeled example (body, kind) used by the built-in kNN classifier. Stored as a claim in the reserved `clog:` namespace. |
| Merge | An entity-alias assertion. Stored as a claim in the reserved namespace; retracting it un-merges. |

---

## 3. Design invariants

These are numbered because tests reference them. Every invariant must have at least one test.

- **INV-1 Write-time work, read-time zero.** `situation()` returns a pre-rendered string (an `Arc` clone of the current snapshot). No ranking, searching, or rendering occurs on the read path. Same for `select` (snapshot iteration only).
- **INV-2 Judgments are views.** The stored claim carries no kind, salience, status, or ttl. All such properties are derived and live in views. Changing derivation logic plus replaying the log re-derives the world.
- **INV-3 Retraction heals everything.** For any claim c: `observe([c]); retract(c.key)` leaves every view and every situation text identical to never having observed c. (rev counters may differ; text may not.)
- **INV-4 Upsert is supersession.** `observe([c1]); observe([c2])` with equal keys is view-equivalent to `observe([c2])` alone.
- **INV-5 Idempotence.** Re-observing a byte-identical claim (same content hash) produces zero view diffs and no situation re-render.
- **INV-6 Provenance is revocable.** `revoke_observer(o)` is view-equivalent to retracting every live claim whose observer == o.
- **INV-7 Beliefs are global, rankings are scoped.** Belief resolution (entity_state) is identical in every scope. Only ordering/membership of `urgent` and the rendered situation differ per scope.
- **INV-8 Reserved namespace is invisible.** Claims whose claim_key starts with `clog:` never appear in live, urgent, open_loops, entity_state, recall results, or any rendered situation. External `observe` of a `clog:`-prefixed key is rejected.
- **INV-9 rev monotonicity.** Global rev strictly increases per committed batch and survives restart. A situation's rev never decreases. Wakes are delivered in rev order per watch.
- **INV-10 Determinism.** Given the same event log and the same manual clock, all views, situations, and revs are byte-identical across runs and across engines (naive vs fold). No wall-clock reads outside the Clock port. No HashMap iteration order may leak into any output (use ordered structures or explicit sorts at every output boundary).
- **INV-11 Crash safety.** Clog owns an append-only event WAL as the source of truth. Engine state (including the Fold db and semantic index) is a rebuildable cache. After a crash at any point, reopen restores a state equal to replaying the WAL prefix that was durably committed.
- **INV-12 Serializability of the surface.** Every public API type derives `serde::{Serialize, Deserialize}`. No closures, trait objects, or lifetimes in the public surface (the MCP project binds on top of this).
- **INV-13 Anonymous lenses are impossible.** Ranking parameters enter only via `set_focus(scope, focus)` or `Config.scopes`. `situation()` accepts only a scope name. Lenses are cheap and creatable at runtime, but they have names, stable revs, and watchability.

---

## 4. Public API

Thirteen functions. This is the complete v1 surface; anything not listed here is internal.

```rust
pub struct Clog; // cheap-clone handle: Clone + Send + Sync

impl Clog {
    pub fn open(cfg: Config) -> Result<Clog, ClogError>;

    // ---- WRITE (one committed batch per call, one rev bump) ----
    pub fn observe(&self, claims: Vec<Claim>, opts: ObserveOpts) -> Result<Ack, ClogError>;
    pub fn retract(&self, claim_key: &str) -> Result<Ack, ClogError>;
    pub fn revoke_observer(&self, observer: &ObserverId) -> Result<Ack, ClogError>;

    // ---- READ (snapshot only; INV-1) ----
    pub fn situation(&self, scope: Option<&str>, template: Option<&str>)
        -> Result<Situation, ClogError>;
    pub fn select(&self, view: View, filter: Filter) -> Result<Vec<Row>, ClogError>;
    pub fn recall(&self, query: &str, k: usize) -> Result<Vec<Hit>, ClogError>;

    // ---- ATTENTION ----
    pub fn set_focus(&self, scope: &str, focus: Focus) -> Result<Ack, ClogError>;
    // unknown scope name = declare-and-materialize; known = re-rank that lens

    // ---- WAKE ----
    pub fn watch(&self, spec: WatchSpec) -> Result<WatchId, ClogError>;
    pub fn wakes(&self) -> crossbeam_channel::Receiver<Wake>;
    pub fn unwatch(&self, id: WatchId) -> Result<(), ClogError>;

    // ---- LEARN ----
    pub fn correct(&self, claim_key: &str, judgment: Judgment) -> Result<Ack, ClogError>;
    // fixes the kind for that claim AND appends an exemplar claim (clog: namespace)
    pub fn merge_entities(&self, alias: &EntityRef, canonical: &EntityRef)
        -> Result<Ack, ClogError>;
    // writes a merge claim clog:merge:{alias}->{canonical}; retract() of that key un-merges
}
```

### 4.1 Data types

```rust
#[derive(Clone, Serialize, Deserialize)]
pub struct Claim {
    pub claim_key: String,             // identity; upsert key; <= 256 bytes; not clog:*
    pub subject_key: Option<String>,   // belief-competition group; <= 256 bytes
    pub source_ref: String,            // provenance URI/id, links out; <= 1024 bytes
    pub observer: ObserverId,          // "gmail-v3", "twist-v1", ...
    pub schema_v: u16,                 // stored, not interpreted in v1
    pub occurred_at: u64,              // unix ms, when true in the world
    pub observed_at: u64,              // unix ms, when sensed
    // recorded_at is NOT caller-supplied; Clog assigns it at commit
    pub reliability: Reliability,      // A..F (Admiralty source grade)
    pub credibility: Credibility,      // One..Six (1 = confirmed, 6 = cannot judge)
    pub entities: Vec<EntityRef>,      // <= 32
    pub body: String,                  // <= 16 KiB, the human-readable assertion
}

#[derive(Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct EntityRef {
    pub etype: String,                 // free-form: "person", "project", "agreement", ...
    pub id: String,
    pub name: Option<String>,          // display; latest-seen wins in the registry
}

pub struct ObserveOpts { pub return_situation: Option<String> } // scope name

pub struct Ack { pub rev: u64, pub situation: Option<Situation> }

pub struct Situation { pub scope: String, pub text: String, pub rev: u64, pub as_of: u64 }

#[derive(Serialize, Deserialize)]
pub enum View { Live, EntityState, OpenLoops, Urgent { scope: String }, Unclassified }

#[derive(Default, Serialize, Deserialize)]
pub struct Filter {
    pub kinds: Option<Vec<String>>,
    pub entities: Option<Vec<EntityRef>>,     // matches on (etype,id), post-alias
    pub observer: Option<ObserverId>,
    pub subject_prefix: Option<String>,
    pub occurred_after: Option<u64>,
    pub min_score: Option<f32>,               // valid only with View::Urgent
    pub limit: Option<usize>,                 // default 50, max 500
}

pub struct Row {
    pub claim: Claim, pub recorded_at: u64,
    pub kind: Option<KindLabel>,              // KindLabel { kind, confidence, source }
    pub score: Option<f32>,                   // present for Urgent
    pub believed: Option<bool>,               // present when subject_key set
}

pub struct Hit { pub claim_key: String, pub distance: f32, pub headline: String }

#[derive(Serialize, Deserialize)]
pub struct Focus {
    pub weights: BTreeMap<String, f32>,       // kind -> weight; missing kind = 1.0
    pub boosts: Vec<(EntityRef, f32)>,        // multiplicative per matched entity
    pub half_life_days: f32,                  // default 7.0
    pub top_k: Option<usize>,                 // overrides Config.top_k for this scope
}

#[derive(Serialize, Deserialize)]
pub struct WatchSpec {
    pub scope: String,
    pub view: View,
    pub filter: Filter,                       // declarative predicate (INV-12)
    pub debounce_ms: u64,                     // default 500
}

pub struct Wake {
    pub watch_id: WatchId, pub scope: String, pub rev: u64,
    pub added: Vec<WakeItem>, pub removed: Vec<WakeItem>,
}
pub struct WakeItem { pub claim_key: String, pub kind: Option<String>, pub headline: String }
// headline = first 120 chars of body, whitespace-collapsed

pub struct Judgment { pub kind: String }      // must be in the configured taxonomy
```

Notes:
- `observe` takes a `Vec<Claim>` so connector bursts commit as one batch, one rev, one re-render pass. A batch of one is the common case and is fine.
- Watches are ephemeral: not persisted, hosts re-register after `open`. Document this in rustdoc.
- There is no `unmerge`, `unlearn`, or scope-delete call. Merges and exemplars are reserved-namespace claims, so `retract` covers the first two. Scope GC is a non-goal in v1 (§13); warn in rustdoc against unbounded dynamic scope creation.

---

## 5. Semantics

### 5.1 Write path

Every write call becomes one `Batch` of internal `Event`s, appended to the WAL, then applied to the engine, then post-processed (classify, re-render, wake evaluation), atomically from the reader's point of view (readers see the pre-batch snapshot until the post-batch snapshot is swapped in).

```rust
enum Event {
    Observe(Claim /* recorded_at filled */),
    Retract { claim_key: String },
    Revoke { observer: ObserverId },
    SetFocus { scope: String, focus: Focus },
    Judge { claim_key: String, kind: String, confidence: f32, source: JudgeSource },
    Tick { epoch: u64 },                      // decay-bucket epoch, see 5.5
}
enum JudgeSource { Rule, Knn, External }      // correct() emits External
```

`observe` internals, per claim, in order:
1. Validate (§10). Reject the whole batch on any invalid claim (atomic batches; no partial commit).
2. Assign `recorded_at` from the Clock port.
3. Compute content hash over all fields except recorded_at. If a live claim with the same claim_key has an equal hash, skip entirely (INV-5).
4. If a live claim with the same claim_key exists with a different hash, emit retraction of the old version then assertion of the new (INV-4).
5. After engine apply, run the classifier cascade (5.6) for claims lacking a confident kind; confident results append `Judge` events to the same batch before render.

`retract` on an unknown or already-retracted key returns `ClogError::UnknownClaim` (idempotent hosts can ignore it).

`revoke_observer` expands, at apply time, to retractions of every live claim with that observer, including reserved-namespace claims it wrote. Requires an observer-to-live-keys index view (or engine scan; see F8).

### 5.2 Alias resolution (merge_entities)

A merge writes claim `clog:merge:{alias.etype}:{alias.id}->{canonical.etype}:{canonical.id}` with the two refs in the body (JSON). Semantics:
- Stored alias edges are always depth-1. If `canonical` is itself currently aliased to c2, the edge written is alias -> c2 (write-time flattening). This keeps the canonical map non-recursive: no transitive closure is ever computed in a view.
- Merging in the other direction later (b -> a after a -> b) is rejected with `ClogError::AliasCycle`.
- The canonical map is a view: alias(etype,id) -> canonical(etype,id). Every view that groups or filters by entity resolves through this map. Retracting the merge claim removes the edge; grouped views re-key accordingly (this is the expensive retraction; it is proportional to claims touching the alias, which is acceptable).
- Entity display names: registry keeps the latest-seen non-null `name` per canonical entity (ordered by recorded_at).

### 5.3 Belief resolution (entity_state)

Applies only among live claims sharing a `subject_key` (post-alias). The believed claim is selected by this total order:

1. later `occurred_at` wins;
2. tie: better `reliability` (A > B > ... > F);
3. tie: better `credibility` (1 > 2 > ... > 6);
4. tie: later `recorded_at`;
5. tie: lexicographically larger claim_key (final deterministic tiebreak).

Claims with `credibility` worse than `Config.belief.min_credibility` (default: Six allowed, i.e. no floor) are excluded from winning unless they are the only live claim for the subject. Losing claims remain live (they appear in Live and can be selected/recalled) but `believed == false`.

Known limitation, accepted for v1: a fresh low-trust claim can override an older high-trust one. Mitigations are host-side (`retract`, `correct`) plus the config floor. Document prominently.

`EntityState` view rows: for each canonical entity, the believed claim per subject_key plus (for rendering) up to N recent believed/standalone claims (N = 8, internal constant).

### 5.4 Scoring and urgency

For scope s with focus f, for each live, non-reserved claim c with kind k (unclassified claims score with kind weight 1.0):

```
score(c, s) = weight(f, k) * trust(c) * recency(c) * boost(f, c)

weight(f,k)  = f.weights.get(k).unwrap_or(1.0)
trust(c)     = REL[c.reliability] * CRED[c.credibility]
  REL:  A=1.00 B=0.90 C=0.75 D=0.50 E=0.25 F=0.10
  CRED: 1=1.00 2=0.90 3=0.75 4=0.50 5=0.25 6=0.10
recency(c)   = 0.5 ^ (bucket_age(c) / f.half_life_days)   // bucketed, see 5.5
boost(f,c)   = product of factor for every (entity, factor) in f.boosts
               whose entity matches any of c.entities post-alias (else 1.0)
```

`Urgent{scope}` = top K live claims by score, K = focus.top_k or Config.top_k (default 12). Ordering ties broken by claim_key. Scores are recomputed only on diffs: claim changes, focus changes, alias changes, kind changes, and bucket-crossing ticks.

### 5.5 Clock, decay buckets, ticks

All time comes from the Clock port (`System` or `Manual`; Manual is mandatory in tests, INV-10). Recency is quantized: `bucket_width_days = half_life_days / buckets_per_half_life` (config, default 4). `bucket_age(c) = floor(age_days / bucket_width) * bucket_width + bucket_width/2` (bucket midpoint).

The tick driver (interval = Config.tick.interval, default 60s; or manual `advance()` on the test handle) emits a `Tick{epoch}` event only when at least one live claim crosses a bucket boundary in at least one scope, and the engine re-scores only the crossing claims. A tick that changes no bucket is a no-op and must not bump rev or re-render (bench B4 asserts this).

### 5.6 Kinds: the classifier cascade

Taxonomy is config-declared. Default:

```
fact, decision, risk, question, commitment, agreement, opportunity, fyi
```

Each kind may declare seed rules and seed exemplars in config. Cascade per unclassified claim:

1. **Rules tier** (free): first matching rule wins with confidence 1.0. Rule = `{ any_of: Vec<Matcher> }`, Matcher = `BodyContains(str, case_insensitive)` | `BodyRegex(str)` | `ObserverIs(str)` | `EntityType(str)`. Evaluated in config order.
2. **kNN tier** (cheap, requires `semantic` feature): ESE-embed the body, query the exemplar index for k=Config.knn.k (5) nearest with cosine similarity >= min_sim (0.80). If >= min_votes (4) agree on one kind, assign it with confidence = votes/k.
3. **No confident result:** the claim lands in the `Unclassified` view with any partial vote attached. That view is the escalation queue: hosts watch it, classify upstream (their LLM), and resolve via `correct()`. Clog itself never escalates and never calls a model.

`correct(claim_key, judgment)` emits `Judge{source: External, confidence: 1.0}` (overriding any prior kind) and appends an exemplar claim `clog:exemplar:{hash(body)}` whose body is the corrected claim's body and whose judgment is stored in a `kind` field of its JSON body. Exemplar claims feed only the kNN index (INV-8 keeps them out of everything else). Retracting an exemplar claim unlearns it.

### 5.7 Views (complete v1 list)

| View | Definition | Keyed by |
|---|---|---|
| live | current versions of non-retracted, non-reserved claims | claim_key |
| kinds | claim_key -> KindLabel (kind, confidence, source) | claim_key |
| unclassified | live minus confidently-kinded | claim_key |
| entity_state | believed claim per (canonical entity, subject_key) + registry (names) | entity |
| open_loops | live claims whose kind is in Config.loop_kinds (default: question, risk, commitment) | claim_key |
| urgent[scope] | top-K by score(claim, scope) | scope, rank |
| semantic index | ESE vector per live claim body + exemplar store (ANNy) | claim_key |
| changes[scope] | membership delta of (urgent ∪ open_loops) between the last two rendered revs of the scope | scope |

`recall(query, k)`: embed query with ESE, ANNy search over live-claim vectors (exemplars excluded), return up to k Hits ordered by ascending distance. `recall` is the one deliberate pull in the design (warm tier); everything else is push.

### 5.8 Rendering and templates

The renderer is deterministic. A template is a UTF-8 string passed by value per call (hosts own and version their templates; Clog stores none). Grammar:

```
template   := ( text | slot )*
slot       := "%{" name ( WS+ key "=" value )* "}"
name       := "header" | "urgent" | "open_loops" | "entities" | "changes"
key        := "limit"            (usize; per-slot cap)
```

Unknown slot names or malformed slots are a `TemplateError` at call time. The default template (used when `template == None`, and baked into the crate):

```
# situation · scope: %{header}

## urgent
%{urgent limit=8}

## open loops
%{open_loops limit=10}

## entities
%{entities limit=10}

## changes since last brief
%{changes limit=6}
```

Slot renderings (exact formats are frozen by golden tests, §11.4):
- header: `{scope} · rev {rev} · {as_of RFC3339}`
- urgent item: `{rank}. ({score:.1}) {headline} [{reliability}/{credibility}] ({claim_key})`
- open_loops item: `- {KIND} {headline} ({claim_key})`
- entities item: `{display or etype:id}: {believed subject summaries, "; "-joined, newest first}`
- changes item: `+ {headline}` / `- {headline}`

Budgeting: render all slots, then if total chars > Config.budget_chars (default 6000), truncate whole items from the end of slots in reverse priority order (changes, entities, open_loops, urgent) until under budget, appending `… ({n} more)` per truncated slot. Never truncate mid-item.

Re-render policy per scope: after each batch (and material tick), re-render only if the scope's slot inputs changed (membership, order, or any rendered field). If the produced text differs from current, swap it in and set situation.rev = batch's global rev. Immaterial batches must not change any scope's rev (INV-5, B4).

### 5.9 Watches and wakes

After each committed batch, for each watch: compute the filtered membership of its view at pre- and post-snapshots; if the delta is non-empty, buffer it. Deltas buffer per watch and flush after `debounce_ms` of quiet (coalescing intermediate adds/removes; an item added then removed within the window cancels out). Wake.rev = the rev of the last batch in the coalesced window. Delivery: one unbounded crossbeam channel per Clog instance (`wakes()` returns a clone of the receiver); per-watch ordering by rev is guaranteed (INV-9); cross-watch ordering is not.

### 5.10 rev model

- `global_rev`: u64, +1 per committed batch (including SetFocus, Judge-carrying, and material Tick batches). Persisted in the WAL; restored on open.
- `Ack.rev` = global_rev of the caller's batch.
- `Situation.rev` = global_rev at which that scope's text last changed.
- Skew is expected and meaningful: `ack.rev > situation(s).rev` means the write did not affect s.

---

## 6. Architecture

### 6.1 Actor model and concurrency

- One writer thread owns the engine and the WAL. All write calls send a command over a bounded channel (`Config.write_queue`, default 1024) and block on a oneshot reply. Backpressure = blocking send.
- Readers never touch the engine. After each batch, the writer publishes an immutable `WorldSnapshot` via `arc_swap::ArcSwap`. `situation`, `select`, and `recall` read the current snapshot only (INV-1). `recall`'s ANNy index is part of the snapshot publication (copy-on-write handle or epoch-guarded read; the index must never be searched mid-mutation).
- The tick driver is a thread (System clock) or absent (Manual clock; the test handle exposes `advance(ms)` which injects Tick batches through the same writer channel).
- `Clog` handle: `Clone + Send + Sync`. Drop of the last handle shuts down threads cleanly (join, fsync WAL).

```
WorldSnapshot {
  rev, as_of,
  live: OrdMap<ClaimKey, StoredClaim>,
  kinds, unclassified, entity_state, open_loops,
  urgent: BTreeMap<ScopeId, Vec<(Score, ClaimKey)>>,
  situations: BTreeMap<ScopeId, Situation>,
  aliases: canonical map,
  semantic: Arc<SemanticIndex>,
}
```

### 6.2 Engine port (ports and adapters)

The IVM engine is behind a trait so Fold is a backend, not a foundation. This is the single most important architectural decision in the spec:

```rust
pub(crate) trait Engine: Send {
    /// Apply one batch of events; return per-view diffs sufficient for
    /// snapshot construction, wake evaluation, and re-render decisions.
    fn apply(&mut self, batch: &[Event], now_epoch: u64) -> ApplyResult;
    /// Full state for snapshot (re)construction, e.g. on open.
    fn dump(&self) -> EngineDump;
}
```

Backends:
- `engine::naive` (always compiled): in-memory ordered maps, full but *targeted* recomputation (only structures reachable from the batch's touched keys/scopes). This is the **semantic oracle**: its behavior *is* the spec. It must be simple enough to audit by eye.
- `engine::fold` (feature = "fold"): the Bog/Fold adapter. Correctness is defined as byte-equality of snapshots with `naive` under differential testing (§11.3). Ships only when the F-checklist (§8) is confirmed or fallbacks are implemented.

Because Clog owns the WAL (INV-11), engine state is disposable: `Config.rebuild_on_open = true` drops engine state and replays the WAL. This is also the mechanism for "re-orientation by replay" when classification config changes.

### 6.3 Persistence

- WAL: append-only file of length-prefixed bincode `Batch` records, each with a CRC32 and the assigned rev. fsync policy = `Config.wal.fsync` (default OnCommit). Torn tail records are truncated on open (R2).
- Snapshot file (naive engine only): periodically (every `Config.snapshot_every_batches`, default 512) the writer serializes `EngineDump` + rev; open = load latest valid snapshot, replay WAL tail.
- The Fold db (fold feature): durable per wtx (F7 confirmed). Each batch's wtx also writes the global rev to a meta record; reopen reads it and replays only the WAL tail beyond it. If the db fails to open, is corrupt, or its rev exceeds the WAL's (impossible under the write-ahead ordering below, hence treated as corruption), rebuild from WAL. Write-ahead ordering is mandatory: WAL append + fsync completes before engine apply; `wal_fsync = OnCommit` is required when the fold feature is enabled.
- Semantic index: rebuilt from live claims on open in v1 (embedding is fast; measure in B5). Persisting ANNy is a v2 optimization.

### 6.4 Module map

```
clog/
  src/lib.rs            public API, handle, actor wiring
  src/types.rs          Claim, EntityRef, Focus, Filter, ... (all serde)
  src/validate.rs       §10
  src/score.rs          pure: trust tables, recency buckets, score()
  src/belief.rs         pure: total order, floor
  src/alias.rs          canonical map + flattening + cycle check
  src/kinds.rs          taxonomy, rules tier, cascade driver
  src/render/           template parser + slot renderers + budgeter
  src/engine/mod.rs     Engine trait, Event, ApplyResult, EngineDump
  src/engine/naive.rs
  src/engine/fold.rs    feature "fold"
  src/semantic.rs       feature "semantic": ESE embed + ANNy wrapper + tombstones
  src/wake.rs           watch registry, delta buffering, debounce
  src/wal.rs            append, replay, snapshotting, CRC
  src/clock.rs          Clock port (System | Manual)
  src/actor.rs          writer loop, snapshot publication
  tests/                §11
  benches/              §11.6
```

Dependency policy: std + serde + thiserror + crossbeam-channel + arc-swap + bincode + crc32fast + regex (rules tier). Fold/ESE/ANNy as git dependencies behind features. Nothing else without a spec change. rustc stable, edition 2024.

---

## 7. Build order note for the implementing agent

Semantics land on the naive engine first. Do not begin `engine/fold.rs` until the golden simulation (§11.4) passes on naive. F4/F7/F11 answers are recorded below; log any further Flower answers in `docs/fold-answers.md` as they arrive. If a Fold capability is missing, implement the listed fallback *inside the adapter*, never by weakening the Engine trait or the invariants. The naive engine is not throwaway: it ships permanently as the differential oracle and the no-fold fallback runtime.

---

## 8. Bog / Fold dependency checklist (for the Flower Computer conversation)

The public docs demonstrate only: `Stream::new(path, graph)`, `FlatMap`, the `Bag` terminal, `wtx { insert / remove }`, `rtx` reads, and signed-diff semantics. **Status update:** the Flower Computer team has confirmed F4 (terminal trait implementable), F7 (durable on write), and F11 (ANNy deletion). Items below are marked accordingly; unconfirmed items retain fallbacks.

- **F1. Fan-out.** One input stream feeding many view branches (a `Tee`-like combinator). *Why:* every view hangs off one claim stream. *Fallback (now trivial given F4):* a single composite terminal that routes each incoming diff to all view-maintaining sub-structures internally. Prefer native fan-out if it exists; the composite terminal is architecturally equivalent.
- **F2. Keyed lookup inside wtx.** Fetch current record by key to implement upsert (retract old + assert new). *Why:* INV-4. *Fallback:* Clog maintains its own claim_key -> content-hash/version map outside Fold (it already must, for INV-5 hashing) and issues explicit remove(old)+insert(new); Fold never needs lookups.
- **F3. Stateful keyed reduces.** GroupBy + custom reduce (BestBelief, latest-name, TopK). *Why:* entity_state and urgent. *Fallback:* implement as custom terminals (F4) holding ordered state, consuming ±diffs.
- **F4. Public, implementable terminal trait. CONFIRMED by Flower Computer.** Custom terminals are the implementation vehicle for: SemanticIndex, SituationRender trigger, changes tracker, and every stateful reduce in F3/F5. This was the make-or-break item; it passed. Build all stateful views as terminals from the start rather than waiting on native GroupBy/TopK combinators.
- **F5. Joins, or parameterized re-scoring.** urgent = claims x focus. *Why:* set_focus re-ranks one lens. *Fallback:* focus lives in the urgent terminal's state; a SetFocus event triggers full re-score of that scope inside the terminal (small: it is one scope's live set).
- **F6. Multiple input types on one stream.** An `Event` enum as the stream item, or multiple Streams over one db file. *Why:* claims, ticks, judges, focus in one ordered log. *Assumption in this spec:* single stream of `Event`. Confirm enum items and per-variant routing are idiomatic.
- **F7. Crash and reopen semantics. CONFIRMED: durable on write.** Every committed wtx persists. Consequences for the adapter: (a) the fold db must store the last-applied global rev (a small meta record written in the same wtx as each batch); reopen = read that rev, replay only the WAL tail beyond it; (b) the snapshot-file machinery of §6.3 becomes naive-engine-only — the fold db *is* the snapshot; (c) **ordering constraint:** engine apply must never precede WAL durability for the same batch, otherwise a crash leaves the engine ahead of the source of truth. Therefore `wal_fsync = OnCommit` is mandatory when the fold feature is enabled (naive may use Interval; its tail loss is consistent because nothing else persisted those events).
- **F8. Predicate scan over stored items.** *Why:* revoke_observer expansion. *Fallback:* observer -> live-claim-keys index maintained as its own view (do this anyway; scanning is O(n)).
- **F9. Read/write concurrency.** Does rtx block wtx? Snapshot isolation? *Why:* actor design; we copy out to ArcSwap regardless, but need to know if copy-out must happen inside the write critical section.
- **F10. Cost model of remove+insert vs update.** *Why:* every upsert is a remove+insert; confirm no pathological amplification in downstream terminals.
- **F11. ANNy deletion. CONFIRMED: native remove exists.** Use it as the primary path; drop the tombstone-at-search-time machinery. Keep bench B5's churn assertion regardless — HNSW deletion can silently degrade graph connectivity and recall even where the API exists, and B5 is the only thing that would catch it. `semantic_rebuild_tombstone_ratio` survives as a contingency (default 1.0 = disabled): if B5 shows recall decay under churn, ratio-triggered rebuild is the escape hatch without an API change.
- **F12. ESE provenance.** License, training source of the embedding map, fixed vocabulary behavior on OOV tokens, stability of DIMENSIONS across versions (persisted vectors), quantization features. *Why:* shipping in a commercial device later; index compatibility.
- **F13. Batch ergonomics and backpressure.** Cost of a 1k-insert wtx; any size limits.
- **F14. Recursion.** Confirm *not needed* is acceptable long-term: v1 avoids it by design (depth-1 alias flattening, no graph closure). Ask what their roadmap is for iterative computation anyway (v2 relations).

---

## 9. Configuration reference

```rust
pub struct Config {
    pub path: PathBuf,                          // directory; Clog creates wal/, snap/, fold.db
    pub scopes: BTreeMap<String, Focus>,        // "default" injected if absent (uniform Focus)
    pub kinds: KindTaxonomy,                    // default taxonomy of §5.6 if empty
    pub loop_kinds: Vec<String>,                // default ["question","risk","commitment"]
    pub top_k: usize,                           // 12
    pub budget_chars: usize,                    // 6000
    pub tick: TickConfig,                       // { mode: System|Manual, interval: 60s }
    pub decay_buckets_per_half_life: u32,       // 4
    pub belief_min_credibility: Credibility,    // Six (no floor)
    pub knn: KnnConfig,                         // { k:5, min_votes:4, min_sim:0.80 }
    pub semantic_enabled: bool,                 // true (feature-gated)
    pub semantic_rebuild_tombstone_ratio: f32,  // 1.0 = disabled (contingency; native delete is primary)
    pub wal_fsync: FsyncPolicy,                 // OnCommit
    pub snapshot_every_batches: u64,            // 512
    pub write_queue: usize,                     // 1024
    pub rebuild_on_open: bool,                  // false
}

pub struct KindTaxonomy { pub kinds: Vec<KindDef> }
pub struct KindDef {
    pub name: String,
    pub rules: Vec<Rule>,                       // §5.6 matchers
    pub seed_exemplars: Vec<String>,            // bodies labeled with this kind
}
```

`Config::default_for(path)` gives a working single-scope instance. The crate includes two example configs proving genericity: `examples/agency.rs` (kinds and scopes for a client-services studio) and `examples/household.rs` (a home device: chores as commitments, appliances as entities, per-member scopes).

---

## 10. Validation rules (observe-time; whole batch rejected on first failure)

- claim_key: non-empty after trim, <= 256 bytes, must not start with `clog:` (INV-8), no control chars.
- subject_key/source_ref: same char rules; <= 256 / <= 1024 bytes. source_ref must be non-empty.
- observer: non-empty, <= 128 bytes.
- body: non-empty after trim, <= 16 KiB.
- entities: <= 32; each etype and id non-empty, <= 128 bytes.
- timestamps: occurred_at and observed_at > 0. Values beyond now + 24h are clamped to now for scoring but stored verbatim (log truth; warn via tracing). occurred_at > observed_at is allowed (predictions/backdated corrections) and not warned.
- Judgment.kind and Focus.weights keys must exist in the taxonomy; Focus values must be finite and > 0; half_life_days in (0.01, 3650).
- Template: parse errors reject the call only (never poison state).

Error taxonomy (`thiserror`): `InvalidClaim { index, reason }`, `ReservedNamespace`, `UnknownClaim`, `UnknownScope`, `UnknownKind`, `AliasCycle`, `TemplateError`, `Storage(io)`, `Corrupt { detail }`, `ShuttingDown`.

---

## 11. Testing plan

Testing is the spec's enforcement mechanism. Every INV maps to at least one named test. Framework: `cargo test` + `proptest` + `insta` (golden) + `criterion` (bench) + `cargo-fuzz` (parser). All tests run with `Clock::Manual` (INV-10). CI gates: all of §11.1–11.5 green, benches within budgets on the reference machine, `cargo doc` clean, no `unwrap` outside tests.

### 11.1 Unit (pure functions; table-driven)

- U-SCORE-1: score() against a fixed table of (kind, rel, cred, age, focus) -> expected value, incl. bucket midpoints and boost stacking.
- U-BELIEF-1: total order of §5.3 across permuted inputs; U-BELIEF-2: credibility floor incl. only-claim exception.
- U-ALIAS-1: write-time flattening; U-ALIAS-2: cycle rejection; U-ALIAS-3: retraction of a merge re-keys grouped views.
- U-TMPL-1: grammar accept/reject table; U-TMPL-2: budget truncation order and `… (n more)` markers; U-TMPL-3: default template byte-stability.
- U-VAL-1: every rule of §10, positive and negative.
- U-KIND-1: rule tier ordering; U-KIND-2: kNN vote thresholds incl. tie at min_votes-1 -> Unclassified.

### 11.2 Property-based (proptest; naive engine; each property also asserts INV-9/10)

Claim generator: arbitrary valid claims over a small alphabet of entities/observers/subjects so collisions occur.

- P1 (INV-3): random interleave of observes then retract-all == empty world (situation text per scope equals empty-world render).
- P2 (INV-4): for random claim sequences with shared keys, final views depend only on the last version per key.
- P3 (INV-5): duplicate any prefix of a sequence; snapshots byte-equal; scope revs unchanged by duplicates.
- P4 (INV-6): revoke(o) == retract every live claim of o, snapshot-equal.
- P5 (INV-7): for random multi-scope configs, entity_state identical across scopes while urgent orderings differ only per focus.
- P6 (order-insensitivity of belief): shuffling arrival order of claims sharing a subject_key never changes the winner (occurred_at et al. fixed).
- P7 (merge round-trip): observe C with entity a; merge a->b; retract merge == never merged, snapshot-equal.
- P8 (wake soundness/completeness): for random watch filters, the set of coalesced wake items equals the membership delta between the watch's first and last snapshot in the window; no wake from immaterial batches.

### 11.3 Differential (the fold gate)

- D1: generate 10k-event random sequences (all Event variants, manual ticks); apply to naive and fold engines; assert byte-equal WorldSnapshots after every batch. Run in CI with fixed seeds + nightly with random seeds. **fold feature cannot merge while D1 fails.**
- D2: crash-point differential — replay the same WAL prefix into a fresh naive engine vs a reopened fold-backed instance; snapshots equal (pairs with R-tests).

### 11.4 Golden simulation (insta snapshots)

- G1: the "client-services studio" fixture: 10 claims across three clients (a slipping deliverable, an overdue invoice observed by two sources then superseded by a bank-feed payment claim with earlier occurred_at, an inbound lead, a PTO fact colliding with a moved kickoff, a client question later self-resolved via retraction, an ambiguous upsell that lands in Unclassified and is resolved via correct()), two focus changes (`delivery-health` -> `cash-and-collections`). Snapshot the situation text at four checkpoints (A–D) plus the changes slot and Unclassified contents. This fixture doubles as `examples/agency.rs`.
- G2: household fixture on a second scope set (proves genericity; one shared worldview, two lenses with disjoint boosts).
- Golden files are the frozen rendering contract of §5.8; changing them requires a spec edit.

### 11.5 Recovery and concurrency

- R1: kill the process (abort) between WAL append and snapshot publication at randomized points across G1; reopen; state equals WAL replay (INV-11).
- R2: torn final WAL record (truncate mid-record, corrupt CRC); reopen succeeds at prior rev; corrupted tail quarantined to `wal.corrupt`.
- R3: rebuild_on_open=true equals normal open, snapshot-equal.
- C1: N writer threads x M reader threads hammering observe/situation/select for 10s under `--cfg loom` for the ArcSwap publication path (readers never observe a partial snapshot); plus a plain stress test asserting rev monotonicity and no deadlock at write_queue saturation.

### 11.6 Benchmarks (criterion; budgets on an M-series laptop, release, semantic on)

- B1: observe batch=1 p99 < 2 ms at 100k live claims (excluding fsync; report both fsync policies).
- B2: observe batch=1000 completes < 250 ms at 100k live.
- B3: situation()/select() p99 < 50 µs regardless of world size (they are snapshot reads; this bench exists to catch INV-1 regressions).
- B4: no-crossing tick cost < 100 µs and zero rev bumps over 1h of simulated ticks on a quiet world.
- B5: semantic — index rebuild of 100k claims < 20 s; recall@10 vs brute-force cosine >= 0.95 on a synthetic corpus, re-measured after 50% churn using native ANNy deletion (guards the F11 confirmation; recall decay here re-activates the ratio-rebuild contingency).
- B6: reopen (snapshot + 511-batch WAL tail) < 1 s at 100k live.

### 11.7 Fuzz

- Z1: template parser (cargo-fuzz, arbitrary bytes; must never panic).
- Z2: WAL reader against arbitrary file corruption (must never panic; must never apply a record failing CRC).

---

## 12. Milestones and exit criteria

- **M0 — types and pure core.** types.rs, validate.rs, score.rs, belief.rs, alias.rs, template parser. Exit: §11.1 green.
- **M1 — naive engine, renderer, WAL.** Views of §5.7 (minus semantic), rendering, batching, rev model, persistence. Exit: P1–P7, G1, R1–R3 green.
- **M2 — scopes, focus, ticks, wakes.** set_focus declare-or-steer, bucketed decay, watch/debounce/coalesce. Exit: P5, P8, B3, B4 green; G1 extended with the focus-shift checkpoints.
- **M3 — semantic + learn.** ESE/ANNy wrapper, tombstones, recall, kNN tier, correct()/exemplars, Unclassified queue. Exit: U-KIND-*, B5 green; G1 exercises the correct() path.
- **M4 — fold adapter.** Behind `feature = "fold"`. F4/F7/F11 are confirmed (build terminals, rev-meta record, native deletion directly); F1/F2/F6/F9 fallbacks apply only if surprises surface. Exit: D1, D2 green; B1/B2 re-run on fold and recorded (note the double-fsync cost under OnCommit).
- **M5 — hardening and freeze.** Fuzz targets, loom pass, rustdoc on every public item with examples, examples/ compile as doctests, CHANGELOG, API freeze tag `v0.1.0`.

Definition of done for the crate: a host can, in under 30 lines, open Clog, declare two scopes, observe the G1 fixture, read two different situations, receive a wake on an invoice risk, correct a misclassification, and watch the situation heal after a retraction — with the fold feature off.

---

## 13. Non-goals (v1) and parking lot (v2)

Explicit non-goals: episode/raw-artifact storage and extraction (upstream), any model invocation, `Claim::Relation` / typed edges / k-hop neighborhoods, entity-resolution *proposals* (only explicit merge_entities; auto-suggest is v2), scope deletion/GC, retention policies, multi-process or networked access, authn/z, token-exact budgeting (chars only), template storage, persisted watches, ANNy persistence.

Parking lot, in likely order: relations + neighborhood view (needs F14 answer), auto-suggested merges from ESE similarity, per-scope claim visibility filters (the privacy lens), WAL compaction/claim TTL at the log level, replay-with-new-taxonomy tooling, Feldera adapter as a second Engine impl.

---

## 14. Open questions with chosen defaults (implementation never blocks on these)

1. Trust/decay constants (§5.4) are unvalidated guesses -> ship as specified, mark `#[doc = "tunable"]`, revisit with first-tenant data.
2. Belief order is time-first (§5.3 limitation) -> ship with config floor; revisit if a tenant hits it.
3. Wake channel is process-global -> revisit per-watch channels if the MCP layer wants isolation.
4. Char budgeting vs tokens -> chars in v1; the binding layer may pass model-aware budgets later.
5. `Unclassified` claims score with weight 1.0 -> alternative (configurable default weight) if unclassified spam drowns urgent.

— end of spec —
