//! RFC3339 (UTC, seconds precision) timestamp formatting (spec §5.8,
//! header slot: `{as_of RFC3339}`).

/// Formats `ms` (milliseconds since the Unix epoch, UTC) as
/// `YYYY-MM-DDTHH:MM:SSZ`.
///
/// Sub-second precision is dropped (truncated, not rounded). Uses Howard
/// Hinnant's civil-from-days algorithm (proleptic Gregorian, no external
/// crate) to go from a day count to a calendar date; this is exact for
/// every `u64` millisecond value representable here (no leap seconds are
/// modeled, matching Unix time).
// Not yet consumed by production code: used by the renderer's `header`
// slot in a later task. Exercised directly by this module's tests in the
// meantime.
#[allow(dead_code)]
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rfc3339_known_values() {
        assert_eq!(rfc3339_utc(0), "1970-01-01T00:00:00Z");
        assert_eq!(rfc3339_utc(86_400_000), "1970-01-02T00:00:00Z");
        // 2000-03-01 is the canonical leap-era edge in the civil algorithm
        assert_eq!(rfc3339_utc(951_868_800_000), "2000-03-01T00:00:00Z");
        // task-8-brief.md gave 1_755_216_000_000 / 1_755_262_496_000 for
        // these two rows, but those ms values are actually 2025-08-15 (one
        // year off from the intended "today" sanity check per the RFC3339
        // algorithm above and independent verification). Corrected here to
        // the ms values that actually correspond to 2026-08-15.
        assert_eq!(rfc3339_utc(1_786_752_000_000), "2026-08-15T00:00:00Z");
        assert_eq!(rfc3339_utc(1_786_798_496_000), "2026-08-15T12:54:56Z");
    }
}
