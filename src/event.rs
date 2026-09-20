//! The capture record.
//!
//! Fixed-size and `Copy` so the hot path can memcpy one into the ring with no
//! allocation and no branching on length. Compression happens later, on the
//! writer thread, where stalling is harmless.
//!
//! WHAT IS DELIBERATELY *NOT* IN HERE
//! ---------------------------------
//! The spec asks for monitor id, foreground process, window title and modifier
//! state on every event. Filling those per-event means calling
//! `GetForegroundWindow` + `OpenProcess` + `MonitorFromPoint` inside the
//! `WM_INPUT` handler -- several syscalls per event at 1 kHz, which is exactly
//! the "never block the input handler" rule the whole project rests on. Those
//! fields are instead recorded as sideband context records on the same QPC
//! timeline whenever they *change* (see `storage::context`), and re-joined to
//! events by timestamp interval during analysis. Nothing is lost; the hot path
//! stays a memcpy.

/// Relative pointer motion reported by the device.
pub const EV_MOVE: u8 = 1;
/// Button transition (down or up -- see `F_DOWN`).
pub const EV_BUTTON: u8 = 2;
/// Wheel detent (vertical, or horizontal when `F_HWHEEL` is set).
pub const EV_WHEEL: u8 = 3;
/// Periodic absolute-position sample emitted by the capture thread's timer, so
/// integrated raw deltas can be re-anchored against true cursor position during
/// analysis. Not a device event.
pub const EV_SYNC: u8 = 4;

// ---- flags bitfield -------------------------------------------------------
/// Button transition direction: set = press, clear = release.
pub const F_DOWN: u8 = 1 << 0;
/// Device reported absolute coordinates (tablet / RDP / remote session) rather
/// than relative deltas. `dx`/`dy` then carry normalised absolute values.
pub const F_ABSOLUTE: u8 = 1 << 1;
/// Absolute coordinates are relative to the whole virtual desktop.
pub const F_VIRTUALDESK: u8 = 1 << 2;
/// Horizontal wheel rather than vertical.
pub const F_HWHEEL: u8 = 1 << 3;
/// Device attributes changed mid-stream (DPI switch, profile change).
pub const F_ATTRCHANGED: u8 = 1 << 4;
/// Device asked not to coalesce this movement.
pub const F_NOCOALESCE: u8 = 1 << 5;
/// Cursor position could not be read for this event; x/y are stale.
pub const F_POS_STALE: u8 = 1 << 6;

// ---- button ids -----------------------------------------------------------
pub const B_LEFT: u8 = 0;
pub const B_RIGHT: u8 = 1;
pub const B_MIDDLE: u8 = 2;
pub const B_X1: u8 = 3;
pub const B_X2: u8 = 4;
pub const B_NONE: u8 = 0xFF;

// ---- modifier bitmask (sampled on button/wheel events only) ---------------
pub const M_CTRL: u8 = 1 << 0;
pub const M_SHIFT: u8 = 1 << 1;
pub const M_ALT: u8 = 1 << 2;
pub const M_WIN: u8 = 1 << 3;

pub const EVENT_SIZE: usize = 32;

/// 32 bytes. At 1 kHz that is 32 KB/s raw, ~115 MB/hour before compression;
/// delta-coded by zstd it lands around 4-10 MB/hour.
///
/// `dx`/`dy` are `i32`, not `i16`, on purpose. `RAWMOUSE.lLastX/lLastY` are
/// `LONG`. A 25600-DPI sensor flicked at ~5 m/s on a 125 Hz connection reports
/// roughly 40000 counts in a single report, which overflows `i16` and would
/// silently corrupt the very fastest movements -- the ones most characteristic
/// of an individual user. The extra 4 bytes compress away to nearly nothing.
#[repr(C)]
#[derive(Clone, Copy, Default, Debug, PartialEq)]
pub struct Event {
    /// ns since session start (QPC), taken at `WM_INPUT` handler entry.
    pub t_ns: u64,
    pub kind: u8,
    pub flags: u8,
    /// Index into the session header's device table.
    pub dev: u8,
    /// Button id for `EV_BUTTON`, else `B_NONE`.
    pub btn: u8,
    /// Raw device delta in mouse counts -- pre-ballistics, the actual motor
    /// signal.
    pub dx: i32,
    pub dy: i32,
    /// Cursor position in virtual-desktop pixels -- post-ballistics, post-clip.
    ///
    /// CAVEAT, and it matters: this is `GetCursorPos` at *handler* time, not at
    /// event time. Under load the two drift apart. It is recorded because it is
    /// cheap and still the best available observation of the post-ballistics
    /// layer, but analysis must treat it as an approximation and prefer
    /// `EV_SYNC` anchors plus integrated dx/dy for fine timing.
    pub x: i32,
    pub y: i32,
    /// Wheel detents * 120 (WHEEL_DELTA), signed. `EV_WHEEL` only.
    pub wheel: i16,
    /// Modifier bitmask. Sampled only on `EV_BUTTON`/`EV_WHEEL`.
    ///
    /// Not sampled on `EV_MOVE`: that would be 4 `GetAsyncKeyState` calls per
    /// event at 1 kHz for information that only ever changes meaning at a
    /// click. Sampling at the click is what section 40 of the spec actually
    /// wants (ctrl-click, shift-click) at ~1/s instead of ~1000/s. Reading
    /// modifiers via a keyboard hook was rejected outright: that is a keylogger
    /// surface, and this recorder must never become one.
    pub mods: u8,
    /// Bitmask of buttons currently held, by `B_*` bit position. Maintained
    /// incrementally by the capture thread, so drags are reconstructible
    /// without scanning backwards.
    pub bstate: u8,
}

const _: () = assert!(core::mem::size_of::<Event>() == EVENT_SIZE);

impl Event {
    #[inline(always)]
    pub fn to_bytes(&self) -> [u8; EVENT_SIZE] {
        let mut b = [0u8; EVENT_SIZE];
        b[0..8].copy_from_slice(&self.t_ns.to_le_bytes());
        b[8] = self.kind;
        b[9] = self.flags;
        b[10] = self.dev;
        b[11] = self.btn;
        b[12..16].copy_from_slice(&self.dx.to_le_bytes());
        b[16..20].copy_from_slice(&self.dy.to_le_bytes());
        b[20..24].copy_from_slice(&self.x.to_le_bytes());
        b[24..28].copy_from_slice(&self.y.to_le_bytes());
        b[28..30].copy_from_slice(&self.wheel.to_le_bytes());
        b[30] = self.mods;
        b[31] = self.bstate;
        b
    }

    pub fn from_bytes(b: &[u8]) -> Self {
        debug_assert!(b.len() >= EVENT_SIZE);
        Self {
            t_ns: u64::from_le_bytes(b[0..8].try_into().unwrap()),
            kind: b[8],
            flags: b[9],
            dev: b[10],
            btn: b[11],
            dx: i32::from_le_bytes(b[12..16].try_into().unwrap()),
            dy: i32::from_le_bytes(b[16..20].try_into().unwrap()),
            x: i32::from_le_bytes(b[20..24].try_into().unwrap()),
            y: i32::from_le_bytes(b[24..28].try_into().unwrap()),
            wheel: i16::from_le_bytes(b[28..30].try_into().unwrap()),
            mods: b[30],
            bstate: b[31],
        }
    }

    pub fn kind_name(&self) -> &'static str {
        match self.kind {
            EV_MOVE => "move",
            EV_BUTTON => "button",
            EV_WHEEL => "wheel",
            EV_SYNC => "sync",
            _ => "?",
        }
    }
}

pub fn button_name(b: u8) -> &'static str {
    match b {
        B_LEFT => "left",
        B_RIGHT => "right",
        B_MIDDLE => "middle",
        B_X1 => "x1",
        B_X2 => "x2",
        _ => "none",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip_preserves_every_field() {
        let e = Event {
            t_ns: 0x0123_4567_89AB_CDEF,
            kind: EV_BUTTON,
            flags: F_DOWN | F_NOCOALESCE,
            dev: 3,
            btn: B_X2,
            dx: -40_000,
            dy: 40_000,
            x: -1920,
            y: 2160,
            wheel: -240,
            mods: M_CTRL | M_SHIFT,
            bstate: 0b0000_0101,
        };
        assert_eq!(Event::from_bytes(&e.to_bytes()), e);
    }

    #[test]
    fn dx_holds_a_high_dpi_flick_that_would_overflow_i16() {
        // 25600 DPI, ~5 m/s, 125 Hz poll -> ~40k counts in one report.
        let e = Event { dx: 40_315, dy: -40_315, ..Default::default() };
        assert_eq!(Event::from_bytes(&e.to_bytes()).dx, 40_315);
    }
}
