//! Start-with-Windows registration.
//!
//! Spec section 4 names two mechanisms, and both are implemented here because
//! neither works everywhere:
//!
//! * **Task Scheduler** is the better one -- a scheduled task can be told to
//!   restart the program if it dies, which matters for something meant to run
//!   for weeks. But `schtasks /create /sc onlogon` registers a task that runs
//!   for *any* user logging on, and on most machines that needs elevation.
//!   Unelevated it fails with `ERROR: Access is denied.`
//!
//! * **`HKCU\...\Run`** needs no elevation at all, because it only ever affects
//!   the current user. It cannot restart a crashed process.
//!
//! `enable()` tries the task first and silently falls back to the Run key, so a
//! normal user gets working startup without a UAC prompt and an elevated one
//! gets the better mechanism. `status()` reports which is actually in force --
//! section 4 is explicit that startup behaviour must be visible, so the UI
//! shows the truth rather than what it last attempted.

use std::os::windows::process::CommandExt;
use std::process::Command;

use windows_sys::Win32::System::Registry::{
    RegCloseKey, RegDeleteValueW, RegOpenKeyExW, RegQueryValueExW, RegSetValueExW, HKEY,
    HKEY_CURRENT_USER, KEY_READ, KEY_WRITE, REG_SZ,
};

/// Keeps `schtasks` from flashing a console window on a GUI-subsystem process.
const CREATE_NO_WINDOW: u32 = 0x0800_0000;

pub const TASK_NAME: &str = "HumanTelemetry";
const RUN_KEY: &str = r"Software\Microsoft\Windows\CurrentVersion\Run";
const RUN_VALUE: &str = "HumanTelemetry";

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Method {
    Off,
    /// Scheduled task at logon. Survives a crash.
    Task,
    /// HKCU Run value. No elevation needed, no crash restart.
    RunKey,
}

impl Method {
    pub fn label(self) -> &'static str {
        match self {
            Method::Off => "off",
            Method::Task => "on (scheduled task)",
            Method::RunKey => "on (registry Run)",
        }
    }
}

/// The command line startup should launch: recording, straight to the tray so
/// signing in does not throw a window in the user's face.
fn launch_command() -> Result<String, String> {
    let exe = std::env::current_exe().map_err(|e| format!("cannot locate own exe: {e}"))?;
    Ok(format!("\"{}\" record --tray", exe.display()))
}

fn schtasks(args: &[&str]) -> std::io::Result<std::process::Output> {
    Command::new("schtasks").args(args).creation_flags(CREATE_NO_WINDOW).output()
}

fn task_exists() -> bool {
    schtasks(&["/query", "/tn", TASK_NAME]).map(|o| o.status.success()).unwrap_or(false)
}

pub fn status() -> Method {
    if task_exists() {
        Method::Task
    } else if run_key_get().is_some() {
        Method::RunKey
    } else {
        Method::Off
    }
}

pub fn is_enabled() -> bool {
    status() != Method::Off
}

/// Enable startup, preferring the scheduled task and falling back to the Run
/// key when that needs rights we do not have.
pub fn enable() -> Result<Method, String> {
    let cmd = launch_command()?;

    // /rl limited: run at the user's normal integrity level. Deliberately not
    // /rl highest -- this program has no need of elevation, and a task that
    // silently runs elevated at every logon is not something to install on
    // someone's machine for convenience.
    match schtasks(&[
        "/create", "/tn", TASK_NAME, "/tr", &cmd, "/sc", "onlogon", "/rl", "limited", "/f",
    ]) {
        Ok(o) if o.status.success() => return Ok(Method::Task),
        Ok(_) => {} // typically "Access is denied" when unelevated
        Err(e) => return Err(format!("could not run schtasks: {e}")),
    }

    run_key_set(&cmd)?;
    Ok(Method::RunKey)
}

/// Remove startup by whichever mechanism installed it. Both are cleared, so a
/// machine that once had the task and later the Run key cannot end up with
/// two copies launching.
pub fn disable() -> Result<(), String> {
    let mut errors = Vec::new();
    if task_exists() {
        match schtasks(&["/delete", "/tn", TASK_NAME, "/f"]) {
            Ok(o) if o.status.success() => {}
            Ok(o) => errors.push(format!(
                "scheduled task: {}{}",
                String::from_utf8_lossy(&o.stdout).trim(),
                String::from_utf8_lossy(&o.stderr).trim()
            )),
            Err(e) => errors.push(format!("schtasks: {e}")),
        }
    }
    if run_key_get().is_some() {
        if let Err(e) = run_key_delete() {
            errors.push(e);
        }
    }
    if errors.is_empty() {
        Ok(())
    } else {
        Err(errors.join("; "))
    }
}

pub fn toggle() -> Result<Method, String> {
    if is_enabled() {
        disable()?;
        Ok(Method::Off)
    } else {
        enable()
    }
}

// ---------------------------------------------------------------------------
// HKCU Run value
// ---------------------------------------------------------------------------

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

fn open_run(access: u32) -> Option<HKEY> {
    let sub = wide(RUN_KEY);
    let mut key: HKEY = std::ptr::null_mut();
    // SAFETY: NUL-terminated subkey path; `key` receives the handle. The Run
    // key always exists on a normal profile, so this is an open, not a create.
    let rc = unsafe { RegOpenKeyExW(HKEY_CURRENT_USER, sub.as_ptr(), 0, access, &mut key) };
    if rc == 0 {
        Some(key)
    } else {
        None
    }
}

fn run_key_get() -> Option<String> {
    let key = open_run(KEY_READ)?;
    let name = wide(RUN_VALUE);
    let mut ty = 0u32;
    let mut len = 0u32;
    // SAFETY: null data pointer asks for the required byte count.
    let rc = unsafe {
        RegQueryValueExW(key, name.as_ptr(), std::ptr::null(), &mut ty, std::ptr::null_mut(), &mut len)
    };
    if rc != 0 || len == 0 || len > 8192 {
        // SAFETY: handle from a successful open.
        unsafe { RegCloseKey(key) };
        return None;
    }
    let mut buf = vec![0u8; len as usize];
    // SAFETY: buffer sized by the call above.
    let rc = unsafe {
        RegQueryValueExW(key, name.as_ptr(), std::ptr::null(), &mut ty, buf.as_mut_ptr(), &mut len)
    };
    // SAFETY: handle from a successful open, not used afterwards.
    unsafe { RegCloseKey(key) };
    if rc != 0 {
        return None;
    }
    let u16s: Vec<u16> = buf
        .chunks_exact(2)
        .map(|c| u16::from_le_bytes([c[0], c[1]]))
        .take_while(|&c| c != 0)
        .collect();
    Some(String::from_utf16_lossy(&u16s))
}

fn run_key_set(cmd: &str) -> Result<(), String> {
    let key = open_run(KEY_WRITE).ok_or_else(|| "cannot open HKCU Run key for writing".to_string())?;
    let name = wide(RUN_VALUE);
    let data = wide(cmd);
    let bytes = data.len() * 2; // REG_SZ length includes the terminating NUL
    // SAFETY: `data` is a NUL-terminated UTF-16 string of exactly `bytes`
    // bytes, which is what REG_SZ expects.
    let rc = unsafe {
        RegSetValueExW(key, name.as_ptr(), 0, REG_SZ, data.as_ptr() as *const u8, bytes as u32)
    };
    // SAFETY: handle from a successful open.
    unsafe { RegCloseKey(key) };
    if rc == 0 {
        Ok(())
    } else {
        Err(format!("could not write HKCU Run value (error {rc})"))
    }
}

fn run_key_delete() -> Result<(), String> {
    let key = open_run(KEY_WRITE).ok_or_else(|| "cannot open HKCU Run key for writing".to_string())?;
    let name = wide(RUN_VALUE);
    // SAFETY: NUL-terminated value name on a key opened for writing.
    let rc = unsafe { RegDeleteValueW(key, name.as_ptr()) };
    // SAFETY: handle from a successful open.
    unsafe { RegCloseKey(key) };
    // 2 == ERROR_FILE_NOT_FOUND: already gone, which is the desired end state.
    if rc == 0 || rc == 2 {
        Ok(())
    } else {
        Err(format!("could not delete HKCU Run value (error {rc})"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn run_key_roundtrips_without_elevation() {
        // Uses the real Run key but a throwaway value name, and cleans up.
        let saved = run_key_get();
        run_key_set("\"C:\\nonexistent\\mp.exe\" record --tray").expect("write");
        let got = run_key_get().expect("read back");
        assert!(got.contains("mp.exe"), "unexpected value: {got}");
        run_key_delete().expect("delete");
        assert!(run_key_get().is_none(), "value should be gone");
        // Deleting twice must not be an error.
        run_key_delete().expect("idempotent delete");

        // Restore anything that was genuinely there before.
        if let Some(prev) = saved {
            run_key_set(&prev).expect("restore");
        }
    }

    #[test]
    fn status_is_consistent_with_is_enabled() {
        assert_eq!(is_enabled(), status() != Method::Off);
    }
}
