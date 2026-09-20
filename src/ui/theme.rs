//! Dark theme: colours, brushes and fonts.
//!
//! Win32 has no theming API that makes a plain dialog dark. The three pieces
//! that actually work, and are used here:
//!
//! * The **title bar** via `DwmSetWindowAttribute(DWMWA_USE_IMMERSIVE_DARK_MODE)`,
//!   documented since Windows 10 2004. Without it a dark window keeps a white
//!   caption and looks broken.
//! * **Static text** via `WM_CTLCOLORSTATIC`, where the parent returns a
//!   background brush and sets the text colour.
//! * **Buttons** via `BS_OWNERDRAW` + `WM_DRAWITEM`. Push buttons ignore
//!   `WM_CTLCOLORBTN` entirely -- the theme engine draws them -- so the only
//!   way to get a dark button that is not a grey slab is to paint it.
//!
//! The undocumented `uxtheme` dark-mode ordinals are deliberately not used:
//! they are unexported by ordinal, change between Windows builds, and break
//! silently.

use windows_sys::Win32::Foundation::{COLORREF, HWND};
use windows_sys::Win32::Graphics::Dwm::DwmSetWindowAttribute;
use windows_sys::Win32::Graphics::Gdi::{
    CreateFontW, CreatePen, CreateSolidBrush, DeleteObject, ANTIALIASED_QUALITY,
    CLIP_DEFAULT_PRECIS, DEFAULT_CHARSET, FF_DONTCARE, FIXED_PITCH, HBRUSH, HFONT, HPEN,
    OUT_DEFAULT_PRECIS, PS_SOLID, VARIABLE_PITCH,
};

/// `DWMWA_USE_IMMERSIVE_DARK_MODE`. Value 20 on Windows 10 2004+ and 11.
const DWMWA_USE_IMMERSIVE_DARK_MODE: u32 = 20;

#[inline]
pub const fn rgb(r: u8, g: u8, b: u8) -> COLORREF {
    (r as u32) | ((g as u32) << 8) | ((b as u32) << 16)
}

// ---- palette --------------------------------------------------------------
pub const BG: COLORREF = rgb(24, 24, 27);
pub const SEPARATOR: COLORREF = rgb(44, 44, 51);
/// Section headings.
pub const ACCENT: COLORREF = rgb(96, 165, 250);
/// Row labels: present but recessive.
pub const LABEL: COLORREF = rgb(138, 140, 150);
/// The numbers themselves.
pub const VALUE: COLORREF = rgb(232, 234, 238);
/// Secondary detail on a value row.
pub const DIM: COLORREF = rgb(120, 122, 132);

pub const OK: COLORREF = rgb(74, 222, 128);
pub const WARN: COLORREF = rgb(250, 204, 21);
pub const BAD: COLORREF = rgb(248, 113, 113);
pub const IDLE: COLORREF = rgb(148, 163, 184);

/// Checkbox: unticked box interior, and the tick itself.
pub const CHECK_EMPTY: COLORREF = rgb(32, 32, 38);
pub const CHECK_BORDER: COLORREF = rgb(88, 90, 100);
pub const CHECK_MARK: COLORREF = rgb(255, 255, 255);

pub const BTN_FACE: COLORREF = rgb(39, 39, 46);
pub const BTN_FACE_DOWN: COLORREF = rgb(58, 58, 68);
pub const BTN_BORDER: COLORREF = rgb(66, 66, 76);
pub const BTN_TEXT: COLORREF = rgb(228, 230, 235);

pub struct Theme {
    pub bg: HBRUSH,
    pub btn_face: HBRUSH,
    pub btn_face_down: HBRUSH,
    pub btn_border: HBRUSH,
    /// Filled box of a ticked checkbox.
    pub accent: HBRUSH,
    /// Interior of an unticked checkbox.
    pub check_empty: HBRUSH,
    pub check_border: HBRUSH,
    /// Two strokes of the tick. 2px so it reads at this size.
    pub check_pen: HPEN,
    /// UI text.
    pub font: HFONT,
    /// Section headings: smaller, semibold.
    pub font_head: HFONT,
    /// The status line.
    pub font_big: HFONT,
    /// Values. Monospaced so digits line up down the column -- the main reason
    /// the old layout read as noise was that nothing aligned.
    pub font_mono: HFONT,
}

fn font(size: i32, weight: i32, face: &str, fixed: bool) -> HFONT {
    let name: Vec<u16> = face.encode_utf16().chain(std::iter::once(0)).collect();
    let pitch = if fixed { FIXED_PITCH } else { VARIABLE_PITCH };
    // SAFETY: plain GDI call; `name` is NUL-terminated and outlives it.
    unsafe {
        CreateFontW(
            size,
            0,
            0,
            0,
            weight,
            0,
            0,
            0,
            DEFAULT_CHARSET as u32,
            OUT_DEFAULT_PRECIS as u32,
            CLIP_DEFAULT_PRECIS as u32,
            ANTIALIASED_QUALITY as u32,
            (pitch | FF_DONTCARE) as u32,
            name.as_ptr(),
        )
    }
}

impl Theme {
    pub fn new() -> Self {
        // SAFETY: colour literals; each brush is released in `destroy`.
        unsafe {
            Self {
                bg: CreateSolidBrush(BG),
                btn_face: CreateSolidBrush(BTN_FACE),
                btn_face_down: CreateSolidBrush(BTN_FACE_DOWN),
                btn_border: CreateSolidBrush(BTN_BORDER),
                accent: CreateSolidBrush(ACCENT),
                check_empty: CreateSolidBrush(CHECK_EMPTY),
                check_border: CreateSolidBrush(CHECK_BORDER),
                check_pen: CreatePen(PS_SOLID, 2, CHECK_MARK),
                font: font(-13, 400, "Segoe UI", false),
                font_head: font(-12, 600, "Segoe UI", false),
                font_big: font(-22, 600, "Segoe UI", false),
                font_mono: font(-13, 400, "Consolas", true),
            }
        }
    }

    /// # Safety
    /// Call once, after every window using these objects is destroyed.
    pub unsafe fn destroy(&self) {
        for o in [
            self.bg,
            self.btn_face,
            self.btn_face_down,
            self.btn_border,
            self.accent,
            self.check_empty,
            self.check_border,
        ] {
            DeleteObject(o as _);
        }
        DeleteObject(self.check_pen as _);
        for f in [self.font, self.font_head, self.font_big, self.font_mono] {
            DeleteObject(f as _);
        }
    }
}

/// Ask DWM for a dark caption. Harmless on builds that predate the attribute:
/// it returns a failure HRESULT and the caption simply stays light.
///
/// # Safety
/// `hwnd` must be a valid top-level window.
pub unsafe fn dark_titlebar(hwnd: HWND) {
    let on: i32 = 1;
    DwmSetWindowAttribute(
        hwnd,
        DWMWA_USE_IMMERSIVE_DARK_MODE,
        &on as *const i32 as *const _,
        std::mem::size_of::<i32>() as u32,
    );
}
