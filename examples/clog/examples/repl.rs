//! An interactive demo REPL for clog: feed it observations, watch the
//! situation document reorganize.
//!
//! ```console
//! cargo run -p clog --example repl            # opens ./clog-repl-data
//! cargo run -p clog --example repl -- --fresh # wipe the store first
//! ```
//!
//! The store boots with the G1 "client-services studio" world (see
//! `tests/g1_agency.rs`) frozen at its most interesting moment: three
//! claims are competing over invoice 1042's status, a client question is
//! still open, and `person:samuel` / `person:sam` are not yet merged. A
//! suggested stage script lives in the crate README.
//!
//! The store is a real clog WAL: type `crash` to abort the process
//! mid-session, relaunch, and the world (including your own observations)
//! comes back — that's INV-11 live.

use std::io::{BufRead, IsTerminal, Write};
use std::path::{Path, PathBuf};

use clog::*;

const DAY_MS: u64 = 86_400_000;
/// The demo's "now": two days after the last seeded batch.
const BOOT_NOW: u64 = 20_008 * DAY_MS;

fn day(n: u64) -> u64 {
    n * DAY_MS
}

// ---------------------------------------------------------------------------
// world configuration (mirrors tests/g1_agency.rs)
// ---------------------------------------------------------------------------

fn demo_config(dir: &std::path::Path) -> Config {
    let mut c = Config::default_for(dir);
    c.tick = TickConfig {
        mode: ClockMode::Manual,
        interval_ms: 60_000,
    };

    c.scopes.insert(
        "delivery-health".into(),
        Focus::uniform()
            .weight("risk", 2.5)
            .weight("question", 1.5)
            .weight("commitment", 1.5)
            .boost(entity("project", "halcyon"), 1.5),
    );
    c.scopes.insert(
        "cash-and-collections".into(),
        Focus::uniform()
            .weight("risk", 2.0)
            .weight("fyi", 0.5)
            .weight("opportunity", 1.5),
    );

    for kd in &mut c.kinds.kinds {
        match kd.name.as_str() {
            "risk" => kd.rules.push(Rule {
                any_of: vec![
                    Matcher::BodyContains("overdue".into(), true),
                    Matcher::BodyContains("slipping".into(), true),
                ],
            }),
            "question" => kd.rules.push(Rule {
                any_of: vec![Matcher::BodyRegex(r"\?$".into())],
            }),
            "fact" => kd.rules.push(Rule {
                any_of: vec![Matcher::ObserverIs("bank-feed".into())],
            }),
            "opportunity" => kd.rules.push(Rule {
                any_of: vec![Matcher::BodyContains("inbound".into(), true)],
            }),
            "fyi" => kd.rules.push(Rule {
                any_of: vec![
                    Matcher::BodyContains("moved".into(), true),
                    Matcher::BodyContains("PTO".into(), true),
                ],
            }),
            _ => {}
        }
    }
    c
}

fn entity(etype: &str, id: &str) -> EntityRef {
    EntityRef {
        etype: etype.into(),
        id: id.into(),
        name: None,
    }
}

fn named(etype: &str, id: &str, name: &str) -> EntityRef {
    EntityRef {
        etype: etype.into(),
        id: id.into(),
        name: Some(name.into()),
    }
}

#[allow(clippy::too_many_arguments)]
fn seed_claim(
    key: &str,
    subject: Option<&str>,
    body: &str,
    observer: &str,
    reliability: Reliability,
    credibility: Credibility,
    occurred_day: u64,
    entities: Vec<EntityRef>,
) -> Claim {
    let occ = day(occurred_day);
    Claim {
        claim_key: key.into(),
        subject_key: subject.map(Into::into),
        source_ref: format!("{observer}:{key}"),
        observer: ObserverId::from(observer),
        schema_v: 1,
        occurred_at: occ,
        observed_at: occ,
        reliability,
        credibility,
        entities,
        body: body.into(),
    }
}

/// The G1 world through checkpoint B plus claim 11: belief competition on
/// the invoice is live, the SOW question is open, and samuel/sam are still
/// two entities — so `retract` and `merge` have visible work to do.
fn seed_world(c: &Clog) -> Result<(), ClogError> {
    use Credibility::*;
    use Reliability::*;
    let halcyon = || named("project", "halcyon", "Halcyon");
    c.observe(
        vec![
            seed_claim(
                "halcyon:deliverable:slip",
                Some("halcyon:deliverable:status"),
                "Halcyon deliverable is slipping by a week",
                "twist",
                B,
                Three,
                20_000,
                vec![halcyon()],
            ),
            seed_claim(
                "halcyon:inv-1042:v1",
                Some("halcyon:inv-1042:status"),
                "Invoice 1042 is 30 days overdue",
                "gmail",
                B,
                Two,
                20_000,
                vec![halcyon()],
            ),
            seed_claim(
                "meridian:kickoff:moved",
                None,
                "Meridian kickoff moved to Thursday",
                "gmail",
                B,
                Two,
                20_000,
                vec![],
            ),
            seed_claim(
                "meridian:pto:sam",
                Some("meridian:pto:samuel:status"),
                "Sam is on PTO next week",
                "slack",
                C,
                Two,
                20_000,
                vec![named("person", "samuel", "Samuel")],
            ),
            seed_claim(
                "meridian:question:sow",
                Some("meridian:sow"),
                "Did Meridian sign the SOW?",
                "gmail",
                B,
                Two,
                20_001,
                vec![],
            ),
            seed_claim(
                "vega:lead:inbound",
                None,
                "Inbound lead from Vega Labs",
                "hubspot",
                B,
                Two,
                20_001,
                vec![],
            ),
            seed_claim(
                "vega:upsell:maybe",
                None,
                "Vega mentioned maybe expanding scope",
                "twist",
                C,
                Three,
                20_001,
                vec![],
            ),
            seed_claim(
                "halcyon:inv-1042:v2",
                Some("halcyon:inv-1042:status"),
                "Invoice 1042 still overdue per bookkeeper",
                "twist",
                C,
                Three,
                20_004,
                vec![halcyon()],
            ),
            seed_claim(
                "halcyon:inv-1042:paid",
                Some("halcyon:inv-1042:status"),
                "Payment received for invoice 1042",
                "bank-feed",
                A,
                One,
                20_002,
                vec![halcyon()],
            ),
            seed_claim(
                "meridian:sam:oneone",
                Some("meridian:sam:oneone:status"),
                "Sam moved his 1:1 to Friday",
                "slack",
                B,
                Two,
                20_005,
                vec![named("person", "sam", "Sam")],
            ),
        ],
        ObserveOpts::default(),
    )?;
    Ok(())
}

// ---------------------------------------------------------------------------
// terminal helpers
// ---------------------------------------------------------------------------

struct Style {
    on: bool,
}

impl Style {
    fn heading(&self, s: &str) -> String {
        if self.on {
            format!("\x1b[1;36m{s}\x1b[0m")
        } else {
            s.to_string()
        }
    }
    fn ok(&self, s: &str) -> String {
        if self.on {
            format!("\x1b[32m{s}\x1b[0m")
        } else {
            s.to_string()
        }
    }
    fn err(&self, s: &str) -> String {
        if self.on {
            format!("\x1b[31m{s}\x1b[0m")
        } else {
            s.to_string()
        }
    }
    fn dim(&self, s: &str) -> String {
        if self.on {
            format!("\x1b[2m{s}\x1b[0m")
        } else {
            s.to_string()
        }
    }
}

fn print_situation(c: &Clog, scope: &str, style: &Style) {
    match c.situation(Some(scope), None) {
        Ok(s) => {
            println!();
            for line in s.text.lines() {
                if line.starts_with('#') {
                    println!("{}", style.heading(line));
                } else {
                    println!("{line}");
                }
            }
            println!();
        }
        Err(e) => println!("{}", style.err(&format!("situation error: {e}"))),
    }
}

fn print_rows(rows: &[Row], with_believed: bool) {
    for r in rows {
        let kind = r
            .kind
            .as_ref()
            .map(|k| format!("[{}]", k.kind))
            .unwrap_or_else(|| "[unclassified]".into());
        let believed = match (with_believed, r.believed) {
            (true, Some(true)) => "  believed",
            (true, Some(false)) => "  (losing)",
            _ => "",
        };
        println!(
            "  {} {} {}{}",
            r.claim.claim_key,
            kind,
            truncate(&r.claim.body, 60),
            believed
        );
    }
    if rows.is_empty() {
        println!("  (none)");
    }
}

fn truncate(s: &str, n: usize) -> String {
    let collapsed: String = s.split_whitespace().collect::<Vec<_>>().join(" ");
    if collapsed.chars().count() <= n {
        collapsed
    } else {
        collapsed.chars().take(n).collect::<String>() + "…"
    }
}

const HELP: &str = "\
commands:
  obs <body>            observe a claim (auto key obs-N, observer 'repl', B/2)
                        bodies containing overdue/slipping/inbound/moved/PTO
                        or ending in '?' get classified by the rules tier
  retract <key>         retract a claim (try: retract halcyon:inv-1042:v2)
  merge <e:id> <e:id>   alias one entity onto another
                        (try: merge person:samuel person:sam)
  scope <name>          switch lens (delivery-health | cash-and-collections)
  scopes                list lenses
  sit                   reprint the current situation
  loops                 open loops view      unclassified   escalation queue
  live                  all live claims (with believed flags)
  tick <days>           advance the manual clock; P1 re-scores on the next
                        write (P2's tick driver makes decay automatic)
  crash                 abort() mid-session — relaunch to see WAL recovery
  reset                 wipe the store and reseed the demo world
  help                  this text            quit           exit cleanly";

// ---------------------------------------------------------------------------
// main loop
// ---------------------------------------------------------------------------

fn open_world(dir: &Path) -> Result<(Clog, u64), ClogError> {
    let c = Clog::open(demo_config(dir))?;
    c.advance(BOOT_NOW)?;
    let live = c.select(View::Live, Filter::default())?;
    if live.is_empty() {
        seed_world(&c)?;
    }
    Ok((c, BOOT_NOW))
}

fn next_obs_key(c: &Clog) -> String {
    let live = c.select(View::Live, Filter::default()).unwrap_or_default();
    let max = live
        .iter()
        .filter_map(|r| r.claim.claim_key.strip_prefix("obs-"))
        .filter_map(|n| n.parse::<u64>().ok())
        .max()
        .unwrap_or(0);
    format!("obs-{}", max + 1)
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let dir = PathBuf::from("clog-repl-data");
    if args.iter().any(|a| a == "--fresh") && dir.exists() {
        let _ = std::fs::remove_dir_all(&dir);
    }

    let style = Style {
        on: std::io::stdout().is_terminal() && std::env::var_os("NO_COLOR").is_none(),
    };

    let (mut world, mut now) = match open_world(&dir) {
        Ok(w) => w,
        Err(e) => {
            eprintln!("failed to open {}: {e}", dir.display());
            std::process::exit(1);
        }
    };
    let mut scope = "delivery-health".to_string();

    println!(
        "{}",
        style.dim(&format!(
            "clog demo repl · store: {} · type 'help' for commands",
            dir.display()
        ))
    );
    print_situation(&world, &scope, &style);

    let stdin = std::io::stdin();
    loop {
        print!("{}> ", style.dim(&scope));
        let _ = std::io::stdout().flush();
        let Some(Ok(line)) = stdin.lock().lines().next() else {
            break;
        };
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let (cmd, rest) = line.split_once(' ').unwrap_or((line, ""));
        let rest = rest.trim();

        match cmd {
            "obs" if !rest.is_empty() => {
                let key = next_obs_key(&world);
                let claim = Claim {
                    claim_key: key.clone(),
                    subject_key: None,
                    source_ref: format!("repl:{key}"),
                    observer: ObserverId::from("repl"),
                    schema_v: 1,
                    occurred_at: now,
                    observed_at: now,
                    reliability: Reliability::B,
                    credibility: Credibility::Two,
                    entities: vec![],
                    body: rest.to_string(),
                };
                match world.observe(vec![claim], ObserveOpts::default()) {
                    Ok(ack) => {
                        let kind = world
                            .select(View::Live, Filter::default())
                            .ok()
                            .and_then(|rows| rows.into_iter().find(|r| r.claim.claim_key == key))
                            .and_then(|r| r.kind)
                            .map(|k| k.kind)
                            .unwrap_or_else(|| "unclassified".into());
                        println!(
                            "{}",
                            style.ok(&format!("observed {key} [{kind}] rev {}", ack.rev))
                        );
                        print_situation(&world, &scope, &style);
                    }
                    Err(e) => println!("{}", style.err(&format!("observe failed: {e}"))),
                }
            }
            "obs" => println!("usage: obs <body>"),
            "retract" if !rest.is_empty() => match world.retract(rest) {
                Ok(ack) => {
                    println!("{}", style.ok(&format!("retracted {rest} rev {}", ack.rev)));
                    print_situation(&world, &scope, &style);
                }
                Err(e) => println!("{}", style.err(&format!("retract failed: {e}"))),
            },
            "retract" => println!("usage: retract <claim_key>"),
            "merge" => {
                let parts: Vec<&str> = rest.split_whitespace().collect();
                let parse = |s: &str| s.split_once(':').map(|(t, i)| entity(t, i));
                match (
                    parts.first().and_then(|s| parse(s)),
                    parts.get(1).and_then(|s| parse(s)),
                ) {
                    (Some(alias), Some(canonical)) => {
                        match world.merge_entities(&alias, &canonical) {
                            Ok(ack) => {
                                println!(
                                    "{}",
                                    style.ok(&format!(
                                        "merged {}:{} -> {}:{} rev {}",
                                        alias.etype,
                                        alias.id,
                                        canonical.etype,
                                        canonical.id,
                                        ack.rev
                                    ))
                                );
                                print_situation(&world, &scope, &style);
                            }
                            Err(e) => println!("{}", style.err(&format!("merge failed: {e}"))),
                        }
                    }
                    _ => println!("usage: merge <etype:id> <etype:id>"),
                }
            }
            "scope" if !rest.is_empty() => {
                if world.situation(Some(rest), None).is_ok() {
                    scope = rest.to_string();
                    print_situation(&world, &scope, &style);
                } else {
                    println!("{}", style.err(&format!("unknown scope: {rest}")));
                }
            }
            "scope" | "scopes" => {
                println!("  delivery-health\n  cash-and-collections\n  default");
            }
            "sit" => print_situation(&world, &scope, &style),
            "loops" => match world.select(View::OpenLoops, Filter::default()) {
                Ok(rows) => print_rows(&rows, false),
                Err(e) => println!("{}", style.err(&format!("select failed: {e}"))),
            },
            "unclassified" => match world.select(View::Unclassified, Filter::default()) {
                Ok(rows) => print_rows(&rows, false),
                Err(e) => println!("{}", style.err(&format!("select failed: {e}"))),
            },
            "live" => match world.select(View::Live, Filter::default()) {
                Ok(rows) => print_rows(&rows, true),
                Err(e) => println!("{}", style.err(&format!("select failed: {e}"))),
            },
            "tick" => {
                let days: f64 = rest.parse().unwrap_or(1.0);
                let ms = (days * DAY_MS as f64) as u64;
                match world.advance(ms) {
                    Ok(()) => {
                        now += ms;
                        println!(
                            "{}",
                            style.ok(&format!(
                                "advanced {days} day(s); recency re-scores on the next write \
                                 (P2's tick driver makes this automatic)"
                            ))
                        );
                    }
                    Err(e) => println!("{}", style.err(&format!("tick failed: {e}"))),
                }
            }
            "crash" => {
                println!(
                    "{}",
                    style.err("aborting mid-session — relaunch to watch the WAL restore the world")
                );
                let _ = std::io::stdout().flush();
                std::process::abort();
            }
            "reset" => {
                drop(world);
                let _ = std::fs::remove_dir_all(&dir);
                match open_world(&dir) {
                    Ok((w, n)) => {
                        world = w;
                        now = n;
                        println!("{}", style.ok("store wiped and reseeded"));
                        print_situation(&world, &scope, &style);
                    }
                    Err(e) => {
                        eprintln!("failed to reopen {}: {e}", dir.display());
                        std::process::exit(1);
                    }
                }
            }
            "help" => println!("{HELP}"),
            "quit" | "exit" => break,
            other => println!("unknown command: {other} (try 'help')"),
        }
    }
}
