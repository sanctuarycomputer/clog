//! Minimal host-integration loop: open, declare a scope, observe a few
//! claims, read the situation, retract one, and read again.
//!
//! Run with `cargo run -p clog --example quickstart`. This is also the
//! source of the README's quickstart listing — keep them in sync.

use clog::*;

fn main() -> Result<(), ClogError> {
    let dir = tempfile::tempdir().expect("tempdir");

    // A scope named "inbox" with default focus; `Config::default_for`
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
    println!(
        "--- situation (rev {}) ---\n{}",
        situation.rev, situation.text
    );

    // A correction: the host learned the invoice claim was wrong.
    clog.retract("gmail:msg/1")?;

    let healed = clog.situation(Some("inbox"), None)?;
    println!(
        "--- situation after retract (rev {}) ---\n{}",
        healed.rev, healed.text
    );

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
