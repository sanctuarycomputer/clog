# clog

Orientation engine for agentic systems, built on the BogKit workspace.
Spec: `../../docs/clog-spec-v1.md`. Build design:
`../../docs/superpowers/specs/2026-08-15-clog-build-design.md`.

Status: P1 in progress (pure core + naive engine + WAL + the writer actor).

## Public API (implemented so far)

```rust
pub struct Clog;                       // Clone + Send + Sync

impl Clog {
    pub fn open(cfg: Config) -> Result<Clog, ClogError>;

    // WRITE — one committed batch per call, at most one rev bump
    pub fn observe(&self, claims: Vec<Claim>, opts: ObserveOpts) -> Result<Ack, ClogError>;
    pub fn retract(&self, claim_key: &str) -> Result<Ack, ClogError>;

    // READ — published snapshot only (INV-1)
    pub fn situation(&self, scope: Option<&str>, template: Option<&str>)
        -> Result<Situation, ClogError>;

    // TEST CLOCK — Manual mode only (INV-10)
    pub fn advance(&self, ms: u64) -> Result<(), ClogError>;
}
```

Plus the data types in `types.rs` (`Claim`, `Config`, `Focus`, `Situation`,
`Ack`, `ObserveOpts`, `ClogError`, …), all serde round-trippable (INV-12).

Still to land in P1: `select`, `revoke_observer`, `merge_entities`.

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

## Docs

`cargo doc -p clog --open` for the rustdoc; the module docs carry the
spec-section references.
