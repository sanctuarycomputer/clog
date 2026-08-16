//! Template grammar parser (spec §5.8, test U-TMPL-1).
//!
//! ```text
//! template := ( text | slot )*
//! slot     := "%{" name ( WS+ key "=" value )* "}"
//! name     := "header" | "urgent" | "open_loops" | "entities" | "changes"
//! key      := "limit"            (usize; per-slot cap)
//! ```
//!
//! There is no escape for a literal `%{` in v1: the two-byte sequence
//! `%{` always begins a slot. A host template that needs a literal `%{`
//! in its rendered text cannot express one; this is a deliberate v1
//! limitation (see spec §5.8), not an oversight.

use crate::types::ClogError;

/// A slot name in the template grammar (spec §5.8).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SlotName {
    /// `%{header}` — `{scope} · rev {rev} · {as_of RFC3339}`.
    Header,
    /// `%{urgent}` — ranked urgent items.
    Urgent,
    /// `%{open_loops}` — open-loop items.
    OpenLoops,
    /// `%{entities}` — entity summaries.
    Entities,
    /// `%{changes}` — changes since the last brief.
    Changes,
}

/// One parsed unit of a template: literal text, or a slot to be rendered.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Segment {
    /// Literal text, emitted verbatim.
    Text(String),
    /// A slot to be rendered, with its optional `limit` cap.
    Slot {
        /// Which materialized view this slot renders.
        name: SlotName,
        /// Per-slot item cap parsed from `limit=<usize>`, if present.
        limit: Option<usize>,
    },
}

/// A parsed template: an ordered sequence of segments.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Template(pub Vec<Segment>);

/// The default template (spec §5.8), used when `template == None`. Frozen
/// byte-for-byte: the §11.4 golden tests (U-TMPL-3) depend on this exact
/// string, including its trailing newline.
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

/// Parses `src` per the template grammar (spec §5.8).
///
/// Scans for the literal `%{`; everything before it is a `Text` segment.
/// Everything up to the next `}` is the slot body: its first
/// whitespace-separated token is the slot name, and each remaining token
/// must be `limit=<usize>`.
///
/// # Errors
///
/// Returns `ClogError::TemplateError` for any malformed or unknown slot:
/// an unterminated slot (`%{` with no matching `}`), an empty slot
/// (`%{}`), an unknown slot name, an unknown key, or a `limit` value that
/// fails to parse as a `usize` (including an empty value). Never panics
/// on any input — malformed byte sequences produce an `Err`, not a panic
/// (a fuzz target covers this in a later milestone).
pub(crate) fn parse(src: &str) -> Result<Template, ClogError> {
    let mut segments = Vec::new();
    let mut rest = src;
    loop {
        match rest.find("%{") {
            None => {
                if !rest.is_empty() {
                    segments.push(Segment::Text(rest.to_string()));
                }
                break;
            }
            Some(start) => {
                if start > 0 {
                    segments.push(Segment::Text(rest[..start].to_string()));
                }
                let after_open = &rest[start + 2..];
                let close = after_open
                    .find('}')
                    .ok_or_else(|| ClogError::TemplateError("unterminated slot: missing '}'".to_string()))?;
                let body = &after_open[..close];
                segments.push(parse_slot(body)?);
                rest = &after_open[close + 1..];
            }
        }
    }
    Ok(Template(segments))
}

/// Parses the body of a single slot (the text between `%{` and `}`).
fn parse_slot(body: &str) -> Result<Segment, ClogError> {
    let mut tokens = body.split_whitespace();
    let name_tok = tokens
        .next()
        .ok_or_else(|| ClogError::TemplateError("empty slot: '%{}' has no name".to_string()))?;
    let name = match name_tok {
        "header" => SlotName::Header,
        "urgent" => SlotName::Urgent,
        "open_loops" => SlotName::OpenLoops,
        "entities" => SlotName::Entities,
        "changes" => SlotName::Changes,
        other => return Err(ClogError::TemplateError(format!("unknown slot name {other:?}"))),
    };

    let mut limit = None;
    for tok in tokens {
        let (key, value) = tok
            .split_once('=')
            .ok_or_else(|| ClogError::TemplateError(format!("malformed slot argument {tok:?}: expected key=value")))?;
        if key != "limit" {
            return Err(ClogError::TemplateError(format!("unknown slot key {key:?}")));
        }
        let parsed = value
            .parse::<usize>()
            .map_err(|_| ClogError::TemplateError(format!("invalid limit value {value:?}: expected a usize")))?;
        limit = Some(parsed);
    }

    Ok(Segment::Slot { name, limit })
}

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
            "%{nope}",             // unknown slot name
            "%{urgent",            // unterminated
            "%{urgent limit=}",    // empty value
            "%{urgent limit=abc}", // non-numeric
            "%{urgent size=3}",    // unknown key
            "%{}",                 // empty slot
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
}
