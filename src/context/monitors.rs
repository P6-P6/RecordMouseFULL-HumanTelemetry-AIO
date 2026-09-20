//! Monitor topology and DPI.
//!
//! Every event stores virtual-desktop coordinates. Turning those back into
//! "which screen, how far from which edge" during analysis needs the layout as
//! it was *at the time*, which is why a fresh snapshot is recorded on every
//! `WM_DISPLAYCHANGE` rather than once at startup.

use serde::{Deserialize, Serialize};
use windows_sys::Win32::Foundation::{BOOL, LPARAM, RECT, TRUE};
use windows_sys::Win32::Graphics::Gdi::{
    EnumDisplayMonitors, GetMonitorInfoW, HDC, HMONITOR, MONITORINFOEXW,
};

/// `MONITORINFOF_PRIMARY` from winuser.h. Defined here because windows-sys
/// moves it between modules across versions; the value is stable Win32 ABI.
const MONITORINFOF_PRIMARY: u32 = 0x0000_0001;
use windows_sys::Win32::UI::HiDpi::{GetDpiForMonitor, MDT_EFFECTIVE_DPI};

#[derive(Clone, Debug, Serialize, Deserialize, Default, PartialEq)]
pub struct MonitorInfo {
    /// Stable-ish index in enumeration order. Use `device` for identity.
    pub index: u32,
    /// GDI device name, e.g. `\\.\DISPLAY1`.
    pub device: String,
    /// Full monitor rectangle in virtual-desktop coordinates.
    pub left: i32,
    pub top: i32,
    pub right: i32,
    pub bottom: i32,
    /// Work area -- the monitor rect minus taskbar and appbars. Needed for the
    /// edge/corner behaviour in spec section 33: the cursor stops at the
    /// taskbar, not at the screen edge.
    pub work_left: i32,
    pub work_top: i32,
    pub work_right: i32,
    pub work_bottom: i32,
    pub dpi_x: u32,
    pub dpi_y: u32,
    /// dpi / 96. 1.0 = 100% scaling, 1.5 = 150%.
    pub scaling: f32,
    pub primary: bool,
}

impl MonitorInfo {
    pub fn width(&self) -> i32 {
        self.right - self.left
    }
    pub fn height(&self) -> i32 {
        self.bottom - self.top
    }
    pub fn contains(&self, x: i32, y: i32) -> bool {
        x >= self.left && x < self.right && y >= self.top && y < self.bottom
    }
}

/// Snapshot the current monitor layout.
pub fn enumerate() -> Vec<MonitorInfo> {
    let mut out: Vec<MonitorInfo> = Vec::new();
    // SAFETY: passing a pointer to our Vec as the callback's LPARAM; the Vec
    // outlives the synchronous enumeration.
    unsafe {
        EnumDisplayMonitors(
            std::ptr::null_mut(),
            std::ptr::null(),
            Some(cb),
            &mut out as *mut Vec<MonitorInfo> as LPARAM,
        );
    }
    for (i, m) in out.iter_mut().enumerate() {
        m.index = i as u32;
    }
    out
}

/// `MONITORENUMPROC`. Called synchronously, once per monitor.
unsafe extern "system" fn cb(hmon: HMONITOR, _hdc: HDC, _rc: *mut RECT, data: LPARAM) -> BOOL {
    let out = &mut *(data as *mut Vec<MonitorInfo>);

    let mut mi: MONITORINFOEXW = std::mem::zeroed();
    mi.monitorInfo.cbSize = std::mem::size_of::<MONITORINFOEXW>() as u32;
    if GetMonitorInfoW(hmon, &mut mi as *mut _ as *mut _) == 0 {
        return TRUE; // skip this one, keep enumerating
    }

    let mut dpi_x = 96u32;
    let mut dpi_y = 96u32;
    // Returns non-zero HRESULT on failure, in which case we keep the 96 default
    // rather than reporting a bogus scale factor.
    GetDpiForMonitor(hmon, MDT_EFFECTIVE_DPI, &mut dpi_x, &mut dpi_y);

    let end = mi.szDevice.iter().position(|&c| c == 0).unwrap_or(mi.szDevice.len());
    let r = mi.monitorInfo.rcMonitor;
    let w = mi.monitorInfo.rcWork;

    out.push(MonitorInfo {
        index: 0,
        device: String::from_utf16_lossy(&mi.szDevice[..end]),
        left: r.left,
        top: r.top,
        right: r.right,
        bottom: r.bottom,
        work_left: w.left,
        work_top: w.top,
        work_right: w.right,
        work_bottom: w.bottom,
        dpi_x,
        dpi_y,
        scaling: dpi_x as f32 / 96.0,
        primary: mi.monitorInfo.dwFlags & MONITORINFOF_PRIMARY != 0,
    });
    TRUE
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn finds_at_least_one_monitor_with_a_sane_rect() {
        let ms = enumerate();
        assert!(!ms.is_empty(), "no monitors enumerated");
        for m in &ms {
            assert!(m.width() > 0 && m.height() > 0, "degenerate rect: {:?}", m);
            assert!(m.dpi_x >= 48, "implausible dpi: {}", m.dpi_x);
            // Work area must sit inside the monitor rect.
            assert!(m.work_left >= m.left && m.work_right <= m.right);
        }
        assert_eq!(ms.iter().filter(|m| m.primary).count(), 1, "expected exactly one primary");
    }
}
