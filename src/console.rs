//! Console attachment for the CLI subcommands.
//!
//! The release binary is built for the `windows` subsystem so that launching it
//! normally does not flash a console -- spec section 56. That also means the
//! process can start with no standard handles at all, so `println!` from a CLI
//! subcommand goes nowhere.
//!
//! `attach()` reattaches to the console that launched us, if there is one.
//!
//! The subtlety that matters: **a redirected handle must be left alone.** When
//! the user runs `HumanTelemetry.exe list > out.txt`, or a script captures the
//! output through a pipe, the shell hands the process a perfectly good stdout
//! that is not a console. Unconditionally repointing stdout at `CONOUT$` --
//! which an earlier version of this did -- sends the output to a console nobody
//! is reading, and the file or pipe comes back empty. So each handle is only
//! replaced when it is genuinely missing.

use windows_sys::Win32::Foundation::{HANDLE, INVALID_HANDLE_VALUE};
use windows_sys::Win32::System::Console::{
    AttachConsole, GetStdHandle, SetStdHandle, ATTACH_PARENT_PROCESS, STD_ERROR_HANDLE,
    STD_OUTPUT_HANDLE,
};

pub fn attach() {
    // SAFETY: no preconditions. Fails harmlessly when launched from Explorer,
    // or when this process already has a console.
    unsafe { AttachConsole(ATTACH_PARENT_PROCESS) };

    // Done regardless of whether the attach succeeded: the process may already
    // have had a console (debug builds), and the handles still need filling in
    // if the subsystem left them empty.
    ensure(STD_OUTPUT_HANDLE);
    ensure(STD_ERROR_HANDLE);
}

/// True when the handle is one the process can actually write to.
fn usable(h: HANDLE) -> bool {
    !h.is_null() && h != INVALID_HANDLE_VALUE
}

/// Give `slot` a console handle, but only if it does not already have a
/// working one -- see the module note on redirection.
fn ensure(slot: u32) {
    // SAFETY: plain query; returns null or INVALID_HANDLE_VALUE when unset.
    let current = unsafe { GetStdHandle(slot) };
    if usable(current) {
        return; // redirected to a file or pipe, or already a console: leave it
    }

    use std::os::windows::io::AsRawHandle;
    // std opens the console device perfectly well; going through CreateFileW
    // here would drag in the Win32_Security feature just to pass a null
    // SECURITY_ATTRIBUTES pointer.
    let Ok(f) = std::fs::OpenOptions::new().read(true).write(true).open("CONOUT$") else {
        return;
    };
    // SAFETY: a valid console handle. `forget` keeps it open for the life of
    // the process -- closing it would invalidate the std handle just set.
    unsafe { SetStdHandle(slot, f.as_raw_handle() as _) };
    std::mem::forget(f);
}
