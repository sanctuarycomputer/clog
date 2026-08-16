//! Rules-tier classifier: the free, deterministic first tier of the
//! cascade classifier (spec §5.6, test U-KIND-1).
//!
//! Config (a `KindTaxonomy`) is compiled once into a `RuleSet` with
//! pre-compiled regexes, then `classify` is called per-claim. Rules are
//! evaluated in config order: taxonomy declaration order, then each kind's
//! rules in their declared order. The first rule whose `any_of` matchers
//! contain any match wins; ties are impossible because the search stops at
//! the first hit.

use regex::Regex;

use crate::types::{Claim, ClogError, JudgeSource, KindLabel, KindTaxonomy, Matcher, Rule};

/// A `Matcher` with any embedded regex pre-compiled at `compile()` time.
#[derive(Debug)]
enum CompiledMatcher {
    /// Case-insensitive substring match (both sides lowercased via
    /// `to_lowercase`). This is a v1 simplification: it is not full Unicode
    /// case-folding, just `char::to_lowercase` applied to the whole string,
    /// which is correct-enough for the ASCII- and common-case text clog
    /// expects in claim bodies.
    BodyContains(String),
    /// Regex match against the claim body, pre-compiled.
    BodyRegex(Regex),
    /// Exact match against the observer's inner string.
    ObserverIs(String),
    /// Matches if any claim entity has this `etype`.
    EntityType(String),
}

/// A `Rule` with its matchers pre-compiled.
#[derive(Debug)]
struct CompiledRule {
    any_of: Vec<CompiledMatcher>,
}

impl CompiledRule {
    fn matches(&self, c: &Claim) -> bool {
        self.any_of.iter().any(|m| match m {
            CompiledMatcher::BodyContains(needle) => c.body.to_lowercase().contains(needle),
            CompiledMatcher::BodyRegex(re) => re.is_match(&c.body),
            CompiledMatcher::ObserverIs(s) => &c.observer.0 == s,
            CompiledMatcher::EntityType(etype) => c.entities.iter().any(|e| &e.etype == etype),
        })
    }
}

/// A compiled `KindTaxonomy`, ready for repeated `classify` calls.
///
/// Holds `(kind_name, rules)` pairs in config order, with every
/// `Matcher::BodyRegex` already compiled so `classify` never re-parses a
/// regex on the hot path.
pub(crate) struct RuleSet {
    kinds: Vec<(String, Vec<CompiledRule>)>,
}

/// Compiles a `KindTaxonomy` into a `RuleSet`, pre-compiling every regex.
///
/// # Errors
///
/// If any `Matcher::BodyRegex` fails to compile, returns
/// `ClogError::Corrupt { detail: "config: bad regex ..." }`. This is a
/// deliberate choice, not a perfect fit: the taxonomy is host-supplied
/// config rather than on-disk state, but `Corrupt` is the closest existing
/// variant for "state clog was handed is unusable" and config counts as
/// state clog must trust. Revisit in P3 if a dedicated `InvalidConfig`
/// variant earns its place.
pub(crate) fn compile(tax: &KindTaxonomy) -> Result<RuleSet, ClogError> {
    fn compile_matcher(m: &Matcher) -> Result<CompiledMatcher, ClogError> {
        Ok(match m {
            Matcher::BodyContains(s) => CompiledMatcher::BodyContains(s.to_lowercase()),
            Matcher::BodyRegex(pat) => {
                let re = Regex::new(pat).map_err(|e| ClogError::Corrupt {
                    detail: format!("config: bad regex {pat:?}: {e}"),
                })?;
                CompiledMatcher::BodyRegex(re)
            }
            Matcher::ObserverIs(s) => CompiledMatcher::ObserverIs(s.clone()),
            Matcher::EntityType(s) => CompiledMatcher::EntityType(s.clone()),
        })
    }

    fn compile_rule(r: &Rule) -> Result<CompiledRule, ClogError> {
        let any_of = r
            .any_of
            .iter()
            .map(compile_matcher)
            .collect::<Result<Vec<_>, _>>()?;
        Ok(CompiledRule { any_of })
    }

    let kinds = tax
        .kinds
        .iter()
        .map(|kd| -> Result<(String, Vec<CompiledRule>), ClogError> {
            let rules = kd
                .rules
                .iter()
                .map(compile_rule)
                .collect::<Result<Vec<_>, _>>()?;
            Ok((kd.name.clone(), rules))
        })
        .collect::<Result<Vec<_>, _>>()?;

    Ok(RuleSet { kinds })
}

/// Classifies `c` against `rs`, returning the first matching kind in config
/// order (taxonomy declaration order, then each kind's rules in their
/// declared order), or `None` if no rule matches.
///
/// A rule matches when any of its matchers match (§5.6 `any_of`). Matches
/// are always confidence `1.0` from `JudgeSource::Rule`.
pub(crate) fn classify(rs: &RuleSet, c: &Claim) -> Option<KindLabel> {
    for (kind, rules) in &rs.kinds {
        for rule in rules {
            if rule.matches(c) {
                return Some(KindLabel {
                    kind: kind.clone(),
                    confidence: 1.0,
                    source: JudgeSource::Rule,
                });
            }
        }
    }
    None
}

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
                "risk" => kd.rules.push(Rule {
                    any_of: vec![Matcher::BodyContains("overdue".into())],
                }),
                "question" => kd.rules.push(Rule {
                    any_of: vec![
                        Matcher::BodyRegex(r"\?$".into()),
                        Matcher::ObserverIs("faq-bot".into()),
                    ],
                }),
                "fact" => kd.rules.push(Rule {
                    any_of: vec![Matcher::EntityType("bankfeed".into())],
                }),
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
        c.entities = vec![EntityRef {
            etype: "bankfeed".into(),
            id: "x".into(),
            name: None,
        }];
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
        tax.kinds[0].rules.push(Rule {
            any_of: vec![Matcher::BodyRegex("(".into())],
        });
        assert!(matches!(compile(&tax), Err(ClogError::Corrupt { .. })));
    }
}
