//! Public API surface for clog.
//!
//! Every type here derives `Clone, Debug, Serialize, Deserialize` (INV-12)
//! and, where meaningful, `PartialEq`. This module is the serde-only
//! contract that every later task builds on: storage, ranking, and
//! rendering all operate on these shapes.

use std::collections::BTreeMap;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

/// Identifies the host/integration that observed a claim (e.g. `"gmail-v3"`).
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct ObserverId(pub String);

impl From<&str> for ObserverId {
    fn from(s: &str) -> Self {
        ObserverId(s.to_string())
    }
}

impl From<String> for ObserverId {
    fn from(s: String) -> Self {
        ObserverId(s)
    }
}

/// A monotonic revision number for a scope's situation document.
pub type Rev = u64;

/// Admiralty reliability rating of a source, A (best) through F (worst).
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub enum Reliability {
    /// Completely reliable.
    A,
    /// Usually reliable.
    B,
    /// Fairly reliable.
    C,
    /// Not usually reliable.
    D,
    /// Unreliable.
    E,
    /// Reliability cannot be judged.
    F,
}

impl Reliability {
    /// 0 = best (A). Belief resolution and scoring use this rank.
    pub fn rank(self) -> u8 {
        self as u8
    }
    /// The Admiralty letter, for rendering.
    pub fn letter(self) -> char {
        (b'A' + self as u8) as char
    }
}

/// Admiralty credibility rating of a claim's content, One (best) through Six (worst).
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub enum Credibility {
    /// Confirmed by other sources.
    One,
    /// Probably true.
    Two,
    /// Possibly true.
    Three,
    /// Doubtful.
    Four,
    /// Improbable.
    Five,
    /// Credibility cannot be judged.
    Six,
}

impl Credibility {
    /// 0 = best (One).
    pub fn rank(self) -> u8 {
        self as u8
    }
    /// The Admiralty digit 1..=6, for rendering.
    pub fn digit(self) -> u8 {
        self as u8 + 1
    }
}

/// A reference to an entity (person, project, etc.) mentioned by a claim.
///
/// Identity is `(etype, id)` only: `name` is a display hint and is excluded
/// from equality, ordering, and hashing, so two refs to the same entity with
/// different display names still compare equal.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct EntityRef {
    /// The entity type, e.g. `"project"` or `"person"`.
    pub etype: String,
    /// The entity's stable identifier within its type.
    pub id: String,
    /// An optional display name, excluded from identity comparisons.
    pub name: Option<String>,
}

impl EntityRef {
    /// The identity key `(etype, id)`, ignoring `name`.
    pub fn key(&self) -> (String, String) {
        (self.etype.clone(), self.id.clone())
    }
}

impl PartialEq for EntityRef {
    fn eq(&self, other: &Self) -> bool {
        self.etype == other.etype && self.id == other.id
    }
}
impl Eq for EntityRef {}
impl PartialOrd for EntityRef {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}
impl Ord for EntityRef {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        (&self.etype, &self.id).cmp(&(&other.etype, &other.id))
    }
}
impl std::hash::Hash for EntityRef {
    fn hash<H: std::hash::Hasher>(&self, h: &mut H) {
        self.etype.hash(h);
        self.id.hash(h);
    }
}

/// A single structured observation submitted by a host.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Claim {
    /// The claim's unique key, chosen by the host.
    pub claim_key: String,
    /// An optional key grouping this claim with others about the same subject.
    pub subject_key: Option<String>,
    /// A reference back to the originating source (e.g. `"gmail:msg/123"`).
    pub source_ref: String,
    /// The observer that submitted this claim.
    pub observer: ObserverId,
    /// The schema version of `body`, for forward compatibility.
    pub schema_v: u16,
    /// When the observed event actually occurred, in epoch millis.
    pub occurred_at: u64,
    /// When clog recorded the observation, in epoch millis.
    pub observed_at: u64,
    /// The source's reliability rating.
    pub reliability: Reliability,
    /// The claim's credibility rating.
    pub credibility: Credibility,
    /// Entities this claim mentions.
    pub entities: Vec<EntityRef>,
    /// The claim's free-text content.
    pub body: String,
}

/// Weighting and boosting preferences used to rank claims for a scope.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Focus {
    /// Per-kind weight multipliers.
    pub weights: BTreeMap<String, f32>,
    /// Multiplicative score boosts for specific entities. Every boost whose
    /// entity a claim mentions multiplies into that claim's score, so two
    /// matching boosts stack as their product (spec §5.4).
    pub boosts: Vec<(EntityRef, f32)>,
    /// The half-life, in days, used for recency decay.
    pub half_life_days: f32,
    /// An optional cap on the number of ranked rows returned.
    pub top_k: Option<usize>,
}

impl Default for Focus {
    fn default() -> Self {
        Focus::uniform()
    }
}

impl Focus {
    /// A `Focus` with no per-kind weighting and no boosts.
    pub fn uniform() -> Focus {
        Focus {
            weights: BTreeMap::new(),
            boosts: Vec::new(),
            half_life_days: 7.0,
            top_k: None,
        }
    }

    /// Sets the weight multiplier for a kind.
    pub fn weight(mut self, kind: &str, w: f32) -> Focus {
        self.weights.insert(kind.to_string(), w);
        self
    }

    /// Adds a multiplicative score boost for an entity (spec §5.4).
    pub fn boost(mut self, e: EntityRef, f: f32) -> Focus {
        self.boosts.push((e, f));
        self
    }

    /// Sets the recency decay half-life, in days.
    pub fn half_life_days(mut self, d: f32) -> Focus {
        self.half_life_days = d;
        self
    }

    /// Sets the maximum number of ranked rows returned.
    pub fn top_k(mut self, k: usize) -> Focus {
        self.top_k = Some(k);
        self
    }
}

/// Selects which materialized view to read.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum View {
    /// Every live claim, in `claim_key` order. Unranked and unfiltered by
    /// recency: this is the whole world, not a feed.
    Live,
    /// The current state of tracked entities.
    EntityState,
    /// Open loops: unresolved questions, risks, and commitments.
    OpenLoops,
    /// Urgent items within a given scope.
    Urgent {
        /// The scope to restrict urgency to.
        scope: String,
    },
    /// Claims that haven't yet been kind-classified.
    Unclassified,
}

/// Filtering criteria applied when reading a view.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Filter {
    /// Restrict to these kinds, if set.
    pub kinds: Option<Vec<String>>,
    /// Restrict to claims mentioning any of these entities, if set.
    pub entities: Option<Vec<EntityRef>>,
    /// Restrict to claims from this observer, if set.
    pub observer: Option<ObserverId>,
    /// Restrict to claims whose subject key starts with this prefix, if set.
    pub subject_prefix: Option<String>,
    /// Restrict to claims that occurred after this epoch-millis timestamp, if set.
    pub occurred_after: Option<u64>,
    /// Restrict to rows scoring at least this value, if set.
    pub min_score: Option<f32>,
    /// Cap the number of rows returned, if set.
    pub limit: Option<usize>,
}

/// Where a kind classification came from.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum JudgeSource {
    /// A deterministic rule matched.
    Rule,
    /// A k-nearest-neighbors classifier matched.
    Knn,
    /// An external judge (e.g. an LLM call) supplied the label.
    External,
}

/// A kind classification with a confidence score.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct KindLabel {
    /// The classified kind, e.g. `"risk"`.
    pub kind: String,
    /// Confidence in `[0, 1]`.
    pub confidence: f32,
    /// The judge that produced this label.
    pub source: JudgeSource,
}

/// A single ranked row in a view: a claim plus its derived annotations.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Row {
    /// The underlying claim.
    pub claim: Claim,
    /// When clog recorded this claim, in epoch millis.
    pub recorded_at: u64,
    /// The claim's kind classification, if any.
    pub kind: Option<KindLabel>,
    /// The claim's ranking score, if applicable to the view.
    pub score: Option<f32>,
    /// Whether the claim is currently believed, if applicable.
    pub believed: Option<bool>,
}

/// A rendered situation document for a scope at a point in time.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Situation {
    /// The scope this situation document covers.
    pub scope: String,
    /// The rendered, budgeted text.
    pub text: String,
    /// The revision number at which this text was rendered.
    pub rev: Rev,
    /// The epoch-millis timestamp this situation reflects.
    pub as_of: u64,
}

/// Acknowledgement returned after an observation is recorded.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Ack {
    /// The new revision number after recording.
    pub rev: Rev,
    /// The freshly rendered situation, if requested.
    pub situation: Option<Situation>,
}

/// Options controlling how an observation is recorded.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct ObserveOpts {
    /// If set, the scope whose situation should be rendered and returned.
    pub return_situation: Option<String>,
}

/// A manual kind judgment supplied by a caller.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Judgment {
    /// The kind being asserted.
    pub kind: String,
}

/// How clog's internal clock advances.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum ClockMode {
    /// Advances with the system clock.
    System,
    /// Advances only when explicitly ticked.
    Manual,
}

/// When the write-ahead log is fsync'd.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum FsyncPolicy {
    /// Fsync after every committed write.
    OnCommit,
    /// Never explicitly fsync; rely on OS buffering.
    Never,
}

/// Configuration for clog's internal clock tick.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct TickConfig {
    /// The clock mode.
    pub mode: ClockMode,
    /// The tick interval in milliseconds.
    pub interval_ms: u64,
}

impl Default for TickConfig {
    fn default() -> Self {
        TickConfig {
            mode: ClockMode::System,
            interval_ms: 60_000,
        }
    }
}

/// A single condition used by a `Rule` to match a claim.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum Matcher {
    /// Matches if the claim body contains this substring (spec §5.6).
    ///
    /// The `bool` is `case_insensitive`: `true` compares both sides
    /// lowercased, `false` compares them verbatim.
    BodyContains(String, bool),
    /// Matches if the claim body matches this regular expression.
    BodyRegex(String),
    /// Matches if the claim's observer equals this string.
    ObserverIs(String),
    /// Matches if any of the claim's entities has this type.
    EntityType(String),
}

/// A classification rule: matches if any of its matchers match.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Rule {
    /// The matchers; the rule matches if any one of them matches.
    pub any_of: Vec<Matcher>,
}

/// The definition of a single claim kind.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct KindDef {
    /// The kind's name, e.g. `"risk"`.
    pub name: String,
    /// Rules used to classify claims into this kind.
    pub rules: Vec<Rule>,
    /// Seed example texts used to bootstrap kNN classification.
    pub seed_exemplars: Vec<String>,
}

/// The set of kinds clog knows how to classify claims into.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct KindTaxonomy {
    /// The defined kinds, in taxonomy order.
    pub kinds: Vec<KindDef>,
}

impl KindTaxonomy {
    /// The default taxonomy of 8 spec kinds, with no rules or exemplars.
    pub fn default_taxonomy() -> KindTaxonomy {
        let names = [
            "fact",
            "decision",
            "risk",
            "question",
            "commitment",
            "agreement",
            "opportunity",
            "fyi",
        ];
        KindTaxonomy {
            kinds: names
                .iter()
                .map(|n| KindDef {
                    name: n.to_string(),
                    rules: Vec::new(),
                    seed_exemplars: Vec::new(),
                })
                .collect(),
        }
    }

    /// Whether `kind` is a defined kind in this taxonomy.
    pub fn contains(&self, kind: &str) -> bool {
        self.kinds.iter().any(|k| k.name == kind)
    }
}

/// Top-level configuration for a clog instance.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Config {
    /// The filesystem path clog stores its data under.
    pub path: PathBuf,
    /// Per-scope focus overrides.
    pub scopes: BTreeMap<String, Focus>,
    /// The claim kind taxonomy in use.
    pub kinds: KindTaxonomy,
    /// Kinds treated as "open loops" (unresolved until closed).
    pub loop_kinds: Vec<String>,
    /// The default maximum number of ranked rows per situation.
    pub top_k: usize,
    /// The character budget for a rendered situation document.
    pub budget_chars: usize,
    /// The internal clock tick configuration.
    pub tick: TickConfig,
    /// The number of decay buckets per half-life.
    pub decay_buckets_per_half_life: u32,
    /// The minimum credibility required for a claim to be believed.
    pub belief_min_credibility: Credibility,
    /// The write-ahead log fsync policy.
    pub wal_fsync: FsyncPolicy,
    /// The bounded write queue depth.
    pub write_queue: usize,
    /// Accepted, and in P1 identical to a normal open: there is no persisted
    /// engine state to drop, so every open already replays the whole WAL.
    /// Becomes meaningful once engine snapshots exist, when it will mean
    /// "ignore the snapshot and rebuild from the log".
    pub rebuild_on_open: bool,
}

impl Config {
    /// Builds a `Config` at `path` with spec-mandated defaults.
    pub fn default_for(path: impl Into<PathBuf>) -> Config {
        Config {
            path: path.into(),
            scopes: BTreeMap::new(),
            kinds: KindTaxonomy::default_taxonomy(),
            loop_kinds: vec![
                "question".to_string(),
                "risk".to_string(),
                "commitment".to_string(),
            ],
            top_k: 12,
            budget_chars: 6000,
            tick: TickConfig::default(),
            decay_buckets_per_half_life: 4,
            belief_min_credibility: Credibility::Six,
            wal_fsync: FsyncPolicy::OnCommit,
            write_queue: 1024,
            rebuild_on_open: false,
        }
    }
}

/// Errors returned by clog's public API.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum ClogError {
    /// A submitted claim at `index` in a batch was invalid.
    #[error("invalid claim at index {index}: {reason}")]
    InvalidClaim {
        /// The index of the offending claim within its batch.
        index: usize,
        /// A human-readable explanation.
        reason: String,
    },
    /// The operation targeted a namespace reserved for internal use.
    #[error("reserved namespace")]
    ReservedNamespace,
    /// The referenced claim does not exist.
    #[error("unknown claim")]
    UnknownClaim,
    /// The referenced scope does not exist.
    #[error("unknown scope")]
    UnknownScope,
    /// The referenced kind does not exist in the taxonomy.
    #[error("unknown kind")]
    UnknownKind,
    /// An alias chain formed a cycle.
    #[error("alias cycle")]
    AliasCycle,
    /// Rendering a situation document template failed.
    #[error("template error: {0}")]
    TemplateError(String),
    /// An underlying storage I/O error occurred.
    #[error("storage error: {0}")]
    Storage(#[from] std::io::Error),
    /// On-disk state was found to be corrupt.
    #[error("corrupt storage: {detail}")]
    Corrupt {
        /// A human-readable explanation.
        detail: String,
    },
    /// The instance is shutting down and cannot accept the operation.
    #[error("shutting down")]
    ShuttingDown,
    /// The supplied filter was invalid.
    #[error("invalid filter: {reason}")]
    InvalidFilter {
        /// A human-readable explanation.
        reason: String,
    },
    /// Semantic (embedding-based) features are disabled in this build/config.
    #[error("semantic features disabled")]
    SemanticDisabled,
    /// The operation requires manual clock mode.
    #[error("manual clock required")]
    ManualClockRequired,
}

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
            entities: vec![EntityRef {
                etype: "project".into(),
                id: "halcyon".into(),
                name: Some("Halcyon".into()),
            }],
            body: "Invoice 1042 is 30 days overdue".into(),
        };
        let bytes = postcard::to_allocvec(&c).unwrap();
        assert_eq!(postcard::from_bytes::<Claim>(&bytes).unwrap(), c);

        let f = Focus::uniform()
            .weight("risk", 2.5)
            .half_life_days(3.0)
            .top_k(8);
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
        let a = EntityRef {
            etype: "person".into(),
            id: "sam".into(),
            name: Some("Sam".into()),
        };
        let b = EntityRef {
            etype: "person".into(),
            id: "sam".into(),
            name: None,
        };
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
