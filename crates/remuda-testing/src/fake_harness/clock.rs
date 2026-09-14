//! Deterministic wall clock shared by every artifact writer.
//!
//! Real sleeps still happen (streamed chunks, simulated tool runtime) so a PTY
//! driver sees real timing, but every timestamp in transcript / rollout /
//! session files comes from this clock. Tests pin the epoch and therefore get
//! byte-comparable artifacts.

/// Monotonic millisecond clock starting at a fixed fixture epoch.
#[derive(Clone, Copy, Debug)]
pub struct FakeClock {
    now_ms: i64,
}

/// 2026-09-13T18:21:45.062Z — the first `task_started` timestamp in the
/// captured codex 0.154 interactive session, reused as the default epoch.
pub const DEFAULT_EPOCH_MS: i64 = 1_789_323_705_062;

impl FakeClock {
    /// Start at an explicit epoch.
    #[must_use]
    pub fn new(epoch_ms: i64) -> Self {
        Self { now_ms: epoch_ms }
    }

    /// Current Unix milliseconds.
    #[must_use]
    pub fn ms(&self) -> i64 {
        self.now_ms
    }

    /// Current Unix seconds.
    #[must_use]
    pub fn secs(&self) -> i64 {
        self.now_ms.div_euclid(1000)
    }

    /// Sleep the test process for `ms` and advance the clock by the same amount.
    pub fn sleep(&mut self, ms: u64) {
        if ms > 0 {
            std::thread::sleep(std::time::Duration::from_millis(ms));
            self.now_ms += ms as i64;
        }
    }

    /// Advance without sleeping (instantaneous bookkeeping).
    pub fn tick(&mut self, ms: u64) {
        self.now_ms += ms as i64;
    }

    /// RFC 3339 UTC timestamp with millisecond precision.
    #[must_use]
    pub fn rfc3339(&self) -> String {
        format_timestamp(self.now_ms)
    }
}

impl Default for FakeClock {
    fn default() -> Self {
        Self::new(DEFAULT_EPOCH_MS)
    }
}

/// Format Unix milliseconds as `YYYY-MM-DDTHH:MM:SS.mmmZ`.
#[must_use]
pub fn format_timestamp(ms: i64) -> String {
    let secs = ms.div_euclid(1000);
    let millis = ms.rem_euclid(1000);
    let days = secs.div_euclid(86_400);
    let mut rem = secs.rem_euclid(86_400);
    let hour = rem / 3600;
    rem %= 3600;
    let minute = rem / 60;
    let second = rem % 60;
    let (year, month, day) = civil_from_days(days);
    format!("{year:04}-{month:02}-{day:02}T{hour:02}:{minute:02}:{second:02}.{millis:03}Z")
}

/// Howard Hinnant's days-from-civil inverse, proleptic Gregorian.
fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let year = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let month = (if mp < 10 { mp + 3 } else { mp - 9 }) as u32;
    (year + i64::from(month <= 2), month, day)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn formats_the_fixture_epoch() {
        assert_eq!(
            format_timestamp(1_789_323_705_062),
            "2026-09-13T18:21:45.062Z"
        );
    }

    #[test]
    fn epoch_agrees_with_codex_fixture() {
        let clock = FakeClock::default();
        assert_eq!(clock.secs(), 1_789_323_705);
        assert!(clock.rfc3339().starts_with("2026-09-13T18:21:45"));
    }
}
