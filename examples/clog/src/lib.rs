//! Clog: an orientation engine for agentic systems.
//!
//! Hosts write structured claims; clog maintains materialized views over them
//! incrementally and renders a budgeted situation document per scope. See
//! `docs/clog-spec-v1.md` in the repository root for the full specification.
#![deny(missing_docs)]
#![cfg_attr(not(test), deny(clippy::unwrap_used, clippy::expect_used))]

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
