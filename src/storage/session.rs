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
    // ---- added after v0.1.0 ------------------------------------------------
    // Every field below carries `#[serde(default)]`.
    //
    // Without it, adding a field makes serde *require* it, and every session
    // recorded before the change stops deserializing -- which silently breaks
    // `list`, `export`, and most seriously `recover_interrupted`, which skips
    // headers it cannot parse and would therefore never mark an old crashed
    // session. The whole premise of this format is that raw data stays
    // readable years later, so a schema addition must never orphan existing
    // recordings.
    /// Local wall clock with offset, e.g. `2026-09-20T16:07:59.457-04:00`.
    ///
    /// Time-of-day behaviour is a first-class question for this dataset -- does
    /// this person move differently late at night? -- and UTC alone cannot
    /// answer it without knowing where the machine was.
    #[serde(default)]
    pub start_wall_local: String,
    #[serde(default)]
    pub utc_offset_minutes: i32,
    #[serde(default)]
    pub timezone: String,
    #[serde(default)]
    pub start_weekday: String,
    #[serde(default)]
    pub start_local_hour: u8,
    /// morning / afternoon / evening / night / late_night
    #[serde(default)]
    pub start_day_part: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub end_wall_utc: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub end_wall_local: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub end_unix_ms: Option<u64>,

    /// QPC frequency. Required to interpret `t_ns` provenance and to detect a
    /// machine whose counter behaves unusually.
    pub qpc_frequency: i64,
    /// Wall-clock milliseconds corresponding to `t_ns == 0`.
    ///
    /// Event timestamps are process-relative, not session-relative, so this is
    /// the anchor that makes them absolute:
    /// `wall_unix_ms = clock_origin_unix_ms + t_ns / 1_000_000`.
    /// Without it a session opened mid-run cannot be placed on a calendar.
    #[serde(default)]
    pub clock_origin_unix_ms: u64,
    /// The `t_ns` value at which this session began, for callers that would
    /// rather work in session-relative time.
    #[serde(default)]
    pub session_start_t_ns: u64,

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
    /// When loss happened, not just how much. Spec section 43 asks for the
    /// timestamp range; a bare count leaves you knowing events vanished but
    /// not which stretch of the recording to distrust.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub first_drop_t_ns: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_drop_t_ns: Option<u64>,
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

/// Which machine, and which account on it, produced this data.
///
/// The same person behaves differently on a desktop with a gaming mouse and on
/// a laptop trackpad, so a dataset that pools them without a machine label is
/// not analysable. `machine_guid` is the stable identity: `computer_name` can
/// be changed at any time, the GUID cannot.
#[derive(Clone, Debug, Serialize, Deserialize, Default)]
pub struct OsInfo {
    pub version: String,
    pub computer_name: String,
    /// Added after v0.1.0 -- see the note on `SessionHeader`.
    #[serde(default)]
    pub user_name: String,
    #[serde(default)]
    pub machine_guid: String,
}

impl OsInfo {
    pub fn probe() -> Self {
        Self {
            version: os_version_string(),
            computer_name: std::env::var("COMPUTERNAME").unwrap_or_default(),
            user_name: std::env::var("USERNAME").unwrap_or_default(),
            machine_guid: machine_guid(),
        }
    }
}

/// `HKLM\SOFTWARE\Microsoft\Cryptography\MachineGuid`: generated at install
/// and stable for the life of the Windows installation.
fn machine_guid() -> String {
    reg_sz(r"SOFTWARE\Microsoft\Cryptography", "MachineGuid")
}

/// Path to the key holding the true OS version.
const CURRENT_VERSION_KEY: &str = r"SOFTWARE\Microsoft\Windows NT\CurrentVersion";

/// The real Windows version, read from the registry.
///
/// Deliberately not `GetVersion`/`GetVersionEx`: since Windows 8.1 those are
/// shimmed and report 6.2.9200 ("Windows 8") to any process without a
/// compatibility manifest. Recording that would put a flatly false OS version
/// in every session header -- and these headers are meant to still be
/// trustworthy years from now. The registry value is not shimmed.
fn os_version_string() -> String {
    let product = reg_sz(CURRENT_VERSION_KEY, "ProductName");
    let display = reg_sz(CURRENT_VERSION_KEY, "DisplayVersion");
    let build = reg_sz(CURRENT_VERSION_KEY, "CurrentBuild");
    let ubr = reg_dword(CURRENT_VERSION_KEY, "UBR");

    // Windows 11 still says "Windows 10" in ProductName; build 22000+ is the
    // documented way to tell them apart.
    let name = if product.is_empty() {
        "Windows".to_string()
    } else if build.parse::<u32>().map(|b| b >= 22_000).unwrap_or(false) {
        product.replace("Windows 10", "Windows 11")
    } else {
        product
    };

    let mut out = name;
    if !display.is_empty() {
        out.push(' ');
        out.push_str(&display);
    }
    if !build.is_empty() {
        out.push_str(" (build ");
        out.push_str(&build);
        if let Some(u) = ubr {
            out.push('.');
            out.push_str(&u.to_string());
        }
        out.push(')');
    }
    out
}

/// Read a REG_SZ from HKLM, returning an empty string when absent.
fn reg_sz(subkey: &str, value: &str) -> String {
    hklm_value(subkey, value)
        .map(|(_, b)| {
            let u: Vec<u16> = b
                .chunks_exact(2)
                .map(|c| u16::from_le_bytes([c[0], c[1]]))
                .take_while(|&c| c != 0)
                .collect();
            String::from_utf16_lossy(&u)
        })
        .unwrap_or_default()
}

/// Read a REG_DWORD from HKLM.
fn reg_dword(subkey: &str, value: &str) -> Option<u32> {
    let (_, b) = hklm_value(subkey, value)?;
    if b.len() < 4 {
        return None;
    }
    Some(u32::from_le_bytes([b[0], b[1], b[2], b[3]]))
}

fn hklm_value(subkey: &str, value: &str) -> Option<(u32, Vec<u8>)> {
    use windows_sys::Win32::System::Registry::{
        RegCloseKey, RegOpenKeyExW, RegQueryValueExW, HKEY, HKEY_LOCAL_MACHINE, KEY_READ,
        KEY_WOW64_64KEY,
    };
    let sub: Vec<u16> = subkey.encode_utf16().chain(std::iter::once(0)).collect();
    let name: Vec<u16> = value.encode_utf16().chain(std::iter::once(0)).collect();
    let mut key: HKEY = std::ptr::null_mut();
    // KEY_WOW64_64KEY: read the 64-bit view regardless of this build's
    // bitness, so the value matches what other tools report.
    // SAFETY: NUL-terminated subkey; `key` receives the handle.
    let rc = unsafe {
        RegOpenKeyExW(HKEY_LOCAL_MACHINE, sub.as_ptr(), 0, KEY_READ | KEY_WOW64_64KEY, &mut key)
    };
    if rc != 0 {
        return None;
    }
    let mut ty = 0u32;
    let mut len = 0u32;
    // SAFETY: null buffer asks for the required size.
    unsafe {
        RegQueryValueExW(
            key,
            name.as_ptr(),
            std::ptr::null(),
            &mut ty,
            std::ptr::null_mut(),
            &mut len,
        )
    };
    if len == 0 || len > 4096 {
        // SAFETY: handle from a successful open.
        unsafe { RegCloseKey(key) };
        return None;
    }
    let mut buf = vec![0u8; len as usize];
    // SAFETY: buffer sized by the call above.
    let rc = unsafe {
        RegQueryValueExW(key, name.as_ptr(), std::ptr::null(), &mut ty, buf.as_mut_ptr(), &mut len)
    };
    // SAFETY: handle from a successful open, unused afterwards.
    unsafe { RegCloseKey(key) };
    if rc != 0 {
        return None;
    }
    buf.truncate(len as usize);
    Some((ty, buf))
}

pub struct Session {
    pub header: SessionHeader,
    pub dir: PathBuf,
}

impl Session {
    pub fn create(root: &Path, reason: &str, qpc_frequency: i64) -> std::io::Result<Self> {
        Self::create_at(root, reason, qpc_frequency, 0, 0)
    }

    /// As `create`, but anchoring the session to the process clock.
    pub fn create_at(
        root: &Path,
        reason: &str,
        qpc_frequency: i64,
        clock_origin_unix_ms: u64,
        session_start_t_ns: u64,
    ) -> std::io::Result<Self> {
        let now_ms = unix_millis();
        let iso = iso8601_utc(now_ms);
        // Filesystem-safe variant of the timestamp, plus a short suffix so two
        // sessions started in the same millisecond cannot collide.
        let stamp = iso.replace([':', '.'], "-").replace('Z', "");
        let id = format!("{}_{:04x}", stamp, std::process::id() & 0xFFFF);

        let dir = root.join("sessions").join(&id);
        std::fs::create_dir_all(&dir)?;

        let offset = crate::clock::local_offset_minutes();
        let hour = crate::clock::local_hour(now_ms, offset);
        let header = SessionHeader {
            session_id: id,
            app_version: APP_VERSION.to_string(),
            format_version: crate::storage::segment::FORMAT_VERSION,
            start_wall_utc: iso,
            start_unix_ms: now_ms,
            start_wall_local: crate::clock::iso8601_local(now_ms, offset),
            utc_offset_minutes: offset,
            timezone: crate::clock::local_timezone_name(),
            // Local, not UTC -- see `clock::local_weekday`.
            start_weekday: crate::clock::local_weekday_name(now_ms, offset).to_string(),
            start_local_hour: hour,
            start_day_part: crate::clock::day_part(hour).to_string(),
            end_wall_utc: None,
            end_wall_local: None,
            end_unix_ms: None,
            qpc_frequency,
            clock_origin_unix_ms,
            session_start_t_ns,
            devices: Vec::new(),
            monitors: Vec::new(),
            ballistics: Ballistics::default(),
            os: OsInfo::probe(),
            reports_received: 0,
            events_written: 0,
            events_dropped: 0,
            first_drop_t_ns: None,
            last_drop_t_ns: None,
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
    pub fn create_populated(
        root: &Path,
        reason: &str,
        clock: &crate::clock::Clock,
    ) -> std::io::Result<Self> {
        let mut s = Self::create_at(
            root,
            reason,
            clock.freq(),
            clock.origin_unix_ms(),
            clock.now_ns(),
        )?;
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
        self.header.end_wall_local =
            Some(crate::clock::iso8601_local(ms, self.header.utc_offset_minutes));
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

    /// A header written by an older build must still deserialize.
    ///
    /// This is the regression test for a real break: adding `user_name`,
    /// `machine_guid` and the local-time fields made serde require them, and
    /// every session recorded before that change became unreadable -- which
    /// also silently disabled crash recovery for those sessions, because
    /// `recover_interrupted` skips headers it cannot parse.
    #[test]
    fn a_header_from_an_older_build_still_loads() {
        // Exactly the v0.1.0 shape: no local-time fields, no user_name, no
        // machine_guid.
        let old = r#"{
            "session_id": "2026-09-20T19-45-40-092_55e8",
            "app_version": "0.1.0",
            "format_version": 1,
            "start_wall_utc": "2026-09-20T19:45:40.092Z",
            "start_unix_ms": 1789069540092,
            "qpc_frequency": 10000000,
            "devices": [],
            "monitors": [],
            "ballistics": {
                "pointer_speed": 7,
                "mouse_threshold1": 6,
                "mouse_threshold2": 10,
                "enhance_pointer_precision": 1,
                "smooth_mouse_x_curve": [],
                "smooth_mouse_y_curve": [],
                "curve_x": [],
                "curve_y": []
            },
            "os": { "version": "6.2.9200", "computer_name": "RYZEN-964" },
            "reports_received": 27000,
            "events_written": 27269,
            "events_dropped": 0,
            "peak_queue_depth": 21,
            "max_write_latency_us": 4424,
            "writer_errors": 0,
            "clean_shutdown": false,
            "interrupted": true,
            "start_reason": "startup",
            "segments": ["events-0000.mpseg"]
        }"#;

        let h: SessionHeader = serde_json::from_str(old).expect("old header must still parse");
        // The data that was actually recorded is intact...
        assert_eq!(h.events_written, 27269);
        assert_eq!(h.os.computer_name, "RYZEN-964");
        assert!(h.interrupted);
        // ...and the fields that did not exist yet default rather than fail.
        assert_eq!(h.os.user_name, "");
        assert_eq!(h.os.machine_guid, "");
        assert_eq!(h.utc_offset_minutes, 0);
        assert_eq!(h.start_wall_local, "");
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
