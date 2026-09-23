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
    /// Wall-clock time corresponding to `now_ns() == 0`.
    ///
    /// Event timestamps are relative to this clock, which is created once per
    /// *process* -- not per session. Without this anchor there is no way to map
    /// an event back to a wall-clock instant, because a session started hours
    /// later still carries timestamps counted from here.
    origin_unix_ms: u64,
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
        Self { freq, origin, origin_unix_ms: unix_millis() }
    }

    /// Wall-clock milliseconds at `t_ns == 0`.
    pub fn origin_unix_ms(&self) -> u64 {
        self.origin_unix_ms
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

// Return codes of GetTimeZoneInformation (winbase.h). Declared here because
// windows-sys does not export them in Win32::System::Time; the values are
// stable Win32 ABI.
const TZ_ID_DAYLIGHT: u32 = 2;
const TZ_ID_INVALID: u32 = 0xFFFF_FFFF;

/// Minutes to add to UTC to get local time.
///
/// Needed because every timestamp here is UTC, and the whole point of dating a
/// session is to ask whether someone moves differently at 2am. Without the
/// offset that question is answered in the wrong timezone.
pub fn local_offset_minutes() -> i32 {
    use windows_sys::Win32::System::Time::{GetTimeZoneInformation, TIME_ZONE_INFORMATION};
    // SAFETY: zeroed is a valid starting state; the call fills it in.
    let mut tz: TIME_ZONE_INFORMATION = unsafe { std::mem::zeroed() };
    let rc = unsafe { GetTimeZoneInformation(&mut tz) };
    if rc == TZ_ID_INVALID {
        return 0;
    }
    // Win32 defines UTC = local + Bias, so local = UTC - Bias. DaylightBias
    // applies only while DST is in force.
    let extra = if rc == TZ_ID_DAYLIGHT { tz.DaylightBias } else { tz.StandardBias };
    -(tz.Bias + extra)
}

/// Current timezone name, e.g. "Eastern Daylight Time".
pub fn local_timezone_name() -> String {
    use windows_sys::Win32::System::Time::{GetTimeZoneInformation, TIME_ZONE_INFORMATION};
    // SAFETY: as above.
    let mut tz: TIME_ZONE_INFORMATION = unsafe { std::mem::zeroed() };
    let rc = unsafe { GetTimeZoneInformation(&mut tz) };
    if rc == TZ_ID_INVALID {
        return String::new();
    }
    let name = if rc == TZ_ID_DAYLIGHT { &tz.DaylightName } else { &tz.StandardName };
    let end = name.iter().position(|&c| c == 0).unwrap_or(name.len());
    String::from_utf16_lossy(&name[..end])
}

/// Day of the week for a Unix millisecond timestamp. 0 = Sunday.
pub fn weekday(unix_ms: u64) -> u8 {
    // 1970-01-01 was a Thursday, which is index 4 when Sunday is 0.
    let days = (unix_ms / 1000) as i64 / 86_400;
    ((days + 4).rem_euclid(7)) as u8
}

pub const WEEKDAY_NAMES: [&str; 7] =
    ["Sunday", "Monday", "Tuesday", "Wednesday", "Thursday", "Friday", "Saturday"];

/// Day of the week in **local** time. 0 = Sunday.
///
/// Exists because calling `weekday()` on a UTC timestamp is wrong for anyone
/// west of Greenwich and silently produces tomorrow's day for every evening
/// session. That bug shipped: in a UTC-5 timezone every session from 19:00
/// local onward was labelled with the next day, which corrupts exactly the
/// day-of-week grouping this metadata exists to support.
pub fn local_weekday(unix_ms: u64, offset_minutes: i32) -> u8 {
    let local = (unix_ms as i64 + (offset_minutes as i64) * 60_000).max(0) as u64;
    weekday(local)
}

/// Local weekday as a name.
pub fn local_weekday_name(unix_ms: u64, offset_minutes: i32) -> &'static str {
    WEEKDAY_NAMES[local_weekday(unix_ms, offset_minutes) as usize]
}

/// Hour of the day, 0-23, in local time.
pub fn local_hour(unix_ms: u64, offset_minutes: i32) -> u8 {
    let local = (unix_ms as i64 / 1000) + (offset_minutes as i64) * 60;
    (local.rem_euclid(86_400) / 3600) as u8
}

/// A coarse label for time-of-day analysis.
pub fn day_part(hour: u8) -> &'static str {
    match hour {
        5..=11 => "morning",
        12..=16 => "afternoon",
        17..=21 => "evening",
        22 | 23 => "night",
        _ => "late_night",
    }
}

/// ISO-8601 local time with the offset spelled out, e.g.
/// `2026-09-20T16:07:59.457-04:00`.
pub fn iso8601_local(unix_ms: u64, offset_minutes: i32) -> String {
    let shifted = (unix_ms as i64 + (offset_minutes as i64) * 60_000).max(0) as u64;
    let base = iso8601_utc(shifted);
    let sign = if offset_minutes < 0 { '-' } else { '+' };
    let a = offset_minutes.abs();
    format!("{}{}{:02}:{:02}", base.trim_end_matches('Z'), sign, a / 60, a % 60)
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn weekday_matches_known_dates() {
        // 1970-01-01 was a Thursday.
        assert_eq!(WEEKDAY_NAMES[weekday(0) as usize], "Thursday");
        // 2000-01-01 was a Saturday. 946_684_800 s since epoch.
        assert_eq!(WEEKDAY_NAMES[weekday(946_684_800_000) as usize], "Saturday");
        // 2026-09-20 was a Sunday: 20716 days since the epoch.
        assert_eq!(WEEKDAY_NAMES[weekday(20_716 * 86_400_000) as usize], "Sunday");
        // ...and the day before it a Saturday, so the sequence steps correctly.
        assert_eq!(WEEKDAY_NAMES[weekday(20_715 * 86_400_000) as usize], "Saturday");
    }

    #[test]
    fn local_weekday_uses_local_time_not_utc() {
        // 2026-09-23T04:40Z is a Wednesday in UTC, but 23:40 Tuesday at UTC-5.
        // The shipped bug reported Wednesday for exactly this case.
        let ms = 20_719 * 86_400_000u64 + (4 * 3600 + 40 * 60) * 1000;
        assert_eq!(WEEKDAY_NAMES[weekday(ms) as usize], "Wednesday", "utc sanity");
        assert_eq!(local_weekday_name(ms, -300), "Tuesday");
        assert_eq!(local_hour(ms, -300), 23);
        // East of Greenwich the shift can go the other way.
        assert_eq!(local_weekday_name(ms, 600), "Wednesday");
    }

    #[test]
    fn local_hour_applies_the_offset_and_wraps() {
        // 1970-01-01T00:30Z
        let t = 30 * 60 * 1000;
        assert_eq!(local_hour(t, 0), 0);
        // UTC-5 pushes it to the previous day, 19:30 local.
        assert_eq!(local_hour(t, -5 * 60), 19);
        // UTC+5:30 gives 06:00 local.
        assert_eq!(local_hour(t, 330), 6);
    }

    #[test]
    fn day_part_covers_every_hour() {
        for h in 0..24u8 {
            assert!(!day_part(h).is_empty(), "hour {h} unlabelled");
        }
        assert_eq!(day_part(2), "late_night");
        assert_eq!(day_part(9), "morning");
        assert_eq!(day_part(14), "afternoon");
        assert_eq!(day_part(19), "evening");
        assert_eq!(day_part(23), "night");
    }

    #[test]
    fn iso_local_carries_the_offset() {
        // 2026-09-20T20:07:59.457Z at UTC-4 is 16:07 local.
        let ms = 1_789_070_879_457u64;
        let utc = iso8601_utc(ms);
        assert!(utc.ends_with('Z'), "{utc}");
        let local = iso8601_local(ms, -240);
        assert!(local.ends_with("-04:00"), "{local}");
        assert!(!local.contains('Z'), "{local}");
        // And a positive offset formats with a plus.
        assert!(iso8601_local(ms, 330).ends_with("+05:30"));
    }

    /// The anchor that makes process-relative event timestamps absolute.
    #[test]
    fn clock_origin_maps_t_ns_back_to_wall_clock() {
        let c = Clock::new();
        let origin = c.origin_unix_ms();
        let before = unix_millis();
        assert!(origin > 1_700_000_000_000, "origin looks unset: {origin}");
        assert!(origin <= before + 50, "origin is in the future");

        // Let a little real time pass, then check the documented mapping:
        //   wall_unix_ms = clock_origin_unix_ms + t_ns / 1_000_000
        std::thread::sleep(std::time::Duration::from_millis(60));
        let t_ns = c.now_ns();
        let mapped = origin + t_ns / 1_000_000;
        let now = unix_millis();
        let skew = (mapped as i64 - now as i64).abs();
        assert!(skew < 250, "mapping drifted {skew} ms (mapped {mapped}, now {now})");
        assert!(t_ns >= 50_000_000, "clock barely advanced: {t_ns} ns");
    }

    #[test]
    fn live_offset_is_a_plausible_timezone() {
        let o = local_offset_minutes();
        assert!((-12 * 60..=14 * 60).contains(&o), "implausible offset {o}");
        // Real timezones are whole quarter-hours.
        assert_eq!(o % 15, 0, "offset {o} is not a quarter-hour");
    }
}
