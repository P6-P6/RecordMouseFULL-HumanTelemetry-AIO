//! Foreground-window tracking.
//!
//! Two halves, deliberately split across threads:
//!
//! * `install_hook` runs on the capture thread. A `WINEVENT_OUTOFCONTEXT` hook
//!   delivers `EVENT_SYSTEM_FOREGROUND` through that thread's message loop, so
//!   the timestamp is taken on the same clock as the mouse events. The callback
//!   does nothing but stamp the time and forward the raw HWND.
//!
//! * `resolve` runs on the writer thread and turns that HWND into a pid, an
//!   executable name and (optionally) a window title. These calls can block --
//!   `GetWindowTextW` sends `WM_GETTEXT` to the target, which hangs if the
//!   target is hung -- so they must never happen on the capture thread.

use windows_sys::Win32::Foundation::{CloseHandle, HWND, MAX_PATH};
use windows_sys::Win32::System::Threading::{
    OpenProcess, QueryFullProcessImageNameW, PROCESS_QUERY_LIMITED_INFORMATION,
};
use windows_sys::Win32::UI::Accessibility::{SetWinEventHook, UnhookWinEvent, HWINEVENTHOOK};
use windows_sys::Win32::UI::WindowsAndMessaging::{
    GetForegroundWindow, GetWindowTextLengthW, GetWindowTextW, GetWindowThreadProcessId,
    EVENT_SYSTEM_FOREGROUND, WINEVENT_OUTOFCONTEXT, WINEVENT_SKIPOWNPROCESS,
};

/// Set by `install_hook` before the hook can fire. Only the capture thread
/// touches this, and only through the two functions below.
static mut SINK: Option<Box<dyn Fn(HWND)>> = None;

/// Install the foreground hook on the *calling* thread.
///
/// The callback fires on this thread's message loop, so the caller must be
/// pumping messages. `sink` should do the minimum possible -- stamp a time,
/// queue the HWND -- and return.
///
/// # Safety
/// Must be called from the capture thread, exactly once, before its message
/// loop starts.
pub unsafe fn install_hook<F: Fn(HWND) + 'static>(sink: F) -> HWINEVENTHOOK {
    SINK = Some(Box::new(sink));
    SetWinEventHook(
        EVENT_SYSTEM_FOREGROUND,
        EVENT_SYSTEM_FOREGROUND,
        std::ptr::null_mut(),
        Some(proc),
        0, // all processes
        0, // all threads
        // OUTOFCONTEXT keeps our code out of other processes entirely -- the
        // events are marshalled to this thread instead of injecting a DLL.
        // SKIPOWNPROCESS avoids a feedback loop from our own windows.
        WINEVENT_OUTOFCONTEXT | WINEVENT_SKIPOWNPROCESS,
    )
}

pub unsafe fn remove_hook(h: HWINEVENTHOOK) {
    if !h.is_null() {
        UnhookWinEvent(h);
    }
    SINK = None;
}

unsafe extern "system" fn proc(
    _hook: HWINEVENTHOOK,
    event: u32,
    hwnd: HWND,
    id_object: i32,
    id_child: i32,
    _thread: u32,
    _time: u32,
) {
    // OBJID_WINDOW / CHILDID_SELF only: ignore the flood of child-object events.
    const OBJID_WINDOW: i32 = 0;
    const CHILDID_SELF: i32 = 0;
    if event != EVENT_SYSTEM_FOREGROUND
        || id_object != OBJID_WINDOW
        || id_child != CHILDID_SELF
        || hwnd.is_null()
    {
        return;
    }
    if let Some(sink) = &*std::ptr::addr_of!(SINK) {
        sink(hwnd);
    }
}

/// Current foreground window, for the initial snapshot at session start.
pub fn current() -> HWND {
    // SAFETY: no arguments, no preconditions.
    unsafe { GetForegroundWindow() }
}

/// Resolve an HWND to (pid, exe name, title).
///
/// Call from the writer thread only -- see the module docs. `want_title` is the
/// user-facing privacy switch from spec section 47; when false no title is read
/// at all, rather than read-then-discarded.
pub fn resolve(hwnd: HWND, want_title: bool) -> (Option<u32>, Option<String>, Option<String>) {
    if hwnd.is_null() {
        return (None, None, None);
    }
    let mut pid: u32 = 0;
    // SAFETY: valid out-pointer; returns the thread id, which we ignore.
    unsafe { GetWindowThreadProcessId(hwnd, &mut pid) };
    let pid_opt = if pid == 0 { None } else { Some(pid) };

    let exe = pid_opt.and_then(process_image_name);
    let title = if want_title { window_title(hwnd) } else { None };
    (pid_opt, exe, title)
}

fn process_image_name(pid: u32) -> Option<String> {
    // QUERY_LIMITED_INFORMATION is the least privilege that still works against
    // processes at a higher integrity level, so elevated apps still resolve.
    // SAFETY: plain call; the handle is closed on every path below.
    let h = unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid) };
    if h.is_null() {
        return None;
    }
    let mut buf = [0u16; MAX_PATH as usize];
    let mut len = buf.len() as u32;
    // SAFETY: `buf` holds `len` WCHARs; `len` is updated to the written count.
    let ok = unsafe { QueryFullProcessImageNameW(h, 0, buf.as_mut_ptr(), &mut len) };
    // SAFETY: `h` came from a successful OpenProcess and is not used after.
    unsafe { CloseHandle(h) };
    if ok == 0 {
        return None;
    }
    let full = String::from_utf16_lossy(&buf[..len as usize]);
    // Store just the executable name: the full path leaks the user profile
    // directory into the dataset for no analytical benefit.
    Some(full.rsplit(['\\', '/']).next().unwrap_or(&full).to_string())
}

fn window_title(hwnd: HWND) -> Option<String> {
    // SAFETY: valid HWND (null-checked by the caller).
    let len = unsafe { GetWindowTextLengthW(hwnd) };
    if len <= 0 {
        return None;
    }
    let mut buf = vec![0u16; len as usize + 1];
    // SAFETY: buffer has room for `len` chars plus the NUL.
    let n = unsafe { GetWindowTextW(hwnd, buf.as_mut_ptr(), buf.len() as i32) };
    if n <= 0 {
        return None;
    }
    Some(String::from_utf16_lossy(&buf[..n as usize]))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolving_a_null_window_yields_nothing_rather_than_panicking() {
        assert_eq!(resolve(std::ptr::null_mut(), true).0, None);
    }

    #[test]
    fn resolves_our_own_process_to_an_exe_name() {
        let pid = std::process::id();
        let name = process_image_name(pid).expect("own process must resolve");
        assert!(name.ends_with(".exe"), "unexpected image name: {name}");
        assert!(!name.contains('\\'), "path should be stripped: {name}");
    }

    #[test]
    fn title_capture_can_be_refused() {
        // With want_title = false no title is produced even for a real window.
        let hwnd = current();
        if !hwnd.is_null() {
            assert_eq!(resolve(hwnd, false).2, None);
        }
    }
}
