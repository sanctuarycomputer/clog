//! Recovery tests (Task 18): INV-11, crash safety.
//!
//! R1 kills a real process at every WAL commit point and proves the reopened
//! world equals a fresh world fed exactly the durable prefix — same document,
//! byte for byte, same rev — and that the survivor still accepts writes.
//!
//! R3 pins the `rebuild_on_open` contract: rebuilding from the WAL must
//! produce the same world as a normal open. In P1 every open already is a
//! full rebuild (there is no engine-state cache yet), so this is cheap today
//! and load-bearing the moment a cache lands.
//!
//! The clock is always `Manual` (INV-10) so both sides of every comparison
//! commit at identical readings.

use clog::*;

/// The fixed clock reading every scripted batch commits at. Both the crashed
/// instance and the fresh comparison instance advance to it before writing,
/// and each replayed batch re-renders against its own recorded `as_of`, so
/// the two documents agree on their headers as well as their content.
const SCRIPT_NOW_MS: u64 = 1_000_000;

/// The number of batches the script commits: revs 1..=4.
const SCRIPT_LEN: u64 = 4;

fn manual_cfg(dir: &std::path::Path) -> Config {
    let mut c = Config::default_for(dir);
    c.tick = TickConfig { mode: ClockMode::Manual, interval_ms: 60_000 };
    c
}

fn claim(key: &str, body: &str) -> Claim {
    Claim {
        claim_key: key.into(),
        subject_key: None,
        source_ref: "t:1".into(),
        observer: ObserverId::from("t"),
        schema_v: 1,
        occurred_at: 500_000,
        observed_at: 500_000,
        reliability: Reliability::B,
        credibility: Credibility::Two,
        entities: vec![],
        body: body.into(),
    }
}

/// Applies the `n`th batch of the script (1-based). Every step commits
/// exactly one batch, so "crashed after batch `n`" and "ran steps 1..=n"
/// describe the same durable prefix.
fn step(c: &Clog, n: u64) {
    match n {
        1 => {
            c.observe(vec![claim("a", "first")], ObserveOpts::default()).unwrap();
        }
        2 => {
            c.observe(vec![claim("b", "second")], ObserveOpts::default()).unwrap();
        }
        3 => {
            c.retract("a").unwrap();
        }
        4 => {
            c.observe(vec![claim("c", "third")], ObserveOpts::default()).unwrap();
        }
        _ => unreachable!("script has {SCRIPT_LEN} steps, asked for {n}"),
    }
}

/// Advances to the script's clock reading, then runs its first `n` batches.
fn run_prefix(c: &Clog, n: u64) {
    c.advance(SCRIPT_NOW_MS).unwrap();
    for i in 1..=n {
        step(c, i);
    }
}

/// R1: a child process aborts immediately after the Nth WAL append (post
/// fsync, pre apply); the parent reopens that directory and compares it
/// against a fresh instance fed the same first N batches.
///
/// The child is this very test binary, re-executed with `CLOG_R1_CHILD` set:
/// the crash hook lives behind `--features test-crash`, so the harness and
/// the code under test have to be the same build.
#[test]
#[cfg_attr(not(feature = "test-crash"), ignore = "needs --features test-crash")]
fn r1_crash_points() {
    if std::env::var_os("CLOG_R1_CHILD").is_some() {
        let dir = std::env::var("CLOG_R1_DIR").expect("child needs CLOG_R1_DIR");
        let c = Clog::open(manual_cfg(std::path::Path::new(&dir))).unwrap();
        run_prefix(&c, SCRIPT_LEN); // aborts partway via CLOG_CRASH_AFTER_WAL
        unreachable!("child should have crashed");
    }

    for crash_after in 1..=SCRIPT_LEN {
        let dir = tempfile::tempdir().unwrap();
        let exe = std::env::current_exe().unwrap();
        // Not `--ignored`: with `test-crash` on, this test is not ignored.
        let status = std::process::Command::new(&exe)
            .args(["r1_crash_points", "--exact", "--nocapture"])
            .env("CLOG_R1_CHILD", "1")
            .env("CLOG_R1_DIR", dir.path())
            .env("CLOG_CRASH_AFTER_WAL", crash_after.to_string())
            .status()
            .unwrap();
        assert!(!status.success(), "child must abort (crash point {crash_after})");
        // A panicking child would also be "unsuccessful", and would mean the
        // hook never fired — so insist on death by signal, i.e. `abort()`.
        #[cfg(unix)]
        {
            use std::os::unix::process::ExitStatusExt;
            /// `SIGABRT`, spelled out rather than pulled in as a dependency;
            /// it is 6 on every Unix clog builds on.
            const SIGABRT: i32 = 6;
            assert_eq!(
                status.signal(),
                Some(SIGABRT),
                "child must die in the crash hook, not fail as a test (crash point {crash_after})"
            );
        }

        // The reopened world == a fresh world fed the same durable prefix.
        let reopened = Clog::open(manual_cfg(dir.path())).unwrap();
        let fresh_dir = tempfile::tempdir().unwrap();
        let fresh = Clog::open(manual_cfg(fresh_dir.path())).unwrap();
        run_prefix(&fresh, crash_after);
        let (s1, s2) = (reopened.situation(None, None).unwrap(), fresh.situation(None, None).unwrap());
        assert_eq!(s1.text, s2.text, "crash point {crash_after}");
        assert_eq!(s1.rev, s2.rev, "crash point {crash_after}");
        assert_eq!(s1.as_of, s2.as_of, "crash point {crash_after}");

        // The survivor is not merely readable: it still writes, and the next
        // rev continues from the durable prefix rather than from a gap.
        reopened.advance(2_000_000).unwrap();
        let ack = reopened
            .observe(vec![claim("post", "after the crash")], ObserveOpts::default())
            .unwrap();
        assert_eq!(ack.rev, crash_after + 1, "crash point {crash_after}");
        assert!(reopened.situation(None, None).unwrap().text.contains("after the crash"));
    }
}

/// R3: `rebuild_on_open` produces the same world as a normal open.
#[test]
fn r3_rebuild_equals_open() {
    let dir = tempfile::tempdir().unwrap();
    {
        let c = Clog::open(manual_cfg(dir.path())).unwrap();
        run_prefix(&c, SCRIPT_LEN);
    } // drop -> clean shutdown

    let normal = Clog::open(manual_cfg(dir.path())).unwrap();
    let s1 = normal.situation(None, None).unwrap();
    drop(normal);

    let mut cfg2 = manual_cfg(dir.path());
    cfg2.rebuild_on_open = true;
    let rebuilt = Clog::open(cfg2).unwrap();
    let s2 = rebuilt.situation(None, None).unwrap();

    assert_eq!(s1.text, s2.text);
    assert_eq!(s1.rev, s2.rev);
    assert_eq!(s1.as_of, s2.as_of);
}
