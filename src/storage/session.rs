//! Session lifecycle, metadata and crash recovery.
//!
//! One session = one directory. Everything needed to interpret the events in
//! it lives beside them, so a dataset copied off this machine years from now is
//! still self-describing: which mouse, which monitors, which pointer-ballistics
//! settings, which build of this program.
//!
//! RECOVERY RULE (spec section 45): a previous session that never wrote a clean
//! shutdown is *marked* interrupted. It is never repaired, truncated, merged or
//! deleted. Raw data is immutable even when it is damaged -- especially then,
//! because a torn session is still evidence about what the recorder did.

use crate::clock::{iso8601_utc, unix_millis};
use crate::context::monitors::MonitorInfo;
use crate::input::ballistics::Ballistics;
use crate::input::devices::DeviceInfo;
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

pub const APP_VERSION: &str = env!("CARGO_PKG_VERSION");

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SessionHeader {
    pub session_id: String,
    pub app_version: String,
    pub format_version: u32,

    pub start_wall_utc: String,
    pub start_unix_ms: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub end_wall_utc: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub end_unix_ms: Option<u64>,

    /// QPC frequency. Required to interpret `t_ns` provenance and to detect a
    /// machine whose counter behaves unusually.
    pub qpc_frequency: i64,

    pub devices: Vec<DeviceInfo>,
    pub monitors: Vec<MonitorInfo>,
    pub ballistics: Ballistics,
    pub os: OsInfo,

    // ---- health (spec section 43): never hide data loss -------------------
    /// `WM_INPUT` reports received from devices. One report can produce
    /// several events, so `events_written` is legitimately larger than this.
    pub reports_received: u64,
    pub events_written: u64,
    pub events_dropped: u64,
    pub peak_queue_depth: u64,
    pub max_write_latency_us: u64,
    pub writer_errors: u64,

    /// False until the program has flushed everything and said so explicitly.
    pub clean_shutdown: bool,
    /// Set by a *later* run that found this session unclean.
    pub interrupted: bool,
    /// Why a new session was started.
    pub start_reason: String,

    pub segments: Vec<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize, Default)]
pub struct OsInfo {
    pub version: String,
    pub computer_name: String,
}

impl OsInfo {
    pub fn probe() -> Self {
        Self {
            version: os_version_string(),
            computer_name: std::env::var("COMPUTERNAME").unwrap_or_default(),
        }
    }
}

fn os_version_string() -> String {
    use windows_sys::Win32::System::SystemInformation::GetVersion;
    // GetVersion is shimmed on modern Windows, so this is a coarse marker, not
    // a precise build id. Recorded as-is rather than guessed at.
    // SAFETY: no arguments, no failure mode.
    let v = unsafe { GetVersion() };
    format!("{}.{}.{}", v & 0xFF, (v >> 8) & 0xFF, (v >> 16) & 0xFFFF)
}

pub struct Session {
    pub header: SessionHeader,
    pub dir: PathBuf,
}

impl Session {
    pub fn create(root: &Path, reason: &str, qpc_frequency: i64) -> std::io::Result<Self> {
        let now_ms = unix_millis();
        let iso = iso8601_utc(now_ms);
        // Filesystem-safe variant of the timestamp, plus a short suffix so two
        // sessions started in the same millisecond cannot collide.
        let stamp = iso.replace([':', '.'], "-").replace('Z', "");
        let id = format!("{}_{:04x}", stamp, std::process::id() & 0xFFFF);

        let dir = root.join("sessions").join(&id);
        std::fs::create_dir_all(&dir)?;

        let header = SessionHeader {
            session_id: id,
            app_version: APP_VERSION.to_string(),
            format_version: crate::storage::segment::FORMAT_VERSION,
            start_wall_utc: iso,
            start_unix_ms: now_ms,
            end_wall_utc: None,
            end_unix_ms: None,
            qpc_frequency,
            devices: Vec::new(),
            monitors: Vec::new(),
            ballistics: Ballistics::default(),
            os: OsInfo::probe(),
            reports_received: 0,
            events_written: 0,
            events_dropped: 0,
            peak_queue_depth: 0,
            max_write_latency_us: 0,
            writer_errors: 0,
            clean_shutdown: false,
            interrupted: false,
            start_reason: reason.to_string(),
            segments: Vec::new(),
        };

        let s = Self { header, dir };
        // Write the unclean header immediately: if we die in the next second,
        // the next run must still find evidence this session existed.
        s.save()?;
        Ok(s)
    }

    /// Create a session with device, monitor and ballistics metadata already
    /// filled in.
    ///
    /// Sessions roll over mid-run (user request, wake from sleep), and each new
    /// one must capture the environment as it is *now* -- the monitor layout or
    /// the pointer-speed slider may well be why the rollover happened.
    pub fn create_populated(root: &Path, reason: &str, qpc_frequency: i64) -> std::io::Result<Self> {
        let mut s = Self::create(root, reason, qpc_frequency)?;
        s.header.ballistics = crate::input::ballistics::Ballistics::snapshot();
        s.header.monitors = crate::context::monitors::enumerate();
        s.header.devices = crate::input::devices::DeviceTable::new().devices().to_vec();
        s.save()?;
        Ok(s)
    }

    pub fn save(&self) -> std::io::Result<()> {
        let path = self.dir.join("session.json");
        let tmp = self.dir.join("session.json.tmp");
        let json = serde_json::to_string_pretty(&self.header)?;
        std::fs::write(&tmp, json)?;
        // Atomic replace, so a crash mid-write can never leave an unparseable
        // header and lose the pointer to otherwise-good event data.
        std::fs::rename(&tmp, &path)
    }

    pub fn segment_path(&self, index: u32) -> PathBuf {
        self.dir.join(format!("events-{index:04}.mpseg"))
    }

    pub fn context_path(&self) -> PathBuf {
        self.dir.join("context.jsonl")
    }

    pub fn finish(&mut self) -> std::io::Result<()> {
        let ms = unix_millis();
        self.header.end_unix_ms = Some(ms);
        self.header.end_wall_utc = Some(iso8601_utc(ms));
        self.header.clean_shutdown = true;
        self.save()
    }
}

/// Outcome of the startup scan.
#[derive(Debug, Default)]
pub struct RecoveryReport {
    pub scanned: usize,
    pub newly_marked_interrupted: Vec<String>,
}

/// Mark any previous session that never recorded a clean shutdown.
///
/// Deliberately conservative: it only ever flips `interrupted` to true on a
/// header that is already `clean_shutdown: false`. No event data is read,
/// rewritten or removed.
pub fn recover_interrupted(root: &Path) -> std::io::Result<RecoveryReport> {
    let mut report = RecoveryReport::default();
    let sessions = root.join("sessions");
    if !sessions.exists() {
        return Ok(report);
    }

    for entry in std::fs::read_dir(&sessions)? {
        let dir = match entry {
            Ok(e) => e.path(),
            Err(_) => continue,
        };
        if !dir.is_dir() {
            continue;
        }
        let hp = dir.join("session.json");
        let Ok(text) = std::fs::read_to_string(&hp) else {
            continue;
        };
        report.scanned += 1;

        let Ok(mut h) = serde_json::from_str::<SessionHeader>(&text) else {
            // An unparseable header is itself a symptom. Leave it alone: a
            // human can look, and the events beside it are still readable.
            continue;
        };
        if h.clean_shutdown || h.interrupted {
            continue;
        }
        h.interrupted = true;
        let id = h.session_id.clone();
        if let Ok(json) = serde_json::to_string_pretty(&h) {
            let tmp = dir.join("session.json.tmp");
            if std::fs::write(&tmp, json).is_ok() && std::fs::rename(&tmp, &hp).is_ok() {
                report.newly_marked_interrupted.push(id);
            }
        }
    }
    Ok(report)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp_root(name: &str) -> PathBuf {
        let mut p = std::env::temp_dir();
        p.push(format!("mp_sess_{}_{}", name, std::process::id()));
        let _ = std::fs::remove_dir_all(&p);
        p
    }

    #[test]
    fn a_new_session_is_unclean_until_it_says_otherwise() {
        let root = tmp_root("clean");
        let mut s = Session::create(&root, "test", 10_000_000).unwrap();
        let text = std::fs::read_to_string(s.dir.join("session.json")).unwrap();
        let h: SessionHeader = serde_json::from_str(&text).unwrap();
        assert!(!h.clean_shutdown, "header must be written unclean up front");

        s.finish().unwrap();
        let text = std::fs::read_to_string(s.dir.join("session.json")).unwrap();
        let h: SessionHeader = serde_json::from_str(&text).unwrap();
        assert!(h.clean_shutdown);
        assert!(h.end_unix_ms.is_some());
        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn recovery_marks_a_crashed_session_and_leaves_a_clean_one_alone() {
        let root = tmp_root("recover");
        // One crashed (never finished), one clean.
        let crashed = Session::create(&root, "crash", 10_000_000).unwrap();
        let crashed_id = crashed.header.session_id.clone();
        drop(crashed);

        std::thread::sleep(std::time::Duration::from_millis(2));
        let mut ok = Session::create(&root, "ok", 10_000_000).unwrap();
        ok.finish().unwrap();
        let ok_id = ok.header.session_id.clone();

        let rep = recover_interrupted(&root).unwrap();
        assert_eq!(rep.scanned, 2);
        assert_eq!(rep.newly_marked_interrupted, vec![crashed_id.clone()]);

        // And it is idempotent -- a second run marks nothing new.
        let rep2 = recover_interrupted(&root).unwrap();
        assert!(rep2.newly_marked_interrupted.is_empty());

        let h: SessionHeader = serde_json::from_str(
            &std::fs::read_to_string(root.join("sessions").join(&ok_id).join("session.json"))
                .unwrap(),
        )
        .unwrap();
        assert!(!h.interrupted, "a clean session must never be marked interrupted");
        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn an_unreadable_header_is_left_for_a_human() {
        let root = tmp_root("garbage");
        let d = root.join("sessions").join("weird");
        std::fs::create_dir_all(&d).unwrap();
        std::fs::write(d.join("session.json"), "{ not json at all").unwrap();

        let rep = recover_interrupted(&root).unwrap();
        assert!(rep.newly_marked_interrupted.is_empty());
        // Untouched.
        assert_eq!(
            std::fs::read_to_string(d.join("session.json")).unwrap(),
            "{ not json at all"
        );
        std::fs::remove_dir_all(&root).ok();
    }
}
