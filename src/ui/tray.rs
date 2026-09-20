//! System tray icon.
//!
//! The icon is drawn in code rather than shipped as an `.ico` resource, which
//! keeps the build to a single `cargo build` with no resource compiler.
//!
//! Two deliberate constraints on the artwork, because this code cannot be
//! visually verified on a headless run:
//!
//! * The glyph is **vertically symmetric about its centre column**, and the
//!   scroll wheel sits above centre. `CreateIcon` bitmap row order is famously
//!   ambiguous, so the shape is chosen to stay recognisable either way up.
//! * The colours are **green** and **grey**, whose byte patterns are identical
//!   in BGRA and RGBA. If the channel order is the opposite of what is assumed,
//!   the icon still renders the intended colour.

use windows_sys::Win32::Foundation::HWND;
use windows_sys::Win32::UI::Shell::{
    Shell_NotifyIconW, NIF_ICON, NIF_MESSAGE, NIF_TIP, NIM_ADD, NIM_DELETE, NIM_MODIFY,
    NOTIFYICONDATAW,
};
use windows_sys::Win32::UI::WindowsAndMessaging::{CreateIcon, DestroyIcon, HICON};

/// Private message the tray sends to our window on mouse interaction.
pub const WM_TRAY: u32 = windows_sys::Win32::UI::WindowsAndMessaging::WM_APP + 1;
const ICON_ID: u32 = 1;

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum TrayState {
    Recording,
    Paused,
    Error,
}

impl TrayState {
    /// RGB triple. Green and grey survive a BGRA/RGBA mix-up unchanged; the
    /// error amber does not, and would read as a different hue, which is still
    /// unambiguously "not the normal state".
    fn rgb(self) -> (u8, u8, u8) {
        match self {
            TrayState::Recording => (0, 200, 0),
            TrayState::Paused => (128, 128, 128),
            TrayState::Error => (255, 140, 0),
        }
    }

    fn tip(self) -> &'static str {
        match self {
            TrayState::Recording => "HumanTelemetry - Recording",
            TrayState::Paused => "HumanTelemetry - Paused",
            TrayState::Error => "HumanTelemetry - Error (see window)",
        }
    }
}

pub struct Tray {
    hwnd: HWND,
    icon: HICON,
    state: TrayState,
    added: bool,
}

impl Tray {
    pub fn new(hwnd: HWND, state: TrayState) -> Self {
        let icon = make_icon(state);
        let mut t = Self { hwnd, icon, state, added: false };
        t.send(NIM_ADD);
        t.added = true;
        t
    }

    pub fn set_state(&mut self, state: TrayState) {
        if state == self.state {
            return;
        }
        let old = self.icon;
        self.state = state;
        self.icon = make_icon(state);
        self.send(NIM_MODIFY);
        // SAFETY: `old` is no longer referenced by the shell after NIM_MODIFY.
        unsafe { DestroyIcon(old) };
    }

    fn data(&self) -> NOTIFYICONDATAW {
        // SAFETY: zeroed is the documented starting state; required fields are
        // filled in below.
        let mut d: NOTIFYICONDATAW = unsafe { std::mem::zeroed() };
        d.cbSize = std::mem::size_of::<NOTIFYICONDATAW>() as u32;
        d.hWnd = self.hwnd;
        d.uID = ICON_ID;
        d.uFlags = NIF_ICON | NIF_MESSAGE | NIF_TIP;
        d.uCallbackMessage = WM_TRAY;
        d.hIcon = self.icon;
        let tip: Vec<u16> = self.state.tip().encode_utf16().collect();
        let n = tip.len().min(d.szTip.len() - 1);
        d.szTip[..n].copy_from_slice(&tip[..n]);
        d
    }

    fn send(&self, msg: u32) {
        let mut d = self.data();
        // SAFETY: correctly sized NOTIFYICONDATAW for a window we own.
        unsafe { Shell_NotifyIconW(msg, &mut d) };
    }
}

impl Drop for Tray {
    fn drop(&mut self) {
        if self.added {
            self.send(NIM_DELETE);
        }
        // SAFETY: icon created by us and no longer registered with the shell.
        unsafe { DestroyIcon(self.icon) };
    }
}

/// Build a 16x16 icon: a mouse silhouette in the state colour.
///
/// The glyph is drawn rather than loaded so the build needs no resource
/// compiler for the tray. (The *executable* icon does use a real `.ico`; see
/// `build.rs`.)
fn make_icon(state: TrayState) -> HICON {
    const N: usize = 16;
    let (r, g, b) = state.rgb();

    // AND mask all zero: the alpha channel in the XOR bits does the masking.
    let and_mask = [0u8; N * N / 8];
    let mut xor = vec![0u8; N * N * 4];

    let mut put = |buf: &mut [u8], x: usize, y: usize, c: (u8, u8, u8)| {
        let i = (y * N + x) * 4;
        // Byte order assumed BGRA; see the module note on colour choice.
        buf[i] = c.2;
        buf[i + 1] = c.1;
        buf[i + 2] = c.0;
        buf[i + 3] = 255;
    };

    let body = (r, g, b);
    let edge = (20u8, 24u8, 28u8);
    let wheel = (255u8, 255u8, 255u8);

    let cx = (N as f32 - 1.0) / 2.0;
    let cy = (N as f32 - 1.0) / 2.0;
    let rx = N as f32 * 0.30;
    let ry = N as f32 * 0.44;
    let split = cy - N as f32 * 0.10;

    for y in 0..N {
        for x in 0..N {
            let nx = (x as f32 - cx) / rx;
            let ny = (y as f32 - cy) / ry;
            let d = nx * nx + ny * ny;
            if d <= 1.0 {
                put(&mut xor, x, y, if d > 0.62 { edge } else { body });
            }
        }
    }

    // Horizontal split between the buttons, and the divider above it.
    let sy = split.round() as usize;
    let vx = cx.round() as usize;
    for x in 0..N {
        let nx = (x as f32 - cx) / rx;
        let ny = (sy as f32 - cy) / ry;
        if nx * nx + ny * ny <= 1.0 {
            put(&mut xor, x, sy, edge);
        }
    }
    for y in 0..sy {
        let nx = (vx as f32 - cx) / rx;
        let ny = (y as f32 - cy) / ry;
        if nx * nx + ny * ny <= 1.0 {
            put(&mut xor, vx, y, edge);
        }
    }

    // Scroll wheel.
    let wy0 = (cy - N as f32 * 0.30).round() as usize;
    let wy1 = (cy - N as f32 * 0.16).round() as usize;
    for y in wy0..=wy1.min(N - 1) {
        put(&mut xor, vx, y, wheel);
    }

    // SAFETY: both buffers are exactly the sizes implied by 16x16, 1 plane,
    // 32 bits per pixel (colour) and 1 bit per pixel (mask).
    unsafe {
        CreateIcon(
            std::ptr::null_mut(),
            N as i32,
            N as i32,
            1,
            32,
            and_mask.as_ptr(),
            xor.as_ptr(),
        )
    }
}
