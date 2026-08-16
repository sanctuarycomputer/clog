# clog

Orientation engine for agentic systems, built on the BogKit workspace.
Spec: `../../docs/clog-spec-v1.md`. Build design:
`../../docs/superpowers/specs/2026-08-15-clog-build-design.md`.

## What clog is

clog is an orientation engine, not a chat framework: hosts write small,
structured claims ("Invoice 1042 is 30 days overdue", sourced from Gmail,
rated for reliability and credibility), and clog maintains a set of
materialized views over them incrementally as claims arrive, get corrected,
or get retracted. On demand it renders a token-budgeted *situation*
document — a ranked, deduplicated brief — per named scope. clog never calls
a model, never generates text beyond deterministic template substitution,
and carries no host-specific semantics: it doesn't know what an "invoice"
or a "renewal" is, only what a claim, a kind, and an entity are.

## The host integration loop

The intended shape of an integration is small and does no retrieval work at
read time:

1. **Connectors call `observe()`.** Each source (Gmail, Slack, a ticket
   tracker, whatever) turns its events into `Claim`s and calls
   `clog.observe(claims, opts)`. clog validates, deduplicates, classifies,
   and updates its materialized views — this is the only place work happens
   on write.
2. **The agent calls `situation()` once per turn.** Before doing anything
   else, the agent reads `clog.situation(scope, None)` as its orientation
   layer: a ranked, budgeted brief of what's live and urgent in that scope.
   This is a pure read of the last published snapshot (INV-1) — no scoring,
   no ranking, no rendering happens at read time, so it's cheap enough to
   call every turn.
3. **Corrections flow back as `retract`/`observe`, not mutation.** If the
   agent or a host process learns a claim was wrong, it retracts the old
   claim and (optionally) observes a corrected one. clog has no "edit"
   operation by design: every state change is an append to the log, so the
   history of what was believed and when is always reconstructable.

What P1 deliberately does *not* do: it never decides *when* to interrupt the
agent, and it never ranks by anything but the deterministic scorer in this
crate. Runtime focus changes, ticked recency decay, and a watch/wake
mechanism for push-style attention land in P2; semantic recall and a kNN
classification tier land in P3. Until then, `situation()` polling and the
rule-based classifier are the whole story, and that is enough to wire a host
today.

## P1 API

| Function | What it does |
|---|---|
| `Clog::open(cfg)` | Opens (or creates) an instance at `cfg.path`, replaying its WAL. |
| `observe(claims, opts)` | Records a batch of claims; duplicates are invisible, changed keys supersede. |
| `retract(claim_key)` | Retracts a claim, healing every view that mentioned it. |
| `revoke_observer(id)` | Withdraws everything one observer ever said, in a single revision. |
| `merge_entities(alias, canonical)` | Declares two entity refs the same entity, depth-1, retractable. |
| `select(view, filter)` | Reads rows out of one materialized view (`Live`, `Urgent`, `OpenLoops`, `EntityState`, `Unclassified`), filtered. |
| `situation(scope, template)` | Reads a scope's rendered, budgeted situation document. |
| `advance(ms)` | Moves the manual test clock forward; `ClockMode::Manual` only. |

## Quickstart

The listing below is also a runnable, compiling example:
`examples/clog/examples/quickstart.rs`. Run it with:

```
cargo run -p clog --example quickstart
```

```rust
use clog::*;

fn main() -> Result<(), ClogError> {
    let dir = tempfile::tempdir().expect("tempdir");

    // A scope named "inbox", declared with default focus; `Config::default_for`
    // always injects "default" too.
    let mut cfg = Config::default_for(dir.path());
    cfg.scopes.insert("inbox".to_string(), Focus::default());
    let clog = Clog::open(cfg)?;

    clog.observe(
        vec![
            claim("gmail:msg/1", "Invoice 1042 is 30 days overdue"),
            claim("gmail:msg/2", "Halcyon renewal call moved to Thursday"),
        ],
        ObserveOpts::default(),
    )?;

    let situation = clog.situation(Some("inbox"), None)?;
    println!("{}", situation.text);

    // A correction: the host learned the invoice claim was wrong.
    clog.retract("gmail:msg/1")?;
    let healed = clog.situation(Some("inbox"), None)?;
    println!("{}", healed.text);
    Ok(())
}

fn claim(key: &str, body: &str) -> Claim {
    Claim {
        claim_key: key.into(),
        subject_key: None,
        source_ref: key.into(),
        observer: ObserverId::from("gmail-v3"),
        schema_v: 1,
        occurred_at: 1_786_752_000_000,
        observed_at: 1_786_752_000_000,
        reliability: Reliability::B,
        credibility: Credibility::Two,
        entities: vec![],
        body: body.into(),
    }
}
```

## Config knobs

All fields on `Config`; build one with `Config::default_for(path)` and
override what you need.

| Field | Default | What it controls |
|---|---|---|
| `scopes` | `{}` (+ `"default"` always injected) | Per-scope `Focus` overrides — weights, boosts, half-life, `top_k`. |
| `kinds` | 8-kind default taxonomy (`fact`, `decision`, `risk`, `question`, `commitment`, `agreement`, `opportunity`, `fyi`) | The claim kind taxonomy the rules classifier matches against. |
| `loop_kinds` | `[question, risk, commitment]` | Which kinds count as "open loops" — unresolved until retracted or reclassified. |
| `top_k` | `12` | Default cap on ranked rows per rendered situation. |
| `budget_chars` | `6000` | Character budget for a rendered situation document. |
| `tick` | `TickConfig { mode: System, interval_ms: 60_000 }` | Clock mode (`System` or `Manual`, INV-10) and tick interval. |
| `decay_buckets_per_half_life` | `4` | Granularity of the bucketed recency-decay clock. |
| `belief_min_credibility` | `Credibility::Six` | The credibility floor below which a claim is never believed. |
| `wal_fsync` | `FsyncPolicy::OnCommit` | Whether the WAL fsyncs after every commit or relies on OS buffering. |
| `write_queue` | `1024` | Bounded depth of the writer's command channel — backpressure past this blocks the caller. |
| `rebuild_on_open` | `false` | If `true`, discards cached engine state and rebuilds every view from a full WAL replay on open. |

## Caveats

These are documented, deliberate P1 behaviors — not bugs — worth knowing
before you build against them:

- **No `%{` escape in templates.** The template renderer has no escape
  sequence for a literal `%{` in v1; if you write custom templates, avoid
  that exact two-character sequence outside of a slot.
- **A batch that is entirely duplicates commits nothing.** If every claim in
  an `observe()` call is byte-identical to what's already live (INV-5), the
  call succeeds but takes no revision — `Ack.rev` comes back unchanged from
  the previous call, not incremented.
- **Watches and wakes are P2.** There is no push notification when a scope's
  situation changes; hosts poll `situation()` (cheap — it's a pure snapshot
  read) rather than subscribing to one.
- **The WAL wire format is not yet versioned.** Pre-1.0, the on-disk frame
  format (`[len][crc32][payload]`, postcard-encoded) can change between
  releases without a migration path. Don't treat a clog data directory as a
  long-term archival format yet.
- **`ClockMode::Manual` is the test/deterministic clock.** Production hosts
  should use the default `ClockMode::System`; `Manual` (advanced only via
  `Clog::advance`) exists so tests can pin `occurred_at`/`as_of` and get
  byte-identical rendered output.
- **Merge dedupe is timestamp-sensitive under `ClockMode::System`.**
  `merge_entities`'s INV-5 dedupe of an identical repeat call compares the
  full claim, including the writer's clock reading — so it holds exactly
  under `Manual`, but a repeat call under a moving system clock will
  supersede and take a new revision even though the resulting alias map is
  identical. This is a revision-churn question, not a correctness one.

## How it fits together

- One writer thread owns the engine and the WAL. Every write is a command on
  a bounded channel; the caller blocks for the `Ack`, so backpressure is just
  a blocking send.
- **WAL append (and fsync) always precedes engine apply.** A crash can lose
  the tail of the log; it can never leave the engine ahead of it.
- The WAL records *effects*, not intentions: a batch carries the caller's
  events plus the classifier's derived `Judge` events, exactly as applied,
  plus the clock reading it was committed at. Replay is pure event
  application — the classifier never runs on replay — so the same log bytes
  always rebuild the same world, down to byte-identical situation text.
- A scope's `rev` and `as_of` move only when its text *materially* changes;
  the rev skew against the global rev is the signal that the writes in
  between did not affect that scope.
- After each batch the writer publishes an immutable `WorldSnapshot` through
  `ArcSwap`. Reads never touch the writer.
- Dropping the last `Clog` handle shuts the writer down: it finishes the
  in-flight batch, fsyncs and exits before `drop` returns.

## Status

- **P1 (this release) — complete.** Pure core, naive engine, WAL, writer
  actor, full public API (`open`/`observe`/`retract`/`revoke_observer`/
  `select`/`situation`/`merge_entities`/`advance`), property suite, G1
  goldens, crash-recovery tests.
- **P2 (next)** — runtime `set_focus`, ticked/bucketed recency decay,
  watches and wakes for push-style attention.
- **P3** — semantic recall, a kNN classification tier layered onto the rule
  classifier, `correct()`/exemplars, and a Notion-fed classifier eval
  harness.
- **P4** — a `fold`-backed engine (behind `feature = "fold"`) as a
  drop-in-faster alternative to the naive engine, differentially tested
  against it.

See `../../docs/clog-spec-v1.md` for the full specification and
`../../docs/superpowers/specs/2026-08-15-clog-build-design.md` for the
build design and milestone plan.

## Docs

`cargo doc -p clog --open` for the rustdoc; the module docs carry the
spec-section references.
