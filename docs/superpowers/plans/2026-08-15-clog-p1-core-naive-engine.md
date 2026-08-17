# Clog P1 (M0+M1): Pure Core + Naive Engine Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** A working `clog` library crate (naive engine only) supporting `open`, `observe`, `retract`, `revoke_observer`, `situation`, `select`, `merge_entities` with WAL persistence, deterministic rendering, and the M0+M1 test suite green.

**Architecture:** Pure semantic modules (score/belief/alias/kinds/render) with no I/O; a naive engine holding all views in `imbl` ordered maps, recomputing slot inputs per batch and re-rendering only when text changes; a single writer thread owning engine + WAL, publishing immutable `WorldSnapshot`s via `ArcSwap`; readers never compute. WAL is length-prefixed postcard records with CRC32; replay is pure event application (classifier never runs on replay).

**Tech Stack:** Rust edition 2024, serde, thiserror, imbl, arc-swap, crossbeam-channel, postcard, crc32fast, regex; dev: proptest, insta, tempfile.

**Spec:** `docs/clog-spec-v1.md` (authority), amended by `docs/superpowers/specs/2026-08-15-clog-build-design.md` (decisions). Read both before starting. Fold capability notes: `docs/fold-answers.md` (not needed for P1 — the fold feature is out of scope here).

## Global Constraints

- Crate lives at `examples/clog/`, `[lib]` only, `publish = false`, edition 2024 (auto-member via workspace glob `examples/*`).
- Dependency allowlist for P1 (runtime): serde (derive), thiserror, imbl, arc-swap, crossbeam-channel, postcard (`use-std`), crc32fast, regex. Dev-only: proptest, insta (yaml off, default), tempfile. **Nothing else.** No chrono/time — RFC3339 is hand-rolled (Task 8). No `fold`/`ese`/`anny` in P1.
- `#![deny(missing_docs)]` on the crate; every public item documented before a task is done.
- No `unwrap`/`expect` in `src/` except in `#[cfg(test)]` code (locked by `#![cfg_attr(not(test), deny(clippy::unwrap_used, clippy::expect_used))]`).
- INV-10 discipline: no `std::collections::HashMap` anywhere state or output ordering could leak from — use `BTreeMap`/`imbl::OrdMap`/`imbl::OrdSet`. All wall-clock reads go through `clock.rs`; **all tests use `ClockMode::Manual`**.
- Spec invariants INV-1..13 are cited by number in tests; test names must match the spec's names (U-SCORE-1 → `u_score_1_…`, P1 → `p1_…`, etc.).
- Reserved namespace prefix is `clog:` (INV-8). Internal observer for reserved claims is `ObserverId("clog")`.
- Every commit: `cargo test -p clog` green first. Run commands from the workspace root.
- If any step's expected output disagrees with reality, STOP and re-read the spec section cited in that task before improvising.

**Deviations from the spec, already approved in the build design — do not "fix" them back:**
- postcard replaces bincode; `imbl` is allowed; naive snapshot files (`snap/`, `snapshot_every_batches`) are **deferred to M5** — open always replays the full WAL in P1.
- `content_hash` is dropped: INV-5 idempotence uses structural equality (`Claim: PartialEq`) against the live map.
- `set_focus`, ticks, watches/wakes, `recall`, `correct`, kNN are **out of scope** (P2/P3). `Event` still defines their variants so the WAL format is stable.
- U-KIND-2 (kNN votes) is P3; U-KIND-1 (rules tier) is in this plan.
- Batches that expand to zero events (pure duplicates) are not committed: no WAL append, no rev bump; `Ack.rev` = current rev.
- Scoring age basis: `occurred_at`, clamped to `now` when > now + 24h (§10).

---

### Task 1: Crate scaffold

**Files:**
- Create: `examples/clog/Cargo.toml`, `examples/clog/src/lib.rs`, `examples/clog/README.md`, `examples/clog/.gitignore`

**Interfaces:**
- Produces: an empty documented crate `clog` that builds inside the workspace; later tasks add modules to `src/lib.rs`.

- [ ] **Step 1: Write the crate manifest and empty lib**

`examples/clog/Cargo.toml`:

```toml
[package]
name = "clog"
version = "0.0.0"
edition = "2024"
publish = false
description = "Orientation engine for agentic systems: claims in, ranked situations out."

[dependencies]
serde = { version = "1", features = ["derive"] }
thiserror = "2"
imbl = "6"
arc-swap = "1"
crossbeam-channel = "0.5"
postcard = { version = "1", features = ["use-std"] }
crc32fast = "1"
regex = "1"

[dev-dependencies]
proptest = "1"
insta = "1"
tempfile = "3"

[features]
# test-crash compiles the crash-injection hook used by the R1 harness (Task 17)
test-crash = []
```

`examples/clog/src/lib.rs`:

```rust
//! Clog: an orientation engine for agentic systems.
//!
//! Hosts write structured claims; clog maintains materialized views over them
//! incrementally and renders a budgeted situation document per scope. See
//! `docs/clog-spec-v1.md` in the repository root for the full specification.
#![deny(missing_docs)]
#![cfg_attr(not(test), deny(clippy::unwrap_used, clippy::expect_used))]
```

`examples/clog/README.md`:

```markdown
# clog

Orientation engine for agentic systems, built on the BogKit workspace.
Spec: `../../docs/clog-spec-v1.md`. Build design:
`../../docs/superpowers/specs/2026-08-15-clog-build-design.md`.

Status: P1 in progress (pure core + naive engine + WAL).

## Public API (implemented so far)

(none yet)
```

`examples/clog/.gitignore`:

```
/tests/fixtures/corpus*
```

- [ ] **Step 2: Verify the workspace picks it up**

Run: `cargo check -p clog`
Expected: compiles clean (empty lib). If `imbl = "6"` fails to resolve, run `cargo add imbl -p clog` to take the latest and keep whatever version it picks.

- [ ] **Step 3: Commit**

```bash
git add examples/clog
git commit -m "feat(clog): scaffold library crate"
```

---

### Task 2: Public types (`types.rs`)

**Files:**
- Create: `examples/clog/src/types.rs`
- Modify: `examples/clog/src/lib.rs` (add `pub mod types;` and re-export `pub use types::*;`)

**Interfaces:**
- Produces (exact, used by every later task):
  - `pub struct ObserverId(pub String)` — serde-transparent; `From<&str>`.
  - `pub type Rev = u64;`
  - `pub enum Reliability { A, B, C, D, E, F }` with `pub fn rank(self) -> u8` (A=0 best) and `pub fn letter(self) -> char`.
  - `pub enum Credibility { One, Two, Three, Four, Five, Six }` with `pub fn rank(self) -> u8` (One=0 best) and `pub fn digit(self) -> u8` (One=1).
  - `pub struct EntityRef { pub etype: String, pub id: String, pub name: Option<String> }` — **Eq/Ord/Hash on (etype,id) only** (manual impls; `name` excluded) plus `pub fn key(&self) -> (String, String)`.
  - `pub struct Claim { pub claim_key: String, pub subject_key: Option<String>, pub source_ref: String, pub observer: ObserverId, pub schema_v: u16, pub occurred_at: u64, pub observed_at: u64, pub reliability: Reliability, pub credibility: Credibility, pub entities: Vec<EntityRef>, pub body: String }` — derives `Clone, Debug, PartialEq, Serialize, Deserialize`.
  - `pub struct Focus { pub weights: BTreeMap<String, f32>, pub boosts: Vec<(EntityRef, f32)>, pub half_life_days: f32, pub top_k: Option<usize> }` with `Default` = `Focus::uniform()` and builder methods `uniform() -> Focus`, `weight(self, kind: &str, w: f32) -> Focus`, `boost(self, e: EntityRef, f: f32) -> Focus`, `half_life_days(self, d: f32) -> Focus`, `top_k(self, k: usize) -> Focus`.
  - `pub enum View { Live, EntityState, OpenLoops, Urgent { scope: String }, Unclassified }`
  - `pub struct Filter { pub kinds: Option<Vec<String>>, pub entities: Option<Vec<EntityRef>>, pub observer: Option<ObserverId>, pub subject_prefix: Option<String>, pub occurred_after: Option<u64>, pub min_score: Option<f32>, pub limit: Option<usize> }` (`Default`).
  - `pub enum JudgeSource { Rule, Knn, External }`
  - `pub struct KindLabel { pub kind: String, pub confidence: f32, pub source: JudgeSource }`
  - `pub struct Row { pub claim: Claim, pub recorded_at: u64, pub kind: Option<KindLabel>, pub score: Option<f32>, pub believed: Option<bool> }`
  - `pub struct Situation { pub scope: String, pub text: String, pub rev: Rev, pub as_of: u64 }`
  - `pub struct Ack { pub rev: Rev, pub situation: Option<Situation> }`
  - `pub struct ObserveOpts { pub return_situation: Option<String> }` (`Default`)
  - `pub struct Judgment { pub kind: String }`
  - `pub enum ClockMode { System, Manual }`, `pub enum FsyncPolicy { OnCommit, Never }`
  - `pub struct TickConfig { pub mode: ClockMode, pub interval_ms: u64 }` (`Default`: System, 60_000)
  - `pub enum Matcher { BodyContains(String), BodyRegex(String), ObserverIs(String), EntityType(String) }` (BodyContains is case-insensitive by definition, §5.6)
  - `pub struct Rule { pub any_of: Vec<Matcher> }`
  - `pub struct KindDef { pub name: String, pub rules: Vec<Rule>, pub seed_exemplars: Vec<String> }`
  - `pub struct KindTaxonomy { pub kinds: Vec<KindDef> }` with `pub fn default_taxonomy() -> KindTaxonomy` (the 8 spec kinds, no rules/exemplars) and `pub fn contains(&self, kind: &str) -> bool`.
  - `pub struct Config { pub path: PathBuf, pub scopes: BTreeMap<String, Focus>, pub kinds: KindTaxonomy, pub loop_kinds: Vec<String>, pub top_k: usize, pub budget_chars: usize, pub tick: TickConfig, pub decay_buckets_per_half_life: u32, pub belief_min_credibility: Credibility, pub wal_fsync: FsyncPolicy, pub write_queue: usize, pub rebuild_on_open: bool }` — `#[non_exhaustive]` is **not** used on Config in P1 (it would block struct-literal tests); instead all-fields-public + `pub fn default_for(path: impl Into<PathBuf>) -> Config` with spec defaults (top_k 12, budget_chars 6000, buckets 4, floor Six, loop_kinds ["question","risk","commitment"], fsync OnCommit, write_queue 1024).
  - `pub enum ClogError` (thiserror, `#[non_exhaustive]`): `InvalidClaim { index: usize, reason: String }`, `ReservedNamespace`, `UnknownClaim`, `UnknownScope`, `UnknownKind`, `AliasCycle`, `TemplateError(String)`, `Storage(#[from] std::io::Error)`, `Corrupt { detail: String }`, `ShuttingDown`, `InvalidFilter { reason: String }`, `SemanticDisabled`, `ManualClockRequired`.

All public types derive `Clone, Debug, Serialize, Deserialize` (INV-12), plus `PartialEq` where meaningful. Every item gets a rustdoc line.

- [ ] **Step 1: Write the failing test** (bottom of `types.rs`)

```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn serde_round_trips_public_surface() {
        let c = Claim {
            claim_key: "halcyon:inv-1042".into(),
            subject_key: Some("halcyon:inv-1042:status".into()),
            source_ref: "gmail:msg/123".into(),
            observer: ObserverId::from("gmail-v3"),
            schema_v: 1,
            occurred_at: 1_000,
            observed_at: 2_000,
            reliability: Reliability::B,
            credibility: Credibility::Two,
            entities: vec![EntityRef { etype: "project".into(), id: "halcyon".into(), name: Some("Halcyon".into()) }],
            body: "Invoice 1042 is 30 days overdue".into(),
        };
        let bytes = postcard::to_allocvec(&c).unwrap();
        assert_eq!(postcard::from_bytes::<Claim>(&bytes).unwrap(), c);

        let f = Focus::uniform().weight("risk", 2.5).half_life_days(3.0).top_k(8);
        let json = serde_json_like_roundtrip(&f); // via postcard, same as above
        assert_eq!(json.weights.get("risk"), Some(&2.5));
        assert_eq!(json.half_life_days, 3.0);
        assert_eq!(json.top_k, Some(8));
    }

    fn serde_json_like_roundtrip(f: &Focus) -> Focus {
        postcard::from_bytes(&postcard::to_allocvec(f).unwrap()).unwrap()
    }

    #[test]
    fn entity_ref_identity_ignores_name() {
        let a = EntityRef { etype: "person".into(), id: "sam".into(), name: Some("Sam".into()) };
        let b = EntityRef { etype: "person".into(), id: "sam".into(), name: None };
        assert_eq!(a, b);
        use std::collections::BTreeSet;
        let mut s = BTreeSet::new();
        s.insert(a);
        assert!(s.contains(&b));
    }

    #[test]
    fn trust_ranks() {
        assert!(Reliability::A.rank() < Reliability::F.rank());
        assert!(Credibility::One.rank() < Credibility::Six.rank());
        assert_eq!(Reliability::C.letter(), 'C');
        assert_eq!(Credibility::Three.digit(), 3);
    }

    #[test]
    fn config_defaults_match_spec() {
        let c = Config::default_for("/tmp/x");
        assert_eq!(c.top_k, 12);
        assert_eq!(c.budget_chars, 6000);
        assert_eq!(c.decay_buckets_per_half_life, 4);
        assert_eq!(c.loop_kinds, vec!["question", "risk", "commitment"]);
        assert!(KindTaxonomy::default_taxonomy().contains("fyi"));
        assert_eq!(KindTaxonomy::default_taxonomy().kinds.len(), 8);
    }
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test -p clog`
Expected: compile error — types don't exist.

- [ ] **Step 3: Implement `types.rs`**

Write all types per the Produces list. Key manual impls:

```rust
impl PartialEq for EntityRef {
    fn eq(&self, other: &Self) -> bool { self.etype == other.etype && self.id == other.id }
}
impl Eq for EntityRef {}
impl PartialOrd for EntityRef {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> { Some(self.cmp(other)) }
}
impl Ord for EntityRef {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        (&self.etype, &self.id).cmp(&(&other.etype, &other.id))
    }
}
impl std::hash::Hash for EntityRef {
    fn hash<H: std::hash::Hasher>(&self, h: &mut H) { self.etype.hash(h); self.id.hash(h); }
}

impl Reliability {
    /// 0 = best (A). Belief resolution and scoring use this rank.
    pub fn rank(self) -> u8 { self as u8 }
    /// The Admiralty letter, for rendering.
    pub fn letter(self) -> char { (b'A' + self as u8) as char }
}
impl Credibility {
    /// 0 = best (One).
    pub fn rank(self) -> u8 { self as u8 }
    /// The Admiralty digit 1..=6, for rendering.
    pub fn digit(self) -> u8 { self as u8 + 1 }
}
```

`default_taxonomy()` kinds, in order: `fact, decision, risk, question, commitment, agreement, opportunity, fyi`.

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test -p clog`
Expected: 4 tests PASS.

- [ ] **Step 5: Commit**

```bash
git add examples/clog/src
git commit -m "feat(clog): public API types (INV-12 serde surface)"
```

---

### Task 3: Validation (`validate.rs`) — U-VAL-1

**Files:**
- Create: `examples/clog/src/validate.rs` (+ `pub(crate) mod validate;` in lib.rs)

**Interfaces:**
- Consumes: `Claim`, `Focus`, `KindTaxonomy`, `ClogError` from Task 2.
- Produces:
  - `pub(crate) fn validate_claim(index: usize, c: &Claim, allow_reserved: bool) -> Result<(), ClogError>` — §10 rules; `allow_reserved` is set only by internal writers (merge claims).
  - `pub(crate) fn validate_focus(f: &Focus, taxonomy: &KindTaxonomy) -> Result<(), ClogError>` — weight keys in taxonomy, finite and > 0 values, half_life in (0.01, 3650), boost factors finite and > 0.
  - `pub(crate) fn scoring_clamp(ts: u64, now: u64) -> u64` — `if ts > now + 86_400_000 { now } else { ts }` (§10: clamp for scoring, store verbatim).

- [ ] **Step 1: Write the failing tests** — table-driven, every §10 rule positive and negative

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::*;

    fn base() -> Claim {
        Claim {
            claim_key: "k1".into(), subject_key: None, source_ref: "src:1".into(),
            observer: ObserverId::from("o1"), schema_v: 1,
            occurred_at: 1, observed_at: 1,
            reliability: Reliability::A, credibility: Credibility::One,
            entities: vec![], body: "b".into(),
        }
    }

    #[test]
    fn u_val_1_claim_rules() {
        // (mutation, should_pass, reason-substring)
        let cases: Vec<(Box<dyn Fn(&mut Claim)>, bool, &str)> = vec![
            (Box::new(|_| {}), true, ""),
            (Box::new(|c| c.claim_key = "  ".into()), false, "claim_key"),
            (Box::new(|c| c.claim_key = "x".repeat(257)), false, "claim_key"),
            (Box::new(|c| c.claim_key = "clog:evil".into()), false, "reserved"),
            (Box::new(|c| c.claim_key = "has\u{0007}bell".into()), false, "control"),
            (Box::new(|c| c.subject_key = Some("x".repeat(257))), false, "subject_key"),
            (Box::new(|c| c.source_ref = "".into()), false, "source_ref"),
            (Box::new(|c| c.source_ref = "x".repeat(1025)), false, "source_ref"),
            (Box::new(|c| c.observer = ObserverId(String::new())), false, "observer"),
            (Box::new(|c| c.observer = ObserverId("x".repeat(129))), false, "observer"),
            (Box::new(|c| c.body = "   ".into()), false, "body"),
            (Box::new(|c| c.body = "x".repeat(16 * 1024 + 1)), false, "body"),
            (Box::new(|c| c.entities = vec![EntityRef { etype: "p".into(), id: "i".into(), name: None }; 33]), false, "entities"),
            (Box::new(|c| c.entities = vec![EntityRef { etype: "".into(), id: "i".into(), name: None }]), false, "etype"),
            (Box::new(|c| c.entities = vec![EntityRef { etype: "p".into(), id: "x".repeat(129), name: None }]), false, "id"),
            (Box::new(|c| c.occurred_at = 0), false, "occurred_at"),
            (Box::new(|c| c.observed_at = 0), false, "observed_at"),
            // occurred_at > observed_at is ALLOWED (predictions)
            (Box::new(|c| { c.occurred_at = 10; c.observed_at = 5; }), true, ""),
        ];
        for (i, (mutate, ok, why)) in cases.iter().enumerate() {
            let mut c = base();
            mutate(&mut c);
            let r = validate_claim(7, &c, false);
            assert_eq!(r.is_ok(), *ok, "case {i}: {r:?}");
            if !ok {
                match r.unwrap_err() {
                    ClogError::InvalidClaim { index, reason } => {
                        assert_eq!(index, 7);
                        assert!(reason.to_lowercase().contains(why), "case {i}: {reason} !~ {why}");
                    }
                    ClogError::ReservedNamespace => assert_eq!(*why, "reserved"),
                    e => panic!("case {i}: wrong error {e:?}"),
                }
            }
        }
        // reserved allowed when internal
        let mut c = base();
        c.claim_key = "clog:merge:a->b".into();
        assert!(validate_claim(0, &c, true).is_ok());
    }

    #[test]
    fn u_val_1_focus_rules() {
        let tax = KindTaxonomy::default_taxonomy();
        assert!(validate_focus(&Focus::uniform().weight("risk", 2.0), &tax).is_ok());
        assert!(matches!(validate_focus(&Focus::uniform().weight("nope", 1.0), &tax), Err(ClogError::UnknownKind)));
        assert!(validate_focus(&Focus::uniform().weight("risk", f32::NAN), &tax).is_err());
        assert!(validate_focus(&Focus::uniform().weight("risk", 0.0), &tax).is_err());
        assert!(validate_focus(&Focus::uniform().half_life_days(0.005), &tax).is_err());
        assert!(validate_focus(&Focus::uniform().half_life_days(4000.0), &tax).is_err());
    }

    #[test]
    fn scoring_clamp_only_beyond_24h() {
        assert_eq!(scoring_clamp(100, 1_000_000), 100);
        assert_eq!(scoring_clamp(1_000_000 + 86_400_000, 1_000_000), 1_000_000 + 86_400_000);
        assert_eq!(scoring_clamp(1_000_000 + 86_400_001, 1_000_000), 1_000_000);
    }
}
```

- [ ] **Step 2: Run to verify failure** — `cargo test -p clog validate` → compile error.

- [ ] **Step 3: Implement.** Byte-length checks use `.len()` (bytes, per spec); "non-empty after trim" uses `.trim().is_empty()`; control chars via `s.chars().any(|ch| ch.is_control())` applied to claim_key, subject_key, source_ref. Reserved check: `c.claim_key.starts_with("clog:") && !allow_reserved` → `ClogError::ReservedNamespace`. Focus half-life bound: `0.01 < h && h < 3650.0` exclusive.

- [ ] **Step 4: Run** — `cargo test -p clog validate` → 3 PASS.

- [ ] **Step 5: Commit** — `git commit -m "feat(clog): observe-time validation (U-VAL-1)"`

---

### Task 4: Scoring (`score.rs`) — U-SCORE-1

**Files:**
- Create: `examples/clog/src/score.rs` (+ `pub(crate) mod score;`)

**Interfaces:**
- Consumes: `Reliability`, `Credibility`, `Focus`, `Claim`, `EntityRef` (Task 2), `scoring_clamp` (Task 3).
- Produces:
  - `pub(crate) fn trust(r: Reliability, c: Credibility) -> f32` — table lookup: REL `[1.00, 0.90, 0.75, 0.50, 0.25, 0.10]` indexed by `rank()`, CRED same values.
  - `pub(crate) fn bucket_age_days(age_days: f32, half_life_days: f32, buckets_per_half_life: u32) -> f32` — width = hl/buckets; `(age/w).floor()*w + w/2.0`; negative age clamps to first bucket midpoint.
  - `pub(crate) fn recency(bucket_age: f32, half_life_days: f32) -> f32` — `0.5f32.powf(bucket_age / half_life_days)`.
  - `pub(crate) fn score_claim(claim: &Claim, kind: Option<&str>, focus: &Focus, canonical_entities: &[ (String,String) ], now_ms: u64, buckets_per_half_life: u32) -> f32` — §5.4 formula; `canonical_entities` are the claim's entity keys *post-alias* (caller resolves); unclassified weight 1.0; boosts multiply for every focus boost whose entity key is in `canonical_entities`.

- [ ] **Step 1: Write the failing test** — the fixed table of §11.1

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::*;

    const DAY_MS: u64 = 86_400_000;

    #[test]
    fn u_score_1_trust_table() {
        assert_eq!(trust(Reliability::A, Credibility::One), 1.00);
        assert_eq!(trust(Reliability::B, Credibility::One), 0.90);
        assert_eq!(trust(Reliability::F, Credibility::Six), 0.10 * 0.10);
        assert_eq!(trust(Reliability::C, Credibility::Four), 0.75 * 0.50);
    }

    #[test]
    fn u_score_1_bucket_midpoints() {
        // half-life 7d, 4 buckets/hl -> width 1.75d
        let w = 7.0 / 4.0;
        assert_eq!(bucket_age_days(0.0, 7.0, 4), w / 2.0);          // first bucket midpoint
        assert_eq!(bucket_age_days(1.0, 7.0, 4), w / 2.0);          // same bucket
        assert_eq!(bucket_age_days(1.75, 7.0, 4), 1.75 + w / 2.0);  // boundary -> next bucket
        assert_eq!(bucket_age_days(-5.0, 7.0, 4), w / 2.0);         // future occurred_at
    }

    #[test]
    fn u_score_1_full_formula_with_boost_stacking() {
        let focus = Focus::uniform()
            .weight("risk", 2.0)
            .boost(EntityRef { etype: "project".into(), id: "halcyon".into(), name: None }, 1.5)
            .boost(EntityRef { etype: "person".into(), id: "sam".into(), name: None }, 2.0);
        let mut claim = crate::validate::tests_base_claim(); // helper added in step 3
        claim.reliability = Reliability::B;    // 0.90
        claim.credibility = Credibility::Three; // 0.75
        let now = 10 * DAY_MS;
        claim.occurred_at = now; // age 0 -> bucket midpoint 0.875d
        let ents = vec![("project".to_string(), "halcyon".to_string()),
                        ("person".to_string(), "sam".to_string())];
        let expected = 2.0 * (0.90 * 0.75)
            * 0.5f32.powf((0.875f32) / 7.0)
            * 1.5 * 2.0; // both boosts stack multiplicatively
        let got = score_claim(&claim, Some("risk"), &focus, &ents, now, 4);
        assert!((got - expected).abs() < 1e-6, "{got} vs {expected}");
        // unclassified -> weight 1.0
        let got_u = score_claim(&claim, None, &focus, &ents, now, 4);
        assert!((got_u - expected / 2.0).abs() < 1e-6);
        // no matching boost entities -> boost 1.0
        let got_n = score_claim(&claim, Some("risk"), &focus, &[], now, 4);
        assert!((got_n - expected / 3.0).abs() < 1e-6);
    }
}
```

- [ ] **Step 2: Run** — `cargo test -p clog score` → compile error.

- [ ] **Step 3: Implement.** Also add to `validate.rs` a `#[cfg(test)] pub(crate) fn tests_base_claim() -> Claim` returning Task 3's `base()` (move `base()` there, name it `tests_base_claim`, re-use from both test modules). Age computation inside `score_claim`: `age_days = (now_ms.saturating_sub(scoring_clamp(claim.occurred_at, now_ms))) as f32 / 86_400_000.0`; half-life from `focus.half_life_days`.

- [ ] **Step 4: Run** — 3 PASS.

- [ ] **Step 5: Commit** — `git commit -m "feat(clog): pure scoring (U-SCORE-1)"`

---

### Task 5: Belief resolution (`belief.rs`) — U-BELIEF-1/2

**Files:**
- Create: `examples/clog/src/belief.rs` (+ `pub(crate) mod belief;`)

**Interfaces:**
- Consumes: `Claim`, `Credibility` (Task 2).
- Produces:
  - `pub(crate) struct BeliefInput<'a> { pub claim: &'a Claim, pub recorded_at: u64 }`
  - `pub(crate) fn belief_key(c: &BeliefInput) -> (u64, std::cmp::Reverse<u8>, std::cmp::Reverse<u8>, u64, String)` — the §5.3 total order as a max-key: `(occurred_at, Reverse(reliability.rank()), Reverse(credibility.rank()), recorded_at, claim_key)`.
  - `pub(crate) fn resolve<'a>(group: &[BeliefInput<'a>], floor: Credibility) -> Option<&'a Claim>` — winner among members with `credibility.rank() <= floor.rank()`; if that set is empty and the group has exactly one live member, that member wins (only-claim exception); if the set is empty with ≥2 members, best by key among all members wins? **No** — spec §5.3: floored claims are "excluded from winning unless they are the only live claim". With ≥2 members all floored, no claim is believed → return `None`.

- [ ] **Step 1: Write failing tests**

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::*;
    use crate::validate::tests_base_claim;

    fn claim(key: &str, occ: u64, r: Reliability, c: Credibility) -> Claim {
        let mut cl = tests_base_claim();
        cl.claim_key = key.into();
        cl.subject_key = Some("s".into());
        cl.occurred_at = occ;
        cl.reliability = r;
        cl.credibility = c;
        cl
    }

    #[test]
    fn u_belief_1_total_order() {
        use Reliability::*; use Credibility::*;
        // each later claim beats all before it, per one tier of the order
        let a = claim("a", 100, F, Six);   // baseline
        let b = claim("b", 100, F, Five);  // better credibility
        let c = claim("c", 100, E, Six);   // better reliability beats credibility tier
        let d = claim("d", 200, F, Six);   // later occurred_at beats everything
        let claims = [&a, &b, &c, &d];
        // recorded_at all equal; exhaustive permutations of arrival order
        for perm in permutations(&claims) {
            let group: Vec<BeliefInput> = perm.iter().map(|c| BeliefInput { claim: c, recorded_at: 1 }).collect();
            assert_eq!(resolve(&group, Credibility::Six).unwrap().claim_key, "d");
        }
        // tie on everything but recorded_at
        let g = [BeliefInput { claim: &a, recorded_at: 5 }, BeliefInput { claim: &b0(&a, "a2"), recorded_at: 9 }];
        assert_eq!(resolve(&g, Credibility::Six).unwrap().claim_key, "a2");
        // full tie -> lexicographically larger claim_key
        let g = [BeliefInput { claim: &a, recorded_at: 5 }, BeliefInput { claim: &b0(&a, "z") , recorded_at: 5 }];
        assert_eq!(resolve(&g, Credibility::Six).unwrap().claim_key, "z");
    }

    fn b0(base: &Claim, key: &str) -> Claim { let mut c = base.clone(); c.claim_key = key.into(); c }

    fn permutations<'a>(xs: &[&'a Claim]) -> Vec<Vec<&'a Claim>> {
        if xs.len() <= 1 { return vec![xs.to_vec()]; }
        let mut out = vec![];
        for i in 0..xs.len() {
            let mut rest = xs.to_vec();
            let x = rest.remove(i);
            for mut p in permutations(&rest) { p.insert(0, x); out.push(p); }
        }
        out
    }

    #[test]
    fn u_belief_2_credibility_floor() {
        use Reliability::*; use Credibility::*;
        let good = claim("good", 100, A, Two);
        let bad = claim("bad", 200, A, Five); // newer but below floor Three
        let g = [BeliefInput { claim: &good, recorded_at: 1 }, BeliefInput { claim: &bad, recorded_at: 2 }];
        assert_eq!(resolve(&g, Three).unwrap().claim_key, "good");
        // only-claim exception
        let g = [BeliefInput { claim: &bad, recorded_at: 2 }];
        assert_eq!(resolve(&g, Three).unwrap().claim_key, "bad");
        // all floored, >= 2 members -> nobody believed
        let bad2 = claim("bad2", 300, A, Six);
        let g = [BeliefInput { claim: &bad, recorded_at: 2 }, BeliefInput { claim: &bad2, recorded_at: 3 }];
        assert!(resolve(&g, Three).is_none());
    }
}
```

- [ ] **Step 2: Run** — compile error.
- [ ] **Step 3: Implement** — `resolve` filters by floor, falls back to only-claim exception, picks `max_by_key(belief_key)`.
- [ ] **Step 4: Run** — 2 PASS.
- [ ] **Step 5: Commit** — `git commit -m "feat(clog): belief resolution total order (U-BELIEF-1/2)"`

---

### Task 6: Alias map (`alias.rs`) — U-ALIAS-1/2

**Files:**
- Create: `examples/clog/src/alias.rs` (+ `pub(crate) mod alias;`)

**Interfaces:**
- Consumes: `ClogError` (Task 2).
- Produces:
  - `pub(crate) type EntityKey = (String, String);` — (etype, id)
  - `#[derive(Clone, Default)] pub(crate) struct AliasMap { edges: imbl::OrdMap<EntityKey, EntityKey> }`
  - `pub(crate) fn resolve(&self, k: &EntityKey) -> EntityKey` — one hop (edges are depth-1 by construction); identity if absent.
  - `pub(crate) fn flatten_target(&self, canonical: &EntityKey) -> EntityKey` — write-time flattening: if `canonical` is itself aliased, return its target (§5.2).
  - `pub(crate) fn insert(&mut self, alias: EntityKey, canonical: EntityKey) -> Result<(), ClogError>` — flattens the target, then rejects `AliasCycle` if the flattened target equals `alias` or the edge would point at itself; also **re-points any existing edges whose target is `alias`** to the new canonical, keeping depth-1 (a→b then b→c re-points a→c).
  - `pub(crate) fn remove(&mut self, alias: &EntityKey)`
  - `pub(crate) fn iter(&self) -> impl Iterator<Item = (&EntityKey, &EntityKey)>`

- [ ] **Step 1: Write failing tests**

```rust
#[cfg(test)]
mod tests {
    use super::*;
    fn k(e: &str, i: &str) -> EntityKey { (e.into(), i.into()) }

    #[test]
    fn u_alias_1_write_time_flattening() {
        let mut m = AliasMap::default();
        m.insert(k("p", "a"), k("p", "b")).unwrap();
        // b -> c: the stored edge for a must re-point to c (depth-1, no chains)
        m.insert(k("p", "b"), k("p", "c")).unwrap();
        assert_eq!(m.resolve(&k("p", "a")), k("p", "c"));
        assert_eq!(m.resolve(&k("p", "b")), k("p", "c"));
        // inserting x -> a flattens to x -> c at write time
        m.insert(k("p", "x"), k("p", "a")).unwrap();
        assert_eq!(m.resolve(&k("p", "x")), k("p", "c"));
        assert_eq!(m.resolve(&k("p", "unrelated")), k("p", "unrelated"));
    }

    #[test]
    fn u_alias_2_cycle_rejected() {
        let mut m = AliasMap::default();
        m.insert(k("p", "a"), k("p", "b")).unwrap();
        assert!(matches!(m.insert(k("p", "b"), k("p", "a")), Err(crate::ClogError::AliasCycle)));
        assert!(matches!(m.insert(k("p", "z"), k("p", "z")), Err(crate::ClogError::AliasCycle)));
    }

    #[test]
    fn remove_unmerges() {
        let mut m = AliasMap::default();
        m.insert(k("p", "a"), k("p", "b")).unwrap();
        m.remove(&k("p", "a"));
        assert_eq!(m.resolve(&k("p", "a")), k("p", "a"));
    }
}
```

(U-ALIAS-3 — retraction re-keys grouped views — needs the engine; it lands in Task 11.)

- [ ] **Step 2: Run** — compile error.
- [ ] **Step 3: Implement.** Note the re-point rule in `insert`: after flattening the target, iterate `edges` for entries whose value == `alias` and re-point them; then check `flattened == alias` → `AliasCycle`.
- [ ] **Step 4: Run** — 3 PASS.
- [ ] **Step 5: Commit** — `git commit -m "feat(clog): depth-1 alias map with flattening and cycle rejection (U-ALIAS-1/2)"`

---

### Task 7: Rules tier (`kinds.rs`) — U-KIND-1

**Files:**
- Create: `examples/clog/src/kinds.rs` (+ `pub(crate) mod kinds;`)

**Interfaces:**
- Consumes: `Claim`, `Matcher`, `Rule`, `KindTaxonomy`, `KindLabel`, `JudgeSource` (Task 2).
- Produces:
  - `pub(crate) struct RuleSet { /* compiled: Vec<(kind_name, Vec<CompiledRule>)> in config order; regexes pre-compiled */ }`
  - `pub(crate) fn compile(tax: &KindTaxonomy) -> Result<RuleSet, ClogError>` — bad regex → `ClogError::InvalidClaim`-style? No: bad regex in *config* → `ClogError::Corrupt { detail }` is wrong too. Use `ClogError::TemplateError` is wrong. **Add nothing**: return `ClogError::UnknownKind` is wrong. Correct call: config errors at `open` time are reported as `ClogError::InvalidClaim { index: 0, reason }`? No. **Decision (document in rustdoc): invalid rule regex → `ClogError::Corrupt { detail: "config: bad regex …" }`** — config is host-supplied state and Corrupt is the taxonomy for unusable persistent/config state. Revisit in P3 if a dedicated `InvalidConfig` variant earns its place.
  - `pub(crate) fn classify(rs: &RuleSet, c: &Claim) -> Option<KindLabel>` — first matching rule in config order wins, confidence 1.0, source Rule. A rule matches when **any** of its matchers match (§5.6 `any_of`).

- [ ] **Step 1: Write failing tests**

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::*;
    use crate::validate::tests_base_claim;

    fn tax_with_rules() -> KindTaxonomy {
        let mut tax = KindTaxonomy::default_taxonomy();
        // risk declared before question in default order? No: default order is
        // fact, decision, risk, question, ... — rules are evaluated in config order.
        for kd in &mut tax.kinds {
            match kd.name.as_str() {
                "risk" => kd.rules.push(Rule { any_of: vec![Matcher::BodyContains("overdue".into())] }),
                "question" => kd.rules.push(Rule { any_of: vec![
                    Matcher::BodyRegex(r"\?$".into()),
                    Matcher::ObserverIs("faq-bot".into()),
                ] }),
                "fact" => kd.rules.push(Rule { any_of: vec![Matcher::EntityType("bankfeed".into())] }),
                _ => {}
            }
        }
        tax
    }

    #[test]
    fn u_kind_1_first_match_wins_in_config_order() {
        let rs = compile(&tax_with_rules()).unwrap();
        let mut c = tests_base_claim();
        // matches BOTH fact (entity type) and risk (body) -> fact wins (declared first)
        c.body = "Invoice 1042 is OVERDUE".into();
        c.entities = vec![EntityRef { etype: "bankfeed".into(), id: "x".into(), name: None }];
        let k = classify(&rs, &c).unwrap();
        assert_eq!(k.kind, "fact");
        assert_eq!(k.confidence, 1.0);
        assert!(matches!(k.source, JudgeSource::Rule));

        // case-insensitive BodyContains
        c.entities.clear();
        assert_eq!(classify(&rs, &c).unwrap().kind, "risk");

        // regex matcher
        c.body = "did we sign the SOW?".into();
        assert_eq!(classify(&rs, &c).unwrap().kind, "question");

        // observer matcher (any_of)
        c.body = "no punctuation".into();
        c.observer = ObserverId::from("faq-bot");
        assert_eq!(classify(&rs, &c).unwrap().kind, "question");

        // no match -> None
        c.observer = ObserverId::from("o1");
        assert!(classify(&rs, &c).is_none());
    }

    #[test]
    fn bad_regex_rejected_at_compile() {
        let mut tax = KindTaxonomy::default_taxonomy();
        tax.kinds[0].rules.push(Rule { any_of: vec![Matcher::BodyRegex("(".into())] });
        assert!(matches!(compile(&tax), Err(ClogError::Corrupt { .. })));
    }
}
```

- [ ] **Step 2: Run** — compile error.
- [ ] **Step 3: Implement.** `BodyContains` lowercases both sides (`to_lowercase`, correct-enough for v1; document). Fix the stray non-ASCII character in the Produces note if copied (keep rustdoc ASCII).
- [ ] **Step 4: Run** — 2 PASS.
- [ ] **Step 5: Commit** — `git commit -m "feat(clog): rules-tier classifier (U-KIND-1)"`

---

### Task 8: Template parser + RFC3339 (`render/template.rs`, `render/time.rs`) — U-TMPL-1

**Files:**
- Create: `examples/clog/src/render/mod.rs` (declares submodules; renderer body comes in Task 9), `examples/clog/src/render/template.rs`, `examples/clog/src/render/time.rs` (+ `pub(crate) mod render;`)

**Interfaces:**
- Produces:
  - `pub(crate) enum SlotName { Header, Urgent, OpenLoops, Entities, Changes }`
  - `pub(crate) enum Segment { Text(String), Slot { name: SlotName, limit: Option<usize> } }`
  - `pub(crate) struct Template(pub Vec<Segment>);`
  - `pub(crate) fn parse(src: &str) -> Result<Template, ClogError>` — grammar §5.8; malformed → `ClogError::TemplateError(msg)`. No escape for literal `%{` (documented).
  - `pub(crate) const DEFAULT_TEMPLATE: &str` — byte-for-byte the spec §5.8 default.
  - `pub(crate) fn rfc3339_utc(ms: u64) -> String` — `YYYY-MM-DDTHH:MM:SSZ`, seconds precision.

- [ ] **Step 1: Write failing tests**

```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn u_tmpl_1_grammar_accept_reject() {
        // accepts
        assert!(parse("plain text no slots").is_ok());
        assert!(parse("%{header}").is_ok());
        assert!(parse("a %{urgent limit=8} b %{open_loops} c").is_ok());
        assert!(parse("%{entities limit=10}%{changes limit=6}").is_ok());
        // slot with limit parses the value
        let t = parse("%{urgent limit=3}").unwrap();
        assert!(matches!(&t.0[0], Segment::Slot { name: SlotName::Urgent, limit: Some(3) }));
        // rejects
        for bad in [
            "%{nope}",                 // unknown slot name
            "%{urgent",                // unterminated
            "%{urgent limit=}",        // empty value
            "%{urgent limit=abc}",     // non-numeric
            "%{urgent size=3}",        // unknown key
            "%{}",                     // empty slot
        ] {
            assert!(matches!(parse(bad), Err(crate::ClogError::TemplateError(_))), "{bad}");
        }
    }

    #[test]
    fn default_template_is_spec_bytes() {
        // frozen by spec §5.8; U-TMPL-3 goldens depend on this exact string
        assert!(DEFAULT_TEMPLATE.starts_with("# situation · scope: %{header}\n"));
        assert!(DEFAULT_TEMPLATE.contains("%{urgent limit=8}"));
        assert!(DEFAULT_TEMPLATE.contains("%{open_loops limit=10}"));
        assert!(DEFAULT_TEMPLATE.contains("%{entities limit=10}"));
        assert!(DEFAULT_TEMPLATE.contains("%{changes limit=6}"));
        assert!(parse(DEFAULT_TEMPLATE).is_ok());
    }

    #[test]
    fn rfc3339_known_values() {
        assert_eq!(rfc3339_utc(0), "1970-01-01T00:00:00Z");
        assert_eq!(rfc3339_utc(86_400_000), "1970-01-02T00:00:00Z");
        // 2000-03-01 is the canonical leap-era edge in the civil algorithm
        assert_eq!(rfc3339_utc(951_868_800_000), "2000-03-01T00:00:00Z");
        assert_eq!(rfc3339_utc(1_755_216_000_000), "2026-08-15T00:00:00Z");
        assert_eq!(rfc3339_utc(1_755_262_496_000), "2026-08-15T12:54:56Z");
    }
}
```

- [ ] **Step 2: Run** — compile error.

- [ ] **Step 3: Implement.** Parser: scan for `%{`, take until `}` (missing `}` → error), split on whitespace; first token is the slot name, remaining tokens must be `limit=<usize>`. The default template (exact, trailing newline included):

```rust
pub(crate) const DEFAULT_TEMPLATE: &str = "\
# situation · scope: %{header}

## urgent
%{urgent limit=8}

## open loops
%{open_loops limit=10}

## entities
%{entities limit=10}

## changes since last brief
%{changes limit=6}
";
```

`rfc3339_utc` via Howard Hinnant's civil-from-days:

```rust
pub(crate) fn rfc3339_utc(ms: u64) -> String {
    let secs = ms / 1000;
    let (days, rem) = (secs / 86_400, secs % 86_400);
    let (h, m, s) = (rem / 3600, (rem % 3600) / 60, rem % 60);
    let z = days as i64 + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let mo = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + i64::from(mo <= 2);
    format!("{y:04}-{mo:02}-{d:02}T{h:02}:{m:02}:{s:02}Z")
}
```

- [ ] **Step 4: Run** — 3 PASS.
- [ ] **Step 5: Commit** — `git commit -m "feat(clog): template parser and RFC3339 (U-TMPL-1)"`

---

### Task 9: Slot renderers + budgeter (`render/mod.rs`) — U-TMPL-2/3

**Files:**
- Modify: `examples/clog/src/render/mod.rs`

**Interfaces:**
- Consumes: `Template`, `Segment`, `SlotName`, `rfc3339_utc` (Task 8); `Reliability::letter`, `Credibility::digit` (Task 2).
- Produces (these are the engine→renderer contract; Tasks 11–15 build them):
  - `pub(crate) fn headline(body: &str) -> String` — whitespace-collapsed (`split_whitespace().join(" ")`), first 120 **chars**.
  - `pub(crate) struct UrgentItem { pub score: f32, pub headline: String, pub reliability: char, pub credibility: u8, pub claim_key: String }`
  - `pub(crate) struct LoopItem { pub kind: String, pub headline: String, pub claim_key: String }`
  - `pub(crate) struct EntityItem { pub display: String, pub summaries: Vec<String> }` — summaries newest-first.
  - `pub(crate) enum ChangeItem { Added(String), Removed(String) }` — payload is the headline.
  - `pub(crate) struct SlotInputs { pub scope: String, pub rev: u64, pub as_of_ms: u64, pub urgent: Vec<UrgentItem>, pub open_loops: Vec<LoopItem>, pub entities: Vec<EntityItem>, pub changes: Vec<ChangeItem> }`
  - `pub(crate) fn render(t: &Template, inputs: &SlotInputs, budget_chars: usize) -> String`

Item formats (frozen by goldens; spec §5.8):
- header: `{scope} · rev {rev} · {rfc3339_utc(as_of_ms)}`
- urgent: `{rank}. ({score:.1}) {headline} [{R}/{C}] ({claim_key})` — rank starts at 1
- open_loops: `- {KIND} {headline} ({claim_key})` — kind uppercased
- entities: `{display}: {summaries joined with "; "}`
- changes: `+ {headline}` / `- {headline}`

Budgeting: render all slots; if total chars > budget, drop whole items from the **end** of slots in reverse priority order — changes, then entities, then open_loops, then urgent — until under budget; each truncated slot gets a final line `… ({n} more)`. Never truncate mid-item. Empty slots render as `(none)`.

- [ ] **Step 1: Write failing tests**

```rust
#[cfg(test)]
mod render_tests {
    use super::*;
    use crate::render::template::{parse, DEFAULT_TEMPLATE};

    fn inputs() -> SlotInputs {
        SlotInputs {
            scope: "default".into(), rev: 7, as_of_ms: 86_400_000,
            urgent: vec![
                UrgentItem { score: 1.25, headline: "Invoice 1042 overdue".into(), reliability: 'B', credibility: 2, claim_key: "inv".into() },
                UrgentItem { score: 0.5, headline: "Kickoff moved".into(), reliability: 'A', credibility: 1, claim_key: "kick".into() },
            ],
            open_loops: vec![LoopItem { kind: "question".into(), headline: "Did we sign?".into(), claim_key: "q1".into() }],
            entities: vec![EntityItem { display: "Halcyon".into(), summaries: vec!["paid".into(), "kicked off".into()] }],
            changes: vec![ChangeItem::Added("Invoice 1042 overdue".into()), ChangeItem::Removed("old thing".into())],
        }
    }

    #[test]
    fn u_tmpl_3_default_template_byte_stability() {
        let out = render(&parse(DEFAULT_TEMPLATE).unwrap(), &inputs(), 6000);
        let expected = "\
# situation · scope: default · rev 7 · 1970-01-02T00:00:00Z

## urgent
1. (1.2) Invoice 1042 overdue [B/2] (inv)
2. (0.5) Kickoff moved [A/1] (kick)

## open loops
- QUESTION Did we sign? (q1)

## entities
Halcyon: paid; kicked off

## changes since last brief
+ Invoice 1042 overdue
- old thing
";
        assert_eq!(out, expected);
    }

    #[test]
    fn u_tmpl_2_budget_truncation_order() {
        // budget small enough to force dropping all changes and one entity summary line
        let t = parse(DEFAULT_TEMPLATE).unwrap();
        let full = render(&t, &inputs(), 6000);
        let tight = render(&t, &inputs(), full.len() - 1);
        // changes go first, replaced by the marker
        assert!(tight.contains("… (") && tight.contains("more)"));
        assert!(!tight.contains("- old thing"));
        // urgent survives longest
        assert!(tight.contains("1. (1.2)"));
        // never over budget
        assert!(tight.chars().count() <= full.len() - 1 || tight.contains("more)"));
    }

    #[test]
    fn per_slot_limit_caps_items() {
        let t = parse("%{urgent limit=1}").unwrap();
        let out = render(&t, &inputs(), 6000);
        assert!(out.contains("1. (1.2)"));
        assert!(!out.contains("Kickoff"));
        assert!(out.contains("… (1 more)"));
    }

    #[test]
    fn headline_collapses_and_caps() {
        assert_eq!(headline("  a\n\n b\tc  "), "a b c");
        let long = "x".repeat(300);
        assert_eq!(headline(&long).chars().count(), 120);
    }

    #[test]
    fn empty_slots_render_none() {
        let t = parse("%{changes}").unwrap();
        let mut i = inputs();
        i.changes.clear();
        assert_eq!(render(&t, &i, 6000), "(none)");
    }
}
```

- [ ] **Step 2: Run** — compile error.
- [ ] **Step 3: Implement.** Slot limit semantics: per-slot `limit` caps items *before* budgeting and contributes its own `… (n more)` if it cut anything; budget truncation appends/updates the marker with the total hidden count. Items within a slot joined by `\n`.
- [ ] **Step 4: Run** — 5 PASS.
- [ ] **Step 5: Commit** — `git commit -m "feat(clog): slot renderers and budgeter (U-TMPL-2/3)"`

---

### Task 10: Engine contract (`engine/mod.rs`)

**Files:**
- Create: `examples/clog/src/engine/mod.rs` (+ `pub(crate) mod engine;`)

**Interfaces:**
- Consumes: types from Task 2, `AliasMap`/`EntityKey` (Task 6), `KindLabel`.
- Produces:
  - `pub(crate) struct StoredClaim { pub claim: Claim, pub recorded_at: u64 }` (serde)
  - `pub(crate) enum Event { Observe(StoredClaim), Retract { claim_key: String }, Revoke { observer: ObserverId }, SetFocus { scope: String, focus: Focus }, Judge { claim_key: String, kind: String, confidence: f32, source: JudgeSource }, Tick { epoch: u64 } }` (serde — the WAL format)
  - `pub(crate) struct Batch { pub rev: Rev, pub events: Vec<Event> }` (serde)
  - `#[derive(Clone)] pub(crate) struct WorldViews { pub claims: OrdMap<String, StoredClaim>, pub kinds: OrdMap<String, KindLabel>, pub unclassified: OrdSet<String>, pub by_subject: OrdMap<String, OrdSet<String>>, pub by_observer: OrdMap<String, OrdSet<String>>, pub by_entity: OrdMap<EntityKey, OrdSet<String>>, pub aliases: AliasMap, pub names: OrdMap<EntityKey, (u64, String)>, pub believed: OrdMap<String, Option<String>>, pub open_loops: OrdSet<String>, pub urgent: OrdMap<String, Vec<(f32, String)>> }`
    - `claims` holds **all** live claims including reserved `clog:*` ones; every view accessor and `select` filters reserved keys out (INV-8). `believed` maps subject_key → winning claim_key (`None` = all-floored group).
    - `urgent` vectors are sorted score-desc, tie claim_key-asc, truncated to the scope's top_k.
  - `pub(crate) struct ApplyResult { pub touched: bool }` — P1's naive re-render recomputes every scope's slot inputs per batch (auditable oracle; see build design §5); `touched=false` short-circuits when a batch applied zero effective events. Richer per-view diffs arrive in P2 when wakes need them.
  - `pub(crate) trait Engine: Send { fn apply(&mut self, events: &[Event], scopes: &BTreeMap<String, Focus>, now_ms: u64) -> ApplyResult; fn views(&self) -> &WorldViews; }`

- [ ] **Step 1: Write the failing test** — serde stability of the WAL types

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use crate::validate::tests_base_claim;

    #[test]
    fn batch_round_trips_postcard() {
        let b = Batch { rev: 3, events: vec![
            Event::Observe(StoredClaim { claim: tests_base_claim(), recorded_at: 9 }),
            Event::Retract { claim_key: "k1".into() },
            Event::Judge { claim_key: "k1".into(), kind: "risk".into(), confidence: 1.0, source: crate::JudgeSource::Rule },
            Event::Tick { epoch: 4 },
        ]};
        let bytes = postcard::to_allocvec(&b).unwrap();
        let b2: Batch = postcard::from_bytes(&bytes).unwrap();
        assert_eq!(b2.rev, 3);
        assert_eq!(b2.events.len(), 4);
    }
}
```

- [ ] **Step 2: Run** — compile error. **Step 3: Implement** the types above (plus `mod naive;` stub left empty until Task 11). **Step 4: Run** — PASS. **Step 5: Commit** — `git commit -m "feat(clog): engine contract, event and batch types"`

---

### Task 11: Naive engine, part 1 — observe/retract/judge (`engine/naive.rs`)

**Files:**
- Create: `examples/clog/src/engine/naive.rs`

**Interfaces:**
- Consumes: Task 10 contract; `belief::resolve` (Task 5), `score::score_claim` (Task 4), `AliasMap` (Task 6).
- Produces: `pub(crate) struct NaiveEngine { views: WorldViews, cfg: NaiveCfg }` with `pub(crate) fn new(cfg: NaiveCfg) -> Self` and `impl Engine for NaiveEngine`. `pub(crate) struct NaiveCfg { pub loop_kinds: Vec<String>, pub top_k: usize, pub buckets_per_half_life: u32, pub belief_floor: Credibility }`.

Apply semantics per event (P1 subset; `SetFocus`/`Tick` are accepted but only `SetFocus` on an unknown scope is impossible here — the actor rejects them until P2; `Revoke` handled in Task 12):
- `Observe(sc)`: insert into `claims`; index into `by_subject`/`by_observer`/`by_entity` (entity keys resolved through `aliases`); update `names` for each entity carrying a name (latest `recorded_at` wins); if the key is a **merge claim** (`clog:merge:` prefix), parse its body (`{"alias":{"etype":..,"id":..},"canonical":{..}}` JSON — hand-rolled parse with `regex` or simple string ops is NOT acceptable; store the two `EntityKey`s postcard-encoded in the body as base64? **No.** Decision: merge claim body is `alias.etype\u{1f}alias.id\u{1f}canonical.etype\u{1f}canonical.id` joined with the ASCII unit separator — trivially split, no JSON dep; document in rustdoc and the ledger) → `aliases.insert` and re-key `by_entity` groups that resolved differently before/after; recompute `believed` for the claim's subject group; recompute `open_loops`/`unclassified` membership for this key; mark all scopes dirty.
- `Retract { claim_key }`: remove from `claims` and all indexes; if merge claim → `aliases.remove` and re-key affected `by_entity` groups (U-ALIAS-3); drop its `kinds` entry; recompute its subject group's `believed`; remove from `open_loops`/`unclassified`.
- `Judge { claim_key, .. }`: upsert `kinds[claim_key]`; recompute `unclassified` (remove) and `open_loops` (member iff kind ∈ loop_kinds and claim live and non-reserved).
- After all events: recompute `urgent[scope]` for every scope from all live non-reserved claims (full re-score — the naive engine is the auditable oracle; targeted scoring arrives with fold in P4 if benches demand it earlier, they won't at fixture scale).

`unclassified` membership: live, non-reserved, and either no `kinds` entry or `kinds[key].confidence < 1.0` with source ≠ External — for P1 simply: no `kinds` entry.

- [ ] **Step 1: Write failing tests**

```rust
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
    }
}
```

- [ ] **Step 2: Run** — compile error / failures.
- [ ] **Step 3: Implement** per the semantics above. Keep every recompute a small private fn (`reindex_claim`, `unindex_claim`, `recompute_belief(subject)`, `recompute_membership(key)`, `recompute_urgent(scopes, now)`) so the code stays auditable-by-eye (spec §6.2 requirement).
- [ ] **Step 4: Run** — 5 PASS.
- [ ] **Step 5: Commit** — `git commit -m "feat(clog): naive engine observe/retract/judge with belief and urgent views"`

---

### Task 12: Naive engine, part 2 — revoke, alias re-keying, entity registry (U-ALIAS-3)

**Files:**
- Modify: `examples/clog/src/engine/naive.rs`

**Interfaces:**
- Consumes/Produces: same `Engine` impl; adds `Revoke` handling and completes alias/registry semantics.

Semantics:
- `Revoke { observer }`: expand via `by_observer[observer]` to retractions of every live claim of that observer **including reserved claims** (INV-6; spec §5.1), applied in claim_key order (determinism).
- Alias insert/retract must re-key `by_entity` and re-resolve `names` (registry keyed by canonical key; on un-merge, names recompute from remaining claims' latest-seen).
- `entity_state` accessor: `pub(crate) fn entity_state(views: &WorldViews) -> Vec<(EntityKey, String, Vec<(String, StoredClaim)>)>` — per canonical entity (sorted): display name (registry name or `etype:id`), and believed claims for subjects touching that entity, newest-first by `occurred_at` (tie: claim_key), capped at 8 (spec internal constant N=8).

- [ ] **Step 1: Write failing tests**

```rust
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
```

- [ ] **Step 2: Run** — failures. **Step 3: Implement.** **Step 4: Run** — 3 new PASS, all prior green. **Step 5: Commit** — `git commit -m "feat(clog): revoke expansion, alias re-keying, entity registry (U-ALIAS-3, INV-6)"`

---

### Task 13: WAL (`wal.rs`)

**Files:**
- Create: `examples/clog/src/wal.rs` (+ `pub(crate) mod wal;`)

**Interfaces:**
- Consumes: `Batch` (Task 10), `FsyncPolicy`, `ClogError`.
- Produces:
  - `pub(crate) struct Wal { /* file handle, path, fsync policy */ }`
  - `pub(crate) fn open_dir(dir: &Path, fsync: FsyncPolicy) -> Result<(Wal, Vec<Batch>), ClogError>` — creates `dir/wal/log`, replays existing records; a torn/corrupt tail is truncated from the log and the removed bytes are appended to `dir/wal/wal.corrupt` (R2); any record failing CRC ends replay the same way (never applied, never panics).
  - `pub(crate) fn append(&mut self, batch: &Batch) -> Result<(), ClogError>` — frame: `[len: u32 LE][crc32(payload): u32 LE][payload = postcard(batch)]`; fsync per policy after write.

- [ ] **Step 1: Write failing tests**

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine::{Batch, Event};
    use std::io::{Read, Seek, SeekFrom, Write};

    fn batch(rev: u64) -> Batch { Batch { rev, events: vec![Event::Tick { epoch: rev }] } }

    #[test]
    fn append_and_replay_round_trip() {
        let dir = tempfile::tempdir().unwrap();
        {
            let (mut w, replayed) = open_dir(dir.path(), crate::FsyncPolicy::OnCommit).unwrap();
            assert!(replayed.is_empty());
            w.append(&batch(1)).unwrap();
            w.append(&batch(2)).unwrap();
        }
        let (_, replayed) = open_dir(dir.path(), crate::FsyncPolicy::OnCommit).unwrap();
        assert_eq!(replayed.iter().map(|b| b.rev).collect::<Vec<_>>(), vec![1, 2]);
    }

    #[test]
    fn r2_torn_tail_truncated_and_quarantined() {
        let dir = tempfile::tempdir().unwrap();
        {
            let (mut w, _) = open_dir(dir.path(), crate::FsyncPolicy::OnCommit).unwrap();
            w.append(&batch(1)).unwrap();
            w.append(&batch(2)).unwrap();
        }
        // tear the last record: chop 3 bytes off the file
        let log = dir.path().join("wal").join("log");
        let len = std::fs::metadata(&log).unwrap().len();
        let f = std::fs::OpenOptions::new().write(true).open(&log).unwrap();
        f.set_len(len - 3).unwrap();
        let (_, replayed) = open_dir(dir.path(), crate::FsyncPolicy::OnCommit).unwrap();
        assert_eq!(replayed.iter().map(|b| b.rev).collect::<Vec<_>>(), vec![1]);
        assert!(dir.path().join("wal").join("wal.corrupt").exists());
        // reopening again is clean (tail already truncated)
        let (_, replayed) = open_dir(dir.path(), crate::FsyncPolicy::OnCommit).unwrap();
        assert_eq!(replayed.len(), 1);
    }

    #[test]
    fn r2_corrupt_crc_never_applied_never_panics() {
        let dir = tempfile::tempdir().unwrap();
        {
            let (mut w, _) = open_dir(dir.path(), crate::FsyncPolicy::OnCommit).unwrap();
            w.append(&batch(1)).unwrap();
            w.append(&batch(2)).unwrap();
        }
        let log = dir.path().join("wal").join("log");
        // flip a byte in the last record's payload
        let mut bytes = std::fs::read(&log).unwrap();
        let n = bytes.len();
        bytes[n - 1] ^= 0xFF;
        std::fs::write(&log, &bytes).unwrap();
        let (_, replayed) = open_dir(dir.path(), crate::FsyncPolicy::OnCommit).unwrap();
        assert_eq!(replayed.iter().map(|b| b.rev).collect::<Vec<_>>(), vec![1]);
    }
}
```

- [ ] **Step 2: Run** — compile error. **Step 3: Implement.** Replay reads frames sequentially; on short read, bad len (> remaining), or CRC mismatch: copy the offending tail bytes to `wal.corrupt` (append mode), `set_len` the log to the last good offset, stop. **Step 4: Run** — 3 PASS. **Step 5: Commit** — `git commit -m "feat(clog): postcard+crc32 WAL with torn-tail quarantine (R2)"`

---

### Task 14: Clock, actor, and the write path (`clock.rs`, `actor.rs`, `lib.rs`)

**Files:**
- Create: `examples/clog/src/clock.rs`, `examples/clog/src/actor.rs`
- Modify: `examples/clog/src/lib.rs`
- Test: `examples/clog/tests/api.rs` (integration; public API only)

**Interfaces:**
- Consumes: everything prior.
- Produces the public handle (spec §4 subset):
  - `pub struct Clog` — `Clone + Send + Sync`.
  - `pub fn open(cfg: Config) -> Result<Clog, ClogError>` — opens WAL, replays batches into a fresh `NaiveEngine` (pure application — no classifier on replay), rebuilds render state batch-by-batch so situation revs are reproduced, spawns the writer thread, publishes the initial snapshot. `"default"` scope injected if absent. `rebuild_on_open` is accepted and (P1) identical to normal open.
  - `pub fn observe(&self, claims: Vec<Claim>, opts: ObserveOpts) -> Result<Ack, ClogError>`
  - `pub fn retract(&self, claim_key: &str) -> Result<Ack, ClogError>` — `UnknownClaim` if not live.
  - `pub fn situation(&self, scope: Option<&str>, template: Option<&str>) -> Result<Situation, ClogError>` — default template → clone of stored `Situation` (INV-1); custom template → parse (`TemplateError` rejects the call only) and assemble from the snapshot's stored `SlotInputs` (string assembly only, no view computation).
  - `pub fn advance(&self, ms: u64) -> Result<(), ClogError>` — Manual clock only (`ManualClockRequired` otherwise); moves the clock (no Tick events in P1).
  - Internal: `clock.rs` — `pub(crate) enum Clock { System, Manual(Arc<AtomicU64>) }` with `pub(crate) fn now_ms(&self) -> u64` (System = `SystemTime::now()` since epoch; the *only* wall-clock read in the crate).
  - Internal: `actor.rs` — `enum Cmd { Write(WriteReq), Advance(u64, Sender<()>), Shutdown }`; writer loop owns `NaiveEngine + Wal + RenderState`; `WorldSnapshot { rev: Rev, as_of: u64, views: WorldViews, scopes: BTreeMap<String, Focus>, situations: BTreeMap<String, SituationState> }`; `SituationState { situation: Situation, inputs: SlotInputs, membership: OrdSet<String> }` (membership = urgent ∪ open_loops at last render, for the changes slot); published via `arc_swap::ArcSwap<WorldSnapshot>`; replies over `crossbeam_channel::bounded(1)`.

Write path per batch (build design §4 order):
1. Validate all claims (whole batch rejected on first failure, atomic).
2. `recorded_at = clock.now_ms()` for every claim in the batch.
3. Expand upserts against current live claims: identical (`==` ignoring nothing — `StoredClaim.claim == new claim`) → skip entirely (INV-5); different → `Retract(old)` + `Observe(new)` (INV-4).
4. Run rules tier for observed claims lacking a kind → append `Judge` events (source Rule, confidence 1.0).
5. If the effective event list is empty → no commit: `Ack { rev: current, situation: opt }`.
6. `rev += 1`; `Batch { rev, events }` → `wal.append` (fsync per policy) — **then** `engine.apply`.
7. Recompute `SlotInputs` per scope from views (naive full recompute); render each scope's default template; if text differs from stored, update `SituationState { situation: Situation { rev, as_of: now }, .. }` and compute the changes delta (added/removed membership vs. previous, adds then removes, each claim_key-asc, headlines from views).
8. Build new `WorldSnapshot`, `ArcSwap::store`.
9. Reply `Ack { rev, situation }` (situation populated when `opts.return_situation` names a scope; `UnknownScope` if it doesn't exist).

Shutdown: `Clog` wraps `Arc<Inner>`; `Inner: Drop` sends `Shutdown` and joins the writer (fsync WAL). All calls after shutdown → `ShuttingDown`.

- [ ] **Step 1: Write failing integration tests** (`tests/api.rs`)

```rust
use clog::*;

fn cfg(dir: &std::path::Path) -> Config {
    let mut c = Config::default_for(dir);
    c.tick = TickConfig { mode: ClockMode::Manual, interval_ms: 60_000 };
    c
}

fn claim(key: &str, body: &str, occ: u64) -> Claim {
    Claim {
        claim_key: key.into(), subject_key: None, source_ref: "t:1".into(),
        observer: ObserverId::from("test"), schema_v: 1,
        occurred_at: occ, observed_at: occ,
        reliability: Reliability::B, credibility: Credibility::Two,
        entities: vec![], body: body.into(),
    }
}

#[test]
fn observe_bumps_rev_and_renders() {
    let dir = tempfile::tempdir().unwrap();
    let c = Clog::open(cfg(dir.path())).unwrap();
    c.advance(1_000_000).unwrap();
    let ack = c.observe(vec![claim("a", "first thing", 500_000)], ObserveOpts::default()).unwrap();
    assert_eq!(ack.rev, 1);
    let s = c.situation(None, None).unwrap();
    assert_eq!(s.scope, "default");
    assert_eq!(s.rev, 1);
    assert!(s.text.contains("first thing"));
}

#[test]
fn inv5_duplicate_observe_is_invisible() {
    let dir = tempfile::tempdir().unwrap();
    let c = Clog::open(cfg(dir.path())).unwrap();
    c.advance(1_000_000).unwrap();
    let a1 = c.observe(vec![claim("a", "x", 500_000)], ObserveOpts::default()).unwrap();
    let s1 = c.situation(None, None).unwrap();
    let a2 = c.observe(vec![claim("a", "x", 500_000)], ObserveOpts::default()).unwrap();
    let s2 = c.situation(None, None).unwrap();
    assert_eq!(a2.rev, a1.rev, "duplicate batch must not commit");
    assert_eq!(s1.rev, s2.rev);
    assert_eq!(s1.text, s2.text);
}

#[test]
fn inv4_upsert_supersedes() {
    let dir = tempfile::tempdir().unwrap();
    let c = Clog::open(cfg(dir.path())).unwrap();
    c.advance(1_000_000).unwrap();
    c.observe(vec![claim("a", "old body", 500_000)], ObserveOpts::default()).unwrap();
    c.observe(vec![claim("a", "new body", 600_000)], ObserveOpts::default()).unwrap();
    let s = c.situation(None, None).unwrap();
    assert!(s.text.contains("new body"));
    assert!(!s.text.contains("old body"));
}

#[test]
fn inv3_retraction_heals_text() {
    let dir = tempfile::tempdir().unwrap();
    let c = Clog::open(cfg(dir.path())).unwrap();
    c.advance(1_000_000).unwrap();
    let empty = c.situation(None, None).unwrap();
    c.observe(vec![claim("a", "temp", 500_000)], ObserveOpts::default()).unwrap();
    c.retract("a").unwrap();
    let healed = c.situation(None, None).unwrap();
    assert_eq!(healed.text.replace(&format!("rev {}", healed.rev), "REV"),
               empty.text.replace(&format!("rev {}", empty.rev), "REV"));
    assert!(matches!(c.retract("a"), Err(ClogError::UnknownClaim)));
}

#[test]
fn inv9_rev_survives_reopen() {
    let dir = tempfile::tempdir().unwrap();
    {
        let c = Clog::open(cfg(dir.path())).unwrap();
        c.advance(1_000_000).unwrap();
        c.observe(vec![claim("a", "x", 500_000)], ObserveOpts::default()).unwrap();
        c.observe(vec![claim("b", "y", 500_000)], ObserveOpts::default()).unwrap();
    } // drop -> clean shutdown
    let c = Clog::open(cfg(dir.path())).unwrap();
    c.advance(2_000_000).unwrap();
    let ack = c.observe(vec![claim("c", "z", 500_000)], ObserveOpts::default()).unwrap();
    assert_eq!(ack.rev, 3);
    let s = c.situation(None, None).unwrap();
    assert!(s.text.contains('x') && s.text.contains('z'));
}

#[test]
fn reserved_namespace_rejected_and_batch_atomic() {
    let dir = tempfile::tempdir().unwrap();
    let c = Clog::open(cfg(dir.path())).unwrap();
    c.advance(1_000_000).unwrap();
    let r = c.observe(vec![claim("ok", "fine", 500_000), claim("clog:sneaky", "no", 500_000)],
                      ObserveOpts::default());
    assert!(r.is_err());
    // atomic: the valid claim must not have landed either
    assert!(!c.situation(None, None).unwrap().text.contains("fine"));
}

#[test]
fn rules_tier_classifies_at_commit() {
    let dir = tempfile::tempdir().unwrap();
    let mut config = cfg(dir.path());
    for kd in &mut config.kinds.kinds {
        if kd.name == "risk" {
            kd.rules.push(clog::Rule { any_of: vec![clog::Matcher::BodyContains("overdue".into())] });
        }
    }
    let c = Clog::open(config).unwrap();
    c.advance(1_000_000).unwrap();
    c.observe(vec![claim("inv", "invoice 1042 is overdue", 500_000)], ObserveOpts::default()).unwrap();
    let s = c.situation(None, None).unwrap();
    assert!(s.text.contains("- RISK invoice 1042"), "open loops slot should show it:\n{}", s.text);
}

#[test]
fn custom_template_and_errors() {
    let dir = tempfile::tempdir().unwrap();
    let c = Clog::open(cfg(dir.path())).unwrap();
    c.advance(1_000_000).unwrap();
    c.observe(vec![claim("a", "hello world", 500_000)], ObserveOpts::default()).unwrap();
    let s = c.situation(None, Some("URGENT ONLY\n%{urgent limit=1}")).unwrap();
    assert!(s.text.starts_with("URGENT ONLY\n1."));
    assert!(matches!(c.situation(None, Some("%{bogus}")), Err(ClogError::TemplateError(_))));
    assert!(matches!(c.situation(Some("nope"), None), Err(ClogError::UnknownScope)));
}
```

- [ ] **Step 2: Run** — `cargo test -p clog --test api` → compile error.
- [ ] **Step 3: Implement** `clock.rs`, `actor.rs`, and the `Clog` handle in `lib.rs` per the write-path list above. Keep the writer loop a single `fn run(state: WriterState, rx: Receiver<Cmd>)` with one match; each numbered write-path step is its own private fn. On open: replay applies each batch's events directly (`engine.apply`), then re-renders — reproducing `SituationState` and revs deterministically.
- [ ] **Step 4: Run** — 8 PASS (plus all unit tests still green).
- [ ] **Step 5: Commit** — `git commit -m "feat(clog): actor, write path, WAL-backed open, situation reads (INV-1/3/4/5/9)"`

---

### Task 15: `select`, `merge_entities`, `revoke_observer`

**Files:**
- Modify: `examples/clog/src/actor.rs`, `examples/clog/src/lib.rs`
- Test: `examples/clog/tests/api.rs` (extend)

**Interfaces:**
- Produces:
  - `pub fn select(&self, view: View, filter: Filter) -> Result<Vec<Row>, ClogError>` — snapshot-only. Ordering: Live/OpenLoops/Unclassified by claim_key asc; Urgent by rank; EntityState by (canonical entity, subject) asc. `min_score` with a non-Urgent view → `InvalidFilter`. `limit` default 50, clamped to 500. Reserved claims never appear (INV-8). Filters: kinds (any-of), entities (any-of, post-alias), observer, subject_prefix, occurred_after, min_score.
  - `pub fn revoke_observer(&self, observer: &ObserverId) -> Result<Ack, ClogError>` — one batch, one rev (INV-6).
  - `pub fn merge_entities(&self, alias: &EntityRef, canonical: &EntityRef) -> Result<Ack, ClogError>` — writes the reserved merge claim (key `clog:merge:{a.etype}:{a.id}->{c.etype}:{c.id}`, observer `clog`, body = unit-separator-joined keys, reliability A, credibility One, occurred/observed/recorded = now, source_ref `clog:merge`); pre-checks `AliasCycle` against the current alias map before committing; `retract` of that key un-merges.

- [ ] **Step 1: Write failing tests** (append to `tests/api.rs`)

```rust
#[test]
fn select_live_with_filters() {
    let dir = tempfile::tempdir().unwrap();
    let c = Clog::open(cfg(dir.path())).unwrap();
    c.advance(1_000_000).unwrap();
    let mut a = claim("a", "alpha", 100_000);
    a.observer = ObserverId::from("gmail");
    let mut b = claim("b", "beta", 900_000);
    b.observer = ObserverId::from("twist");
    c.observe(vec![a, b], ObserveOpts::default()).unwrap();

    let all = c.select(View::Live, Filter::default()).unwrap();
    assert_eq!(all.iter().map(|r| r.claim.claim_key.as_str()).collect::<Vec<_>>(), vec!["a", "b"]);
    assert!(all[0].recorded_at >= 1_000_000);

    let f = Filter { observer: Some(ObserverId::from("twist")), ..Filter::default() };
    assert_eq!(c.select(View::Live, f).unwrap().len(), 1);

    let f = Filter { occurred_after: Some(500_000), ..Filter::default() };
    assert_eq!(c.select(View::Live, f).unwrap()[0].claim.claim_key, "b");

    let f = Filter { min_score: Some(0.1), ..Filter::default() };
    assert!(matches!(c.select(View::Live, f), Err(ClogError::InvalidFilter { .. })));

    let rows = c.select(View::Urgent { scope: "default".into() }, Filter::default()).unwrap();
    assert!(rows[0].score.is_some());
}

#[test]
fn inv6_revoke_observer_one_batch() {
    let dir = tempfile::tempdir().unwrap();
    let c = Clog::open(cfg(dir.path())).unwrap();
    c.advance(1_000_000).unwrap();
    let mut a = claim("a", "alpha", 100_000); a.observer = ObserverId::from("gmail");
    let mut b = claim("b", "beta", 100_000);  b.observer = ObserverId::from("gmail");
    c.observe(vec![a, b], ObserveOpts::default()).unwrap();
    let ack = c.revoke_observer(&ObserverId::from("gmail")).unwrap();
    assert_eq!(ack.rev, 2); // one batch, one rev
    assert!(c.select(View::Live, Filter::default()).unwrap().is_empty());
}

#[test]
fn p7_shape_merge_round_trip_via_api() {
    let dir = tempfile::tempdir().unwrap();
    let c = Clog::open(cfg(dir.path())).unwrap();
    c.advance(1_000_000).unwrap();
    let mut cl = claim("about-a", "note about a", 100_000);
    cl.entities = vec![EntityRef { etype: "p".into(), id: "a".into(), name: None }];
    c.observe(vec![cl], ObserveOpts::default()).unwrap();
    let before = c.situation(None, None).unwrap();

    let a = EntityRef { etype: "p".into(), id: "a".into(), name: None };
    let b = EntityRef { etype: "p".into(), id: "b".into(), name: None };
    c.merge_entities(&a, &b).unwrap();
    // entity filter follows the alias
    let f = Filter { entities: Some(vec![b.clone()]), ..Filter::default() };
    assert_eq!(c.select(View::Live, f).unwrap().len(), 1);
    // cycle rejected
    assert!(matches!(c.merge_entities(&b, &a), Err(ClogError::AliasCycle)));
    // merge claim is invisible (INV-8)
    assert!(c.select(View::Live, Filter::default()).unwrap().iter().all(|r| !r.claim.claim_key.starts_with("clog:")));
    // un-merge by retracting the reserved key
    c.retract("clog:merge:p:a->p:b").unwrap();
    let after = c.situation(None, None).unwrap();
    assert_eq!(before.text.replace(&format!("rev {}", before.rev), "R"),
               after.text.replace(&format!("rev {}", after.rev), "R"));
}
```

- [ ] **Step 2: Run** — failures. **Step 3: Implement.** `select` runs entirely on the loaded snapshot (INV-1): iterate the view's ordered keys, hydrate `Row { claim, recorded_at, kind, score, believed }` (`believed = Some(views.believed[subject] == Some(key))` when the claim has a subject_key; `score` only for Urgent). **Step 4: Run** — 3 PASS. **Step 5: Commit** — `git commit -m "feat(clog): select, merge_entities, revoke_observer (INV-6/8)"`

---

### Task 16: Property tests P1–P4, P6, P7

**Files:**
- Create: `examples/clog/tests/props.rs`

**Interfaces:**
- Consumes: public API only, Manual clock.

Strategy: a claim generator over small alphabets so collisions occur — keys from `k0..k7`, subjects from `{None, s0, s1, s2}`, observers `{o0, o1}`, entities `{(p,a),(p,b)}`, bodies 1–3 words from a 6-word list, occurred_at in 1..=5 (ms scale is irrelevant), reliability/credibility across full range. Each property builds two `Clog` instances in tempdirs with identical Manual-clock scripts and compares **normalized situation text** (rev markers stripped, as in Task 14's INV-3 test) and `select(Live)` rows.

- [ ] **Step 1: Write the failing tests**

```rust
use clog::*;
use proptest::prelude::*;

// -- generators ---------------------------------------------------------
fn arb_claim() -> impl Strategy<Value = Claim> {
    (0..8u8, prop::option::of(0..3u8), 0..2u8, 0..6u8, 0..6u8, 1..6u64, prop::collection::vec(0..6u8, 1..4), prop::bool::ANY)
        .prop_map(|(k, s, o, rel, cred, occ, words, with_ent)| {
            let vocab = ["invoice", "overdue", "kickoff", "moved", "question", "paid"];
            Claim {
                claim_key: format!("k{k}"),
                subject_key: s.map(|s| format!("s{s}")),
                source_ref: "prop:1".into(),
                observer: ObserverId::from(if o == 0 { "o0" } else { "o1" }),
                schema_v: 1,
                occurred_at: occ, observed_at: occ,
                reliability: [Reliability::A, Reliability::B, Reliability::C, Reliability::D, Reliability::E, Reliability::F][rel as usize],
                credibility: [Credibility::One, Credibility::Two, Credibility::Three, Credibility::Four, Credibility::Five, Credibility::Six][cred as usize],
                entities: if with_ent { vec![EntityRef { etype: "p".into(), id: "a".into(), name: None }] } else { vec![] },
                body: words.iter().map(|w| vocab[*w as usize]).collect::<Vec<_>>().join(" "),
            }
        })
}

fn open_manual(dir: &std::path::Path) -> Clog {
    let mut c = Config::default_for(dir);
    c.tick = TickConfig { mode: ClockMode::Manual, interval_ms: 60_000 };
    let h = Clog::open(c).unwrap();
    h.advance(1_000_000).unwrap();
    h
}

fn norm(s: &Situation) -> String {
    // strip the rev so text comparison ignores counters (INV-3 wording)
    let mut t = s.text.clone();
    t = t.replace(&format!("rev {}", s.rev), "rev _");
    t
}

fn live_keys(c: &Clog) -> Vec<String> {
    c.select(View::Live, Filter::default()).unwrap().into_iter().map(|r| r.claim.claim_key).collect()
}

proptest! {
    #![proptest_config(ProptestConfig { cases: 64, ..ProptestConfig::default() })]

    // P1 (INV-3): observe everything, retract all -> empty-world text
    #[test]
    fn p1_retract_all_heals(claims in prop::collection::vec(arb_claim(), 1..12)) {
        let d = tempfile::tempdir().unwrap();
        let c = open_manual(d.path());
        let empty = norm(&c.situation(None, None).unwrap());
        for cl in &claims { let _ = c.observe(vec![cl.clone()], ObserveOpts::default()); }
        for k in live_keys(&c) { c.retract(&k).unwrap(); }
        prop_assert_eq!(norm(&c.situation(None, None).unwrap()), empty);
    }

    // P2 (INV-4): only the last version per key matters
    #[test]
    fn p2_last_writer_wins(claims in prop::collection::vec(arb_claim(), 1..12)) {
        let d1 = tempfile::tempdir().unwrap();
        let full = open_manual(d1.path());
        for cl in &claims { let _ = full.observe(vec![cl.clone()], ObserveOpts::default()); }

        let mut last: std::collections::BTreeMap<String, Claim> = Default::default();
        for cl in &claims { last.insert(cl.claim_key.clone(), cl.clone()); }
        let d2 = tempfile::tempdir().unwrap();
        let compact = open_manual(d2.path());
        for cl in last.values() { let _ = compact.observe(vec![cl.clone()], ObserveOpts::default()); }

        prop_assert_eq!(norm(&full.situation(None, None).unwrap()), norm(&compact.situation(None, None).unwrap()));
        prop_assert_eq!(live_keys(&full), live_keys(&compact));
    }

    // P3 (INV-5): duplicating a prefix changes nothing, incl. scope revs
    #[test]
    fn p3_duplicates_invisible(claims in prop::collection::vec(arb_claim(), 1..8), cut in 0..8usize) {
        let cut = cut.min(claims.len());
        let d1 = tempfile::tempdir().unwrap();
        let a = open_manual(d1.path());
        for cl in &claims { let _ = a.observe(vec![cl.clone()], ObserveOpts::default()); }
        let s_a = a.situation(None, None).unwrap();

        let d2 = tempfile::tempdir().unwrap();
        let b = open_manual(d2.path());
        for cl in &claims { let _ = b.observe(vec![cl.clone()], ObserveOpts::default()); }
        for cl in claims.iter().take(cut) {
            // replay a prefix of stale versions: only claims still live in identical
            // form are true duplicates; superseded keys will upsert — so restrict to
            // claims whose key's final version is this version
            if claims.iter().rev().find(|c2| c2.claim_key == cl.claim_key).map(|c2| c2 == cl).unwrap_or(false) {
                let _ = b.observe(vec![cl.clone()], ObserveOpts::default());
            }
        }
        let s_b = b.situation(None, None).unwrap();
        prop_assert_eq!(s_a.rev, s_b.rev, "duplicate observes must not advance situation rev");
        prop_assert_eq!(s_a.text, s_b.text);
    }

    // P4 (INV-6): revoke == retract-each
    #[test]
    fn p4_revoke_equals_retract_each(claims in prop::collection::vec(arb_claim(), 1..12)) {
        let d1 = tempfile::tempdir().unwrap();
        let a = open_manual(d1.path());
        let d2 = tempfile::tempdir().unwrap();
        let b = open_manual(d2.path());
        for cl in &claims {
            let _ = a.observe(vec![cl.clone()], ObserveOpts::default());
            let _ = b.observe(vec![cl.clone()], ObserveOpts::default());
        }
        let _ = a.revoke_observer(&ObserverId::from("o0"));
        for r in b.select(View::Live, Filter { observer: Some(ObserverId::from("o0")), ..Filter::default() }).unwrap() {
            b.retract(&r.claim.claim_key).unwrap();
        }
        prop_assert_eq!(norm(&a.situation(None, None).unwrap()), norm(&b.situation(None, None).unwrap()));
        prop_assert_eq!(live_keys(&a), live_keys(&b));
    }

    // P6: belief winner is arrival-order-insensitive
    #[test]
    fn p6_belief_order_insensitive(mut claims in prop::collection::vec(arb_claim(), 2..8), seed in 0..1000u64) {
        for (i, c) in claims.iter_mut().enumerate() {
            c.subject_key = Some("shared".into());
            c.claim_key = format!("k{i}"); // distinct keys, same subject
        }
        let d1 = tempfile::tempdir().unwrap();
        let a = open_manual(d1.path());
        for cl in &claims { a.observe(vec![cl.clone()], ObserveOpts::default()).unwrap(); }

        // deterministic shuffle
        let mut shuffled = claims.clone();
        let mut s = seed;
        for i in (1..shuffled.len()).rev() {
            s = s.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
            shuffled.swap(i, (s as usize) % (i + 1));
        }
        let d2 = tempfile::tempdir().unwrap();
        let b = open_manual(d2.path());
        for cl in &shuffled { b.observe(vec![cl.clone()], ObserveOpts::default()).unwrap(); }

        let believed = |h: &Clog| h.select(View::EntityState, Filter::default()).unwrap()
            .into_iter().map(|r| r.claim.claim_key).collect::<Vec<_>>();
        prop_assert_eq!(believed(&a), believed(&b));
    }

    // P7: merge round-trip is invisible
    #[test]
    fn p7_merge_round_trip(claims in prop::collection::vec(arb_claim(), 1..8)) {
        let d1 = tempfile::tempdir().unwrap();
        let a = open_manual(d1.path());
        for cl in &claims { let _ = a.observe(vec![cl.clone()], ObserveOpts::default()); }
        let before = norm(&a.situation(None, None).unwrap());
        let al = EntityRef { etype: "p".into(), id: "a".into(), name: None };
        let ca = EntityRef { etype: "p".into(), id: "b".into(), name: None };
        a.merge_entities(&al, &ca).unwrap();
        a.retract("clog:merge:p:a->p:b").unwrap();
        prop_assert_eq!(norm(&a.situation(None, None).unwrap()), before);
    }
}
```

- [ ] **Step 2: Run** — `cargo test -p clog --test props` → expect compile errors first, then possible real counterexamples. **Any counterexample is a bug in Tasks 11–15 — fix the engine, never weaken the property.** (P3's guard is the plan's understanding of "duplicate"; if it still fails, re-read INV-5 before touching the test.)
- [ ] **Step 3/4: Fix until green.** Run with `PROPTEST_CASES=256` once locally for confidence.
- [ ] **Step 5: Commit** — `git commit -m "test(clog): property suite P1-P4, P6, P7"`

---

### Task 17: Golden simulation G1 (insta)

**Files:**
- Create: `examples/clog/tests/g1_agency.rs`, snapshots under `examples/clog/tests/snapshots/`

**Interfaces:**
- Consumes: public API. This freezes the §5.8 rendering contract — **changing a golden after this task requires a spec edit** (§11.4).

The fixture (P1 scope — focus shifts and `correct()` checkpoints arrive in P2/P3): scopes `delivery-health` (risk 2.5, question 1.5, commitment 1.5, boost project:halcyon 1.5) and `cash-and-collections` (risk 2.0, fyi 0.5, opportunity 1.5) declared in Config; rules: risk ← BodyContains("overdue")|BodyContains("slipping"), question ← BodyRegex(`\?$`), fact ← ObserverIs("bank-feed"), opportunity ← BodyContains("inbound"). Ten claims across three clients (Manual clock; timestamps in whole days around a fixed epoch):

1. `halcyon:deliverable:slip` — "Halcyon deliverable is slipping by a week" (risk, project:halcyon)
2. `halcyon:inv-1042:v1` subject `halcyon:inv-1042:status` — "Invoice 1042 is 30 days overdue" (risk, gmail, B/2)
3. `halcyon:inv-1042:v2` same subject — "Invoice 1042 still unpaid per bookkeeper" (twist, C/3, later occurred_at)
4. `halcyon:inv-1042:paid` same subject — "Payment received for invoice 1042" (bank-feed, A/1, occurred_at EARLIER than 3 but wins on nothing — checkpoint B shows belief goes to 3, the spec §5.3 known limitation, then retracting 3 flips belief to this claim)
5. `meridian:kickoff:moved` — "Meridian kickoff moved to Thursday" (fyi)
6. `meridian:pto:sam` — "Sam is on PTO next week" (fyi, person:sam)
7. `meridian:question:sow` subject `meridian:sow` — "Did Meridian sign the SOW?" (question)
8. `vega:lead:inbound` — "Inbound lead from Vega Labs" (opportunity)
9. `vega:upsell:maybe` — "Vega mentioned maybe expanding scope" (no rule matches → Unclassified)
10. `meridian:question:sow:answered` — retraction of 7 at checkpoint C (self-resolved)

Checkpoints, each snapshotting both scopes' situation texts plus `select(View::Unclassified)` keys:
- **A**: after claims 1–2 and 5–9.
- **B**: after 3 then 4 (supersession chain; assert `believed` flags via `select(View::EntityState)` too).
- **C**: after retracting 7 and retracting 3 (healing + belief flip to the bank-feed claim).
- **D**: after `merge_entities(person:samuel → person:sam)` (registry proof).

- [ ] **Step 1: Write the fixture test** — build helpers `fn fixture_config(dir) -> Config`, `fn claim(...) -> Claim` with explicit fields per the list; at each checkpoint:

```rust
insta::assert_snapshot!("g1_a_delivery", c.situation(Some("delivery-health"), None).unwrap().text);
insta::assert_snapshot!("g1_a_cash", c.situation(Some("cash-and-collections"), None).unwrap().text);
```

- [ ] **Step 2: Run** — `cargo test -p clog --test g1_agency` → snapshots missing (expected failure).
- [ ] **Step 3: Review and accept** — `cargo insta review` (or `INSTA_UPDATE=accept cargo test -p clog --test g1_agency` then **manually read every snapshot** against §5.8's formats: header line, urgent numbering, `[B/2]` trust markers, `(none)` empties, changes slot showing checkpoint deltas). Do not accept a snapshot you have not read line by line.
- [ ] **Step 4: Run** — green, snapshots committed.
- [ ] **Step 5: Commit** — `git add -A examples/clog/tests && git commit -m "test(clog): G1 agency golden simulation (rendering contract frozen)"`

---

### Task 18: Recovery tests R1 (crash points), R3 (rebuild)

**Files:**
- Create: `examples/clog/tests/recovery.rs`
- Modify: `examples/clog/src/actor.rs` (crash hook), `examples/clog/src/wal.rs` if needed

**Interfaces:**
- Produces: crash hook in the writer, compiled only with `--features test-crash`:

```rust
#[cfg(feature = "test-crash")]
fn maybe_crash_after_wal(batch_no: u64) {
    if let Ok(n) = std::env::var("CLOG_CRASH_AFTER_WAL") {
        if n.parse::<u64>() == Ok(batch_no) { std::process::abort(); }
    }
}
```

called immediately after `wal.append` returns (post-fsync), before `engine.apply`.

- [ ] **Step 1: Write the failing tests**

```rust
// tests/recovery.rs
use clog::*;

fn script(c: &Clog) -> u64 {
    c.advance(1_000_000).unwrap();
    let mk = |k: &str, b: &str| Claim { claim_key: k.into(), subject_key: None,
        source_ref: "t:1".into(), observer: ObserverId::from("t"), schema_v: 1,
        occurred_at: 500_000, observed_at: 500_000, reliability: Reliability::B,
        credibility: Credibility::Two, entities: vec![], body: b.into() };
    let mut rev = 0;
    rev = c.observe(vec![mk("a", "first")], ObserveOpts::default()).map(|a| a.rev).unwrap_or(rev);
    rev = c.observe(vec![mk("b", "second")], ObserveOpts::default()).map(|a| a.rev).unwrap_or(rev);
    rev = c.retract("a").map(|a| a.rev).unwrap_or(rev);
    rev = c.observe(vec![mk("c", "third")], ObserveOpts::default()).map(|a| a.rev).unwrap_or(rev);
    rev
}

fn manual_cfg(dir: &std::path::Path) -> Config {
    let mut c = Config::default_for(dir);
    c.tick = TickConfig { mode: ClockMode::Manual, interval_ms: 60_000 };
    c
}

// R1: child aborts after the Nth WAL append; parent reopens and compares
// against a fresh instance fed the same first N batches.
#[test]
#[cfg_attr(not(feature = "test-crash"), ignore = "needs --features test-crash")]
fn r1_crash_points() {
    if std::env::var("CLOG_R1_CHILD").is_ok() {
        let dir = std::env::var("CLOG_R1_DIR").unwrap();
        let c = Clog::open(manual_cfg(std::path::Path::new(&dir))).unwrap();
        script(&c); // aborts partway via CLOG_CRASH_AFTER_WAL
        unreachable!("child should have crashed");
    }
    for crash_after in 1..=4u64 {
        let dir = tempfile::tempdir().unwrap();
        let exe = std::env::current_exe().unwrap();
        let status = std::process::Command::new(&exe)
            .args(["r1_crash_points", "--exact", "--nocapture"]) // not --ignored: with test-crash on, the test is not ignored
            .env("CLOG_R1_CHILD", "1")
            .env("CLOG_R1_DIR", dir.path())
            .env("CLOG_CRASH_AFTER_WAL", crash_after.to_string())
            .status().unwrap();
        assert!(!status.success(), "child must abort");

        // reopened world == fresh world fed the same durable prefix
        let reopened = Clog::open(manual_cfg(dir.path())).unwrap();
        let fresh_dir = tempfile::tempdir().unwrap();
        let fresh = Clog::open(manual_cfg(fresh_dir.path())).unwrap();
        replay_prefix(&fresh, crash_after);
        let (s1, s2) = (reopened.situation(None, None).unwrap(), fresh.situation(None, None).unwrap());
        assert_eq!(s1.text, s2.text, "crash point {crash_after}");
        assert_eq!(s1.rev, s2.rev);
    }
}

fn replay_prefix(c: &Clog, n: u64) {
    c.advance(1_000_000).unwrap();
    let mk = |k: &str, b: &str| Claim { claim_key: k.into(), subject_key: None,
        source_ref: "t:1".into(), observer: ObserverId::from("t"), schema_v: 1,
        occurred_at: 500_000, observed_at: 500_000, reliability: Reliability::B,
        credibility: Credibility::Two, entities: vec![], body: b.into() };
    let steps: Vec<Box<dyn Fn(&Clog)>> = vec![
        Box::new(move |c| { c.observe(vec![mk("a", "first")], ObserveOpts::default()).unwrap(); }),
        Box::new(move |c| { c.observe(vec![mk("b", "second")], ObserveOpts::default()).unwrap(); }),
        Box::new(|c| { c.retract("a").unwrap(); }),
        Box::new(move |c| { c.observe(vec![mk("c", "third")], ObserveOpts::default()).unwrap(); }),
    ];
    for s in steps.iter().take(n as usize) { s(c); }
}

// R3: rebuild_on_open == normal open
#[test]
fn r3_rebuild_equals_open() {
    let dir = tempfile::tempdir().unwrap();
    { let c = Clog::open(manual_cfg(dir.path())).unwrap(); script(&c); }
    let normal = Clog::open(manual_cfg(dir.path())).unwrap();
    let s1 = normal.situation(None, None).unwrap();
    drop(normal);
    let mut cfg2 = manual_cfg(dir.path());
    cfg2.rebuild_on_open = true;
    let rebuilt = Clog::open(cfg2).unwrap();
    let s2 = rebuilt.situation(None, None).unwrap();
    assert_eq!(s1.text, s2.text);
    assert_eq!(s1.rev, s2.rev);
}
```

- [ ] **Step 2: Run** — `cargo test -p clog --test recovery` (R3) and `cargo test -p clog --test recovery --features test-crash -- --include-ignored` (R1). Expected: fail until the hook exists / bugs fixed.
- [ ] **Step 3: Implement the hook** and fix anything R1 exposes (typical bug: replying to the caller before fsync, or publishing before WAL append).
- [ ] **Step 4: Run both commands** — green. Manual-clock caveat: the child's `advance` calls set the same timestamps, so texts match exactly.
- [ ] **Step 5: Commit** — `git commit -m "test(clog): crash-point recovery R1 and rebuild R3 (INV-11)"`

---

### Task 19: Docs, clippy, exit checklist

**Files:**
- Modify: `examples/clog/README.md`, rustdoc across `src/`

- [ ] **Step 1: README** — rewrite with: what clog is (3 sentences), the P1 API table (7 functions + `advance`), a 20-line quickstart (open → observe → situation → retract), pointers to spec/build-design, status ("P1 complete; P2 next: scopes/set_focus/ticks/wakes"), and the documented caveats from the ledger (no `%{` escape; duplicate batches don't commit; watches are P2).
- [ ] **Step 2: Rustdoc pass** — `cargo doc -p clog --no-deps` must be warning-free; every public item has at least one sentence; `Clog::open` gets a compiling doctest mirroring the README quickstart (use a tempdir).
- [ ] **Step 3: Lints** — `cargo clippy -p clog --all-targets -- -D warnings` clean; `cargo fmt -p clog --check` clean.
- [ ] **Step 4: Full exit run** —

```bash
cargo test -p clog
cargo test -p clog --features test-crash -- --include-ignored
cargo doc -p clog --no-deps
```

All green = M0+M1 exit criteria met (§11.1 subset, P1–P4/P6/P7, G1, R1–R3).

- [ ] **Step 5: Commit** — `git commit -m "docs(clog): P1 README and rustdoc; M0+M1 exit checklist green"`

---

## Self-review notes (already applied)

- **Spec coverage:** M0 units U-SCORE-1 (T4), U-BELIEF-1/2 (T5), U-ALIAS-1/2 (T6) /3 (T12), U-TMPL-1 (T8) /2/3 (T9), U-VAL-1 (T3), U-KIND-1 (T7). M1: views (T11/12), render+budget (T9), batching/rev (T14), WAL (T13), P1–P4/P6/P7 (T16), G1 (T17), R1–R3 (T13/T18). Deferred per build design: U-KIND-2, P5, P8, D*, B*, Z*, loom, snapshot files, set_focus/ticks/wakes/recall/correct.
- **Known simplification:** naive `urgent` recomputes all scopes per batch — acceptable for the oracle; do not "optimize" it in P1.
- **Type consistency:** `Rev = u64` alias (not a newtype — spec structs use bare u64); `ObserverId` newtype; `WorldViews.by_observer` keyed by `String` (the observer's inner string) to keep OrdMap keys simple.
