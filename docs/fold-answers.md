# Fold capability answers (spec §8 F-checklist)

Resolved 2026-08-15 by reading the fold/ese/anny source in this workspace, extending
the three answers (F4, F7, F11) previously confirmed by the Flower Computer team.
Citations are to files in this repo.

| # | Question | Answer | Evidence |
|---|---|---|---|
| F1 | Fan-out | **Yes, native.** Tuples of `Push` nodes (up to 16) broadcast every delta to each element; the reader mirrors the tuple. | `fold/src/pipeline/mod.rs` (module docs, `tuple` mod) |
| F2 | Keyed lookup inside wtx | **Yes.** `WriteTx::get` reads a key seeing the transaction's own uncommitted writes; `KeyedStream::upsert` is exactly retract-old + assert-new, and short-circuits byte-identical records (free INV-5 assist). Clog still keeps its own key→hash map, so the adapter can also expand upserts itself. | `fold/src/stream/mod.rs:143`, `fold/src/stream/keyed.rs:167` |
| F3 | Stateful keyed reduces | **Partly native, rest via F4.** `Aggregate` (invertible step fn), `TopK`, `Ranked`, `KeyedRanked` exist. BestBelief / latest-name / focus-parameterized TopK are custom terminals. | `fold/src/pipeline/ops/keyed.rs`, `ops/scored.rs`, `terminal/ranked.rs` |
| F4 | Implementable terminal trait | **CONFIRMED (team + code).** `Push` is public: `init`/`push`/`commit`/`abort`/`reader`, with commit re-entrant mid-transaction. All stateful clog views are built as custom terminals. | `fold/src/pipeline/mod.rs:78` |
| F5 | Joins / parameterized re-scoring | **No joins; fallback applies.** `Map`/`ScoreBy` fns must be deterministic so retractions cancel — time- and focus-dependent scores cannot be a `ScoreBy`. Focus + bucket state live inside the per-scope urgent terminal; `SetFocus`/`Tick` events trigger internal re-score of that scope. | `fold/src/pipeline/ops/mod.rs` (Map docs) |
| F6 | Multiple input types on one stream | **Workable as assumed.** Single `Stream<Event>` with per-variant `FilterMap` routing into the fan-out tuple. | `fold/src/pipeline/ops/mod.rs` (`FilterMap`) |
| F7 | Crash/reopen | **CONFIRMED, with nuance.** Commits are durable against *process* crash when `wtx` returns; `checkpoint()` (fsync) additionally hardens against OS/power failure. Adapter writes the global rev as a meta record in each wtx; reopen reads it and replays only the WAL tail. Write-ahead ordering (WAL fsync before engine apply) mandatory; `wal_fsync = OnCommit` required with the fold feature. | `fold/src/stream/unkeyed.rs:84` |
| F8 | Predicate scan | **Build the index anyway.** observer→live-keys maintained as its own view (`Multimap`-shaped) in both engines; no scans. | `fold/src/pipeline/terminal/mod.rs` (`Multimap`) |
| F9 | Read/write concurrency | **Single-writer by construction.** `wtx` takes `&mut self`; `rtx` pins a snapshot from `&self`. Matches clog's one-writer-thread actor; snapshot copy-out happens outside the write critical section via ArcSwap publication. | `fold/src/stream/unkeyed.rs:50,78` |
| F10 | remove+insert cost | **Acceptable.** Upsert-as-retract+insert is the designed contract (`KeyedStream`); posting sinks are set-semantic per record and read no prior state on retraction. Confirm empirically in B1/B2 re-runs at M4. | `fold/src/pipeline/terminal/mod.rs` (Retraction section) |
| F11 | ANNy deletion | **CONFIRMED (team + code).** `Hnsw` terminal calls `index.remove(id)` — true node deletion, no tombstones. Keep B5's churn/recall assertion as the guard; `semantic_rebuild_tombstone_ratio` stays a disabled contingency. | `fold/src/pipeline/terminal/search/hnsw.rs:55` |
| F12 | ESE provenance | **Open — ask Flower.** License file exists in `ese/`; training source, OOV behavior, and DIMENSIONS stability across versions still unconfirmed. Non-blocking for v1 (index rebuilt on open; vectors not persisted by clog). We build with features `dim-512`, `quant-8` per the new-project script. |
| F13 | Batch ergonomics | **Fine.** One wtx per clog batch; no size limits observed. Measure 1k-claim batches in B2. |
| F14 | Recursion | **Not needed, as designed.** Depth-1 alias flattening at write time; no closure computation anywhere. v2 relations still want Flower's roadmap answer. |

Log any further Flower Computer answers here as they arrive.
