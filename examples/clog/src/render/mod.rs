//! Deterministic rendering: template parsing, RFC3339 timestamps, and the
//! slot renderer + budgeter (spec §5.8, tests U-TMPL-2/3).

/// Template grammar parser (spec §5.8, test U-TMPL-1).
pub(crate) mod template;

/// RFC3339 (UTC, seconds precision) timestamp formatting used by the
/// `header` slot (spec §5.8).
pub(crate) mod time;

use template::{Segment, SlotName, Template};
use time::rfc3339_utc;

/// Collapses whitespace and caps length for item headlines (spec §5.8).
///
/// Splits `body` on any whitespace run (`split_whitespace`) and rejoins
/// with single spaces, then truncates to the first 120 **chars** (not
/// bytes — a multi-byte char is never split).
pub(crate) fn headline(body: &str) -> String {
    let collapsed = body.split_whitespace().collect::<Vec<_>>().join(" ");
    collapsed.chars().take(120).collect()
}

/// A ranked urgent item (spec §5.8, `%{urgent}` slot).
#[derive(Clone, Debug)]
pub(crate) struct UrgentItem {
    /// Urgency score, rendered to one decimal place.
    pub score: f32,
    /// Pre-normalized headline text (see [`headline`]).
    pub headline: String,
    /// Admiralty reliability letter (`Reliability::letter`), A-E.
    pub reliability: char,
    /// Admiralty credibility digit (`Credibility::digit`), 1-6.
    pub credibility: u8,
    /// The claim key backing this item.
    pub claim_key: String,
}

/// An open-loop item (spec §5.8, `%{open_loops}` slot).
#[derive(Clone, Debug)]
pub(crate) struct LoopItem {
    /// The open-loop kind (e.g. `"question"`), rendered uppercased.
    pub kind: String,
    /// Pre-normalized headline text (see [`headline`]).
    pub headline: String,
    /// The claim key backing this item.
    pub claim_key: String,
}

/// An entity summary item (spec §5.8, `%{entities}` slot).
#[derive(Clone, Debug)]
pub(crate) struct EntityItem {
    /// The entity's display name.
    pub display: String,
    /// Summary lines for this entity, newest-first; joined with `"; "`.
    pub summaries: Vec<String>,
}

/// A change since the last brief (spec §5.8, `%{changes}` slot).
#[derive(Clone, Debug)]
pub(crate) enum ChangeItem {
    /// A newly surfaced headline, rendered `+ {headline}`.
    Added(String),
    /// A headline no longer present, rendered `- {headline}`.
    Removed(String),
}

/// All materialized data a single `render` call needs (spec §5.8).
///
/// This is the engine-to-renderer contract: Tasks 11-15 build the
/// `Vec<*Item>` fields from the live views.
#[derive(Clone, Debug)]
pub(crate) struct SlotInputs {
    /// The scope this document is rendered for.
    pub scope: String,
    /// The scope's revision counter at render time.
    pub rev: u64,
    /// The as-of timestamp (ms since Unix epoch, UTC) rendered by `header`.
    pub as_of_ms: u64,
    /// Ranked urgent items, in the order they should be numbered.
    pub urgent: Vec<UrgentItem>,
    /// Open-loop items.
    pub open_loops: Vec<LoopItem>,
    /// Per-entity summaries.
    pub entities: Vec<EntityItem>,
    /// Changes since the last brief.
    pub changes: Vec<ChangeItem>,
}

/// The rendered lines of one non-header slot, plus a count of items hidden
/// so far (by the slot's own `limit=`, by budget truncation, or both).
struct SlotState {
    lines: Vec<String>,
    hidden: usize,
}

impl SlotState {
    /// Builds the state for a slot from its fully-formatted item lines and
    /// the template's per-slot `limit`, capping `lines` and recording how
    /// many items that cap hid (spec §5.8: "per-slot limit caps items
    /// before budgeting").
    fn new(mut lines: Vec<String>, limit: Option<usize>) -> Self {
        let mut hidden = 0;
        if let Some(n) = limit
            && lines.len() > n
        {
            hidden = lines.len() - n;
            lines.truncate(n);
        }
        Self { lines, hidden }
    }

    /// Drops the last remaining item line (budget truncation never cuts
    /// mid-item), incrementing the hidden count. Returns `false` if there
    /// was nothing left to drop.
    fn drop_one(&mut self) -> bool {
        if self.lines.pop().is_some() {
            self.hidden += 1;
            true
        } else {
            false
        }
    }

    /// Renders this slot's block: `(none)` if genuinely empty, otherwise
    /// its item lines joined by `\n`, with a trailing `… ({n} more)`
    /// marker line if any items are hidden (spec §5.8).
    fn render(&self) -> String {
        if self.lines.is_empty() && self.hidden == 0 {
            return "(none)".to_string();
        }
        let mut lines = self.lines.clone();
        if self.hidden > 0 {
            lines.push(format!("… ({} more)", self.hidden));
        }
        lines.join("\n")
    }
}

/// Finds the `limit=` configured for the first occurrence of `name` in the
/// template, if any (`None` if the slot isn't present, or is present
/// without a `limit`).
fn configured_limit(t: &Template, name: SlotName) -> Option<usize> {
    t.0.iter().find_map(|seg| match seg {
        Segment::Slot { name: n, limit } if *n == name => *limit,
        _ => None,
    })
}

/// Renders `t` against `inputs`, then budgets the result down to
/// `budget_chars` **chars** (spec §5.8, tests U-TMPL-2/3).
///
/// Whole items are dropped from the end of slots in reverse priority order
/// — `changes`, then `entities`, then `open_loops`, then `urgent` — until
/// the document fits, or nothing is left to drop. Truncation never cuts
/// mid-item; each truncated slot gets (or updates) a trailing
/// `… ({n} more)` marker with its total hidden count.
pub(crate) fn render(t: &Template, inputs: &SlotInputs, budget_chars: usize) -> String {
    let urgent_lines: Vec<String> = inputs
        .urgent
        .iter()
        .enumerate()
        .map(|(i, u)| {
            format!(
                "{}. ({:.1}) {} [{}/{}] ({})",
                i + 1,
                u.score,
                u.headline,
                u.reliability,
                u.credibility,
                u.claim_key
            )
        })
        .collect();
    let open_loop_lines: Vec<String> = inputs
        .open_loops
        .iter()
        .map(|l| {
            format!(
                "- {} {} ({})",
                l.kind.to_uppercase(),
                l.headline,
                l.claim_key
            )
        })
        .collect();
    let entity_lines: Vec<String> = inputs
        .entities
        .iter()
        .map(|e| format!("{}: {}", e.display, e.summaries.join("; ")))
        .collect();
    let change_lines: Vec<String> = inputs
        .changes
        .iter()
        .map(|c| match c {
            ChangeItem::Added(h) => format!("+ {h}"),
            ChangeItem::Removed(h) => format!("- {h}"),
        })
        .collect();

    let mut urgent_state = SlotState::new(urgent_lines, configured_limit(t, SlotName::Urgent));
    let mut open_loops_state =
        SlotState::new(open_loop_lines, configured_limit(t, SlotName::OpenLoops));
    let mut entities_state = SlotState::new(entity_lines, configured_limit(t, SlotName::Entities));
    let mut changes_state = SlotState::new(change_lines, configured_limit(t, SlotName::Changes));

    let header_line = format!(
        "{} · rev {} · {}",
        inputs.scope,
        inputs.rev,
        rfc3339_utc(inputs.as_of_ms)
    );

    let build = |urgent: &SlotState,
                 open_loops: &SlotState,
                 entities: &SlotState,
                 changes: &SlotState|
     -> String {
        let mut out = String::new();
        for seg in &t.0 {
            match seg {
                Segment::Text(s) => out.push_str(s),
                Segment::Slot { name, .. } => {
                    let block = match name {
                        SlotName::Header => header_line.clone(),
                        SlotName::Urgent => urgent.render(),
                        SlotName::OpenLoops => open_loops.render(),
                        SlotName::Entities => entities.render(),
                        SlotName::Changes => changes.render(),
                    };
                    out.push_str(&block);
                }
            }
        }
        out
    };

    let mut current = build(
        &urgent_state,
        &open_loops_state,
        &entities_state,
        &changes_state,
    );

    while current.chars().count() > budget_chars {
        let dropped = changes_state.drop_one()
            || entities_state.drop_one()
            || open_loops_state.drop_one()
            || urgent_state.drop_one();
        if !dropped {
            break;
        }
        current = build(
            &urgent_state,
            &open_loops_state,
            &entities_state,
            &changes_state,
        );
    }

    current
}

#[cfg(test)]
mod render_tests {
    use super::*;
    use crate::render::template::{DEFAULT_TEMPLATE, parse};

    fn inputs() -> SlotInputs {
        SlotInputs {
            scope: "default".into(),
            rev: 7,
            as_of_ms: 86_400_000,
            urgent: vec![
                UrgentItem {
                    score: 1.25,
                    headline: "Invoice 1042 overdue".into(),
                    reliability: 'B',
                    credibility: 2,
                    claim_key: "inv".into(),
                },
                UrgentItem {
                    score: 0.5,
                    headline: "Kickoff moved".into(),
                    reliability: 'A',
                    credibility: 1,
                    claim_key: "kick".into(),
                },
            ],
            open_loops: vec![LoopItem {
                kind: "question".into(),
                headline: "Did we sign?".into(),
                claim_key: "q1".into(),
            }],
            entities: vec![EntityItem {
                display: "Halcyon".into(),
                summaries: vec!["paid".into(), "kicked off".into()],
            }],
            changes: vec![
                ChangeItem::Added("Invoice 1042 overdue".into()),
                ChangeItem::Removed("old thing".into()),
            ],
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
        // NOTE: the brief's original budget here was `full.len() - 1`
        // (bytes). This renders with three middle dots (U+00B7, 2 bytes
        // each), so `full.len()` (bytes) exceeds `full.chars().count()`
        // by 3, and `render`'s budget check is char-based (spec §5.8:
        // "total chars ... not bytes"). A byte-derived budget of
        // `full.len() - 1` is therefore never tight enough to trigger any
        // truncation at all, which would make every assertion below
        // vacuous or false. Using `full.chars().count() - 1` restores the
        // test's intent — a budget just barely under the full render —
        // against a spec-correct char-counting budgeter. See
        // task-9-report.md for the full note.
        let tight = render(&t, &inputs(), full.chars().count() - 1);
        // changes go first, replaced by the marker
        assert!(tight.contains("… (") && tight.contains("more)"));
        assert!(!tight.contains("- old thing"));
        // urgent survives longest
        assert!(tight.contains("1. (1.2)"));
        // never over budget
        assert!(tight.chars().count() < full.len() || tight.contains("more)"));
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
