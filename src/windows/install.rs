//! Install the recorder to local disk.
//!
//! WHY THIS EXISTS
//! ---------------
//! The project folder lives on a Google Drive virtual filesystem, and startup
//! was registered to run the executable straight out of it. That silently does
//! not work: Windows fires `HKCU\...\Run` entries almost immediately at logon,
//! while a cloud filesystem mounts some seconds later. The path does not exist
//! yet, the launch fails, nothing is logged, and nothing retries. The symptom
//! is a correct-looking Run entry, a real executable, and a recorder that
//! simply never starts -- which cost a week of recording here before anyone
//! noticed.
//!
//! The fix is not to retry or delay; it is to not depend on a late-mounting
//! drive at all. A copy of the executable lives on local disk, which is always
//! present before anything runs at logon, and startup points at that.

use std::path::{Path, PathBuf};

/// Where the running copy lives. Beside the data, under `%LOCALAPPDATA%`.
pub fn install_dir() -> PathBuf {
    let base = std::env::var("LOCALAPPDATA")
        .map(PathBuf::from)
        .unwrap_or_else(|_| std::env::temp_dir());
    base.join("RecordMouseFULL-HumanTelemetry-AIO").join("bin")
}

pub fn installed_exe() -> PathBuf {
    install_dir().join("HumanTelemetry.exe")
}

/// Are we already running from the installed location?
pub fn running_installed() -> bool {
    match std::env::current_exe() {
        Ok(me) => same_file(&me, &installed_exe()),
        Err(_) => false,
    }
}

fn same_file(a: &Path, b: &Path) -> bool {
    match (a.canonicalize(), b.canonicalize()) {
        (Ok(x), Ok(y)) => x == y,
        _ => a == b,
    }
}

/// Copy this executable to local disk if it is not already there, and return
/// the path that startup should point at.
///
/// Safe to run while an older copy is executing: Windows refuses to overwrite
/// a running image but *will* rename it, so the old one is moved aside first
/// and deleted on the next run.
pub fn ensure_installed() -> Result<PathBuf, String> {
    let src = std::env::current_exe().map_err(|e| format!("cannot locate own exe: {e}"))?;
    let dst = installed_exe();

    if same_file(&src, &dst) {
        return Ok(dst); // already the installed copy
    }

    let dir = install_dir();
    std::fs::create_dir_all(&dir).map_err(|e| format!("cannot create {}: {e}", dir.display()))?;

    // Sweep up a previous rename-aside, if that copy has since exited.
    let stale = dst.with_extension("exe.old");
    let _ = std::fs::remove_file(&stale);

    if dst.exists() {
        // A running image cannot be overwritten, but it can be renamed.
        if let Err(e) = std::fs::rename(&dst, &stale) {
            return Err(format!(
                "cannot replace {}: {e}\n\
                 Exit the running copy from the tray and try again.",
                dst.display()
            ));
        }
    }

    std::fs::copy(&src, &dst)
        .map_err(|e| format!("cannot copy to {}: {e}", dst.display()))?;
    Ok(dst)
}

/// Remove the installed copy. Startup is unregistered separately.
pub fn uninstall() -> Result<(), String> {
    let dst = installed_exe();
    let _ = std::fs::remove_file(dst.with_extension("exe.old"));
    if !dst.exists() {
        return Ok(());
    }
    if running_installed() {
        return Err(
            "the installed copy is the one running; exit it from the tray first".to_string()
        );
    }
    std::fs::remove_file(&dst).map_err(|e| format!("cannot remove {}: {e}", dst.display()))?;
    let _ = std::fs::remove_dir(install_dir());
    Ok(())
}
