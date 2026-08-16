//! Deterministic rendering: template parsing, RFC3339 timestamps, and (in a
//! later task) the slot renderer itself (spec §5.8).

/// Template grammar parser (spec §5.8, test U-TMPL-1).
pub(crate) mod template;

/// RFC3339 (UTC, seconds precision) timestamp formatting used by the
/// `header` slot (spec §5.8).
pub(crate) mod time;

// Re-exported for the renderer body landing in the next task; not yet
// consumed by production code.
#[allow(unused_imports)]
pub(crate) use template::{parse, Segment, SlotName, Template, DEFAULT_TEMPLATE};
#[allow(unused_imports)]
pub(crate) use time::rfc3339_utc;
