# Clog build design

**Date:** 2026-08-15
**Status:** approved in brainstorm; delta on [`docs/clog-spec-v1.md`](../../clog-spec-v1.md)
**Scope:** this document records every decision made on top of the v1 spec. Where it
amends the spec, the amendment is called out explicitly. The spec remains the
authority for everything not mentioned here.

## 1. Goal and posture

Real v1 library, built in the spec's order: pure core → naive engine (semantic
oracle) → scopes/wakes → semantic/learn → fold adapter → hardening. Hackathon
timing is secondary to correctness; the naive engine ships permanently as the
differential oracle and no-fold runtime.

## 2. Crate placement

- `examples/clog/` in this workspace (README convention), **library crate**
  (`[lib]`, no binary), `publish = false`, edition 2024. Auto-wired via the
  `examples/*` workspace glob.
- The spec's `examples/agency.rs` and `examples/household.rs` are cargo examples
  inside the crate. `fuzz/` subdir holds the cargo-fuzz targets (Z1, Z2).
- Path deps: `fold` (optional, feature `fold`), `ese` + `anny` (optional, feature
  `semantic`; ese features `dim-512`, `quant-8`, so `DIMENSIONS = 512`).
- Default features: `semantic` on, `fold` off — matching the spec's definition of
  done. Day-to-day iteration uses `--no-default-features` (ese's embedded map
  dominates compile time); CI builds both.

## 3. Dependency amendments (spec §6.4 policy)

- **Add `imbl`**: persistent `OrdMap`/`OrdSet` with O(1) clone for `WorldSnapshot`
  publication. A full `BTreeMap` deep-clone per batch would blow the B1 budget at
  100k live claims.
- **Replace `bincode` with `postcard`** for WAL record encoding: the entire
  workspace already speaks postcard, its wire format is stable and documented, and
  it avoids bincode 2.x API churn. Length-prefix + CRC32 framing is clog's own,
  unchanged.
- Everything else per spec: serde, thiserror, crossbeam-channel, arc-swap,
  crc32fast, regex. Dev-deps: proptest, insta, criterion; loom behind `--cfg loom`.

## 4. WAL and event-log semantics (spec §5.1/§6.3 clarification)

**The WAL records effects, not intentions.** A committed batch contains the
caller's events *plus* classifier-derived `Judge` events *plus* any `Tick`
events, exactly as applied. Replay — including `rebuild_on_open` — is pure event
application; **the classifier cascade never runs during replay**. This makes
INV-10 trivially statable (same WAL bytes → same world) and immune to exemplar
drift. "Replay with new classification config" is the v2 tooling the spec's
parking lot already names, not a v1 behavior.

Per-batch order: validate → assign `recorded_at` → run cascade against the
pre-batch snapshot → assemble the full event list → WAL append + fsync → engine
apply → render/wake evaluation → publish snapshot.

## 5. Naive engine internals

- Engine state is `imbl` maps throughout: primary (`live`, `kinds`, `aliases`) plus
  secondary indexes the semantics require (`by_subject`, `by_observer` [F8],
  `by_entity`), plus derived views (`entity_state`, `open_loops`, `unclassified`,
  per-scope `urgent`, decay-bucket occupancy per scope so ticks cost
  O(crossing claims)).
- **Diffs are recorded during apply, not computed by snapshot comparison.**
  `apply()` updates touched structures and appends to
  `ApplyResult { per-view added/removed/changed, scopes_dirty }`. Targeted
  recomputation: belief only for the touched subject group, scoring only for
  changed keys, rendering only for scopes whose slot inputs changed. Alias
  retraction re-keys only `by_entity[alias]` (the spec's accepted expensive case).
- Spec §5.9's pre/post membership delta is the *semantic definition*; the
  diff-driven implementation is verified against it by property test P8.
- **`EngineDump` = `WorldSnapshot`** (plus secondary indexes). No parallel dump
  type.
- **Naive snapshot-file machinery is deferred to M5.** Until then, reopen = full
  WAL replay. B6 (reopen bench) is an M5 gate anyway; R1/R2 still fully exercise
  the WAL. `snapshot_every_batches` config lands with the machinery.
- **`changes[scope]` falls out of the render pass**: the renderer already diffs
  slot inputs to decide re-rendering; that delta *is* the changes slot. One
  mechanism, not two.
- All semantics live in shared pure modules (`score.rs`, `belief.rs`, `alias.rs`,
  `kinds.rs`, `render/`); both engines call the same functions, so differential
  testing checks orchestration, not two formula copies.

## 6. Fold adapter shape (M4)

- One `Stream<Event>` (not `KeyedStream`: batches are heterogeneous). Per-variant
  `FilterMap` routes into a tuple fan-out of custom `Push` terminals: live table,
  observer/subject/entity indexes, belief (per subject group), per-scope urgent
  (holds focus + bucket state; consumes Observe/Judge/SetFocus/Tick), open-loops,
  unclassified, and the semantic branch reusing fold's `Hnsw` terminal over an ese
  `Map`.
- Each custom terminal buffers its "emitted this tx" delta; the adapter drains
  them into `ApplyResult` after `wtx` — both engines speak the same diff language
  and the snapshot layer is engine-agnostic.
- A meta record in each wtx stores the global rev; reopen reads it and replays
  only the WAL tail (F7). WAL fsync strictly precedes engine apply;
  `wal_fsync = OnCommit` is mandatory with the fold feature.
- Upsert expansion (retract old + assert new) is done by clog before pushing,
  using its key→hash map; fold never needs lookups (F2 fallback, trivial).
- See `docs/fold-answers.md` for the full resolved F-checklist.

## 7. Rust-practice decisions

- Newtypes: `ObserverId(String)`, `ScopeId(String)`, `Rev(u64)` (serde-transparent).
- `#[non_exhaustive]` on `ClogError` and `Config`; `Config::default_for(path)` +
  builder methods.
- `Focus` builder for ergonomics (`Focus::uniform().weight("risk", 2.5).boost(…)`),
  plain serde struct underneath (INV-12 intact).
- CI hygiene: `#![deny(missing_docs)]`, clippy `-D warnings` +
  `clippy::unwrap_used` (allowed in tests), rustfmt check, MSRV pinned.
- INV-10 discipline: `BTreeMap`/`imbl::OrdMap` at every output boundary; f32 score
  ties broken by claim_key per spec.
- **Wake debounce runs on the Clock port, not wall time.** Under `Clock::Manual`,
  `advance()` drives debounce-window expiry; otherwise P8 is flaky.
- R1 crash tests use a child-process harness: the test re-execs itself as a
  subprocess that aborts at an injected crash point; the parent reopens and
  asserts WAL-replay equality.

## 8. Ambiguity ledger (small calls the spec leaves open)

| Topic | Decision |
|---|---|
| `Filter.min_score` on non-Urgent view | new `InvalidFilter { reason }` error variant |
| `recall()` with semantic disabled/feature off | new `SemanticDisabled` error variant |
| Literal `%{` in templates | no escape in v1; documented |
| Cloned `wakes()` receivers | compete (each Wake delivered once); documented single-consumer intent |
| `situation(None)` | the `"default"` scope; unknown names → `UnknownScope` |
| `Filter.limit` | clamps to max 500 (no error) |
| Exemplar claim body | JSON `{ "body": …, "kind": … }`; key `clog:exemplar:{content_hash(body)}` |
| `Situation.as_of` | clock reading at the rendering batch; RFC3339 UTC in the header slot |

## 9. Default scopes and foci

The lib ships exactly one default: scope `"default"` with a uniform `Focus`
(weights 1.0, no boosts, half-life 7 days) — scoring reduces to trust × recency,
a usable zero-config ordering. No named preset foci in the API: clog owns algebra,
not judgment, and §14.1's constants are unvalidated. Recipes live in the examples:
`agency.rs` ships `delivery-health` and `cash-and-collections` lenses;
`household.rs` ships per-member scopes with disjoint boosts.

## 10. Classifier eval harness (new M3 deliverable)

Source: the garden3d Notion "Observations" database (~10.5k rows hand-labeled by
Type — a manual prototype of clog). Not training (ESE is frozen; kNN is exemplar
lookup); three uses:

1. **Seed exemplars**: balanced ~30–50 bodies/kind for the agency example config.
2. **Eval**: hold-out split; measure cascade *coverage* (% confidently
   auto-classified) and *accuracy* (agreement with Type); sweep
   `k`/`min_sim`/`min_votes`. Replaces §14.1 guesses with measurements.
3. **Scoring sanity**: the DB's human Salience column (High/Medium/Low) checks
   that plausible foci rank High-salience observations above Low (rank
   correlation, not exact order).

Label mapping: FYI/Risk/Decision/Question/Commitment → same-named kinds; Lead →
`opportunity`; **Resourcing → custom kind `resourcing` declared in the agency
example** (demonstrating custom taxonomies with real data); 6 noise rows dropped.
Note Commitment has only 35 examples — weak coverage there is a finding, not a
failure (Unclassified is the designed escalation path).

Privacy: the export script applies a **stable pseudonym map** (each real
client/person → consistent fake name, preserving cross-claim structure). The full
corpus stays gitignored and is pulled on demand; only a small **human-reviewed**
sample (~30–50/kind) is committed as `tests/fixtures/exemplars.jsonl`. Renaming
alone is not full sanitization — the committed sample gets a manual skim for
sensitive content beyond names before landing.

## 11. Build process

One implementation plan per milestone gate, written with the writing-plans skill,
TDD throughout, user review between plans:

- **P1 = M0+M1**: types, validation, pure core, template parser; naive engine,
  renderer, WAL, rev model. Exit: §11.1 units, P1–P4, P6–P7, G1, R1–R3 green.
- **P2 = M2**: scopes, focus, bucketed decay/ticks, watches/wakes. Exit: P5, P8,
  B3, B4 green; G1 focus-shift checkpoints.
- **P3 = M3**: ESE/ANNy wrapper, recall, kNN tier, correct()/exemplars,
  Unclassified queue, **Notion-fed eval harness**. Exit: U-KIND-*, B5 green.
- **P4 = M4**: fold adapter behind `feature = "fold"`. Exit: D1, D2 green; B1/B2
  re-run on fold.
- **P5 = M5**: fuzz, loom, deferred naive snapshot files (B6), rustdoc + examples
  as doctests, CHANGELOG, freeze `v0.1.0`.

README maintenance: `examples/clog/README.md` currency is an exit criterion of
every plan; the repo `CLAUDE.md` carries the standing rule.
