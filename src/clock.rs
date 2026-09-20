//! Monotonic high-resolution timing.
//!
//! Everything downstream is inter-event *intervals*, so what matters is
//! resolution and monotonicity, not absolute accuracy. QPC gives sub-microsecond
//! resolution and is monotonic across cores. `GetMessageTime` (10-16 ms) and
//! `timeGetTime` (1 ms) are both far too coarse to see a 1 kHz mouse.

use windows_sys::Win32::System::Performance::{
    QueryPerformanceCounter, QueryPerformanceFrequency,
};

#[derive(Clone, Copy)]
pub struct Clock {
    freq: i64,
    origin: i64,
}

impl Clock {
    pub fn new() -> Self {
        let mut freq = 0i64;
        let mut origin = 0i64;
        // SAFETY: both write through a valid out-pointer and cannot fail on
        // any Windows version that supports this binary (XP+).
        unsafe {
            QueryPerformanceFrequency(&mut freq);
            QueryPerformanceCounter(&mut origin);
        }
        debug_assert!(freq > 0);
        Self { freq, origin }
    }

    /// Nanoseconds since this clock was created.
    ///
    /// Split into whole seconds and remainder before scaling so the intermediate
    /// never overflows i64, which a naive `ticks * 1_000_000_000` does after
    /// roughly 3 seconds on a 10 MHz counter.
    #[inline(always)]
    pub fn now_ns(&self) -> u64 {
        let mut c = 0i64;
        // SAFETY: valid out-pointer; QPC never fails post-XP.
        unsafe { QueryPerformanceCounter(&mut c) };
        let ticks = c.wrapping_sub(self.origin).max(0);
        let secs = ticks / self.freq;
        let rem = ticks % self.freq;
        (secs as u64) * 1_000_000_000 + (rem as u64) * 1_000_000_000 / (self.freq as u64)
    }

    pub fn freq(&self) -> i64 {
        self.freq
    }
}

/// Wall-clock milliseconds since the Unix epoch. Session metadata only -- never
/// used for anything that needs monotonicity.
pub fn unix_millis() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// ISO-8601 UTC, for human-readable session headers.
pub fn iso8601_utc(unix_ms: u64) -> String {
    // Civil-from-days (Howard Hinnant's algorithm). Avoids pulling in chrono
    // for what is literally one timestamp per session.
    let secs = (unix_ms / 1000) as i64;
    let ms = unix_ms % 1000;
    let days = secs.div_euclid(86_400);
    let tod = secs.rem_euclid(86_400);

    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };

    format!(
        "{:04}-{:02}-{:02}T{:02}:{:02}:{:02}.{:03}Z",
        y, m, d, tod / 3600, (tod % 3600) / 60, tod % 60, ms
    )
}
