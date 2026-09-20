//! The capture thread: Raw Input registration, message loop, event decode.
//!
//! This is the one place in the program where latency matters. The handler
//! does exactly four things -- stamp the clock, decode the report, read the
//! cursor, push into the ring -- and returns. No allocation, no locking, no
//! I/O, no analysis.
//!
//! WINDOW TYPE
//! -----------
//! A hidden top-level window, not a message-only (`HWND_MESSAGE`) one. Raw
//! Input works with either, but message-only windows do not receive broadcast
//! messages, and `WM_DISPLAYCHANGE` is a broadcast. Losing monitor-topology
//! changes would silently corrupt every coordinate recorded after the user
//! unplugged a screen. `WS_EX_TOOLWINDOW` keeps it out of alt-tab; it is never
//! shown.
//!
//! WHY NOT `GetRawInputBuffer`
//! --------------------------
//! Reading reports in batches is cheaper per event, but every event in a batch
//! is then stamped with one handler-entry time, destroying the inter-event
//! intervals that this entire project exists to measure. Per-message
//! `GetRawInputData` costs more CPU and buys exact arrival times. At 1 kHz the
//! cost is not measurable; the timing loss would be permanent.

use crate::clock::Clock;
use crate::context::{foreground, monitors, ContextRecord, ContextSender};
use crate::event::*;
use crate::input::devices::{DeviceTable, USAGE_MOUSE, USAGE_PAGE_GENERIC};
use crate::ring::Ring;

use std::cell::Cell;
use std::sync::atomic::{AtomicBool, AtomicIsize, AtomicU64, Ordering};
use std::sync::Arc;

use windows_sys::Win32::Foundation::{HWND, LPARAM, LRESULT, POINT, WPARAM};
use windows_sys::Win32::System::LibraryLoader::GetModuleHandleW;
use windows_sys::Win32::System::Power::RegisterPowerSettingNotification;
use windows_sys::Win32::System::SystemServices::GUID_CONSOLE_DISPLAY_STATE;
use windows_sys::Win32::System::Threading::{
    GetCurrentThread, SetThreadPriority, THREAD_PRIORITY_TIME_CRITICAL,
};
use windows_sys::Win32::UI::Input::KeyboardAndMouse::{
    GetAsyncKeyState, VK_CONTROL, VK_LWIN, VK_MENU, VK_RWIN, VK_SHIFT,
};
use windows_sys::Win32::UI::Input::{
    GetRawInputData, RegisterRawInputDevices, HRAWINPUT, RAWINPUT, RAWINPUTDEVICE, RAWINPUTHEADER,
    RIDEV_DEVNOTIFY, RIDEV_INPUTSINK, RID_INPUT, RIM_TYPEMOUSE,
};
use windows_sys::Win32::UI::WindowsAndMessaging::*;

/// How often the capture thread emits an absolute-position anchor.
const SYNC_TIMER_ID: usize = 1;
const SYNC_INTERVAL_MS: u32 = 200;

// ---- RAWMOUSE bitfields ---------------------------------------------------
// Values from the Windows SDK (winuser.h). Defined here rather than imported
// because they live in different modules across windows-sys versions; the
// values themselves are part of the stable Win32 ABI.
const RI_LEFT_DOWN: u16 = 0x0001;
const RI_LEFT_UP: u16 = 0x0002;
const RI_RIGHT_DOWN: u16 = 0x0004;
const RI_RIGHT_UP: u16 = 0x0008;
const RI_MIDDLE_DOWN: u16 = 0x0010;
const RI_MIDDLE_UP: u16 = 0x0020;
const RI_BTN4_DOWN: u16 = 0x0040;
const RI_BTN4_UP: u16 = 0x0080;
const RI_BTN5_DOWN: u16 = 0x0100;
const RI_BTN5_UP: u16 = 0x0200;
const RI_WHEEL: u16 = 0x0400;
const RI_HWHEEL: u16 = 0x0800;

const MOUSE_MOVE_ABSOLUTE_F: u16 = 0x01;
const MOUSE_VIRTUAL_DESKTOP_F: u16 = 0x02;
const MOUSE_ATTRIBUTES_CHANGED_F: u16 = 0x04;
const MOUSE_MOVE_NOCOALESCE_F: u16 = 0x08;

/// Table mapping a transition flag to (button id, is_down).
const TRANSITIONS: [(u16, u8, bool); 10] = [
    (RI_LEFT_DOWN, B_LEFT, true),
    (RI_LEFT_UP, B_LEFT, false),
    (RI_RIGHT_DOWN, B_RIGHT, true),
    (RI_RIGHT_UP, B_RIGHT, false),
    (RI_MIDDLE_DOWN, B_MIDDLE, true),
    (RI_MIDDLE_UP, B_MIDDLE, false),
    (RI_BTN4_DOWN, B_X1, true),
    (RI_BTN4_UP, B_X1, false),
    (RI_BTN5_DOWN, B_X2, true),
    (RI_BTN5_UP, B_X2, false),
];

/// Shared, observable state of the capture thread.
pub struct CaptureState {
    pub paused: AtomicBool,
    /// Count of `WM_INPUT` *reports* received from the device.
    ///
    /// Deliberately not called "events": one report can yield several events
    /// (a move plus two button transitions plus a wheel detent all arrive in
    /// one report), and `EV_SYNC` anchors are events with no report behind
    /// them at all. Conflating the two makes `written > received` look like
    /// corruption when it is the normal, correct case.
    pub reports: AtomicU64,
    /// Window handle, published once the thread has created it. 0 until then.
    hwnd: AtomicIsize,
}

impl CaptureState {
    pub fn new() -> Self {
        Self {
            paused: AtomicBool::new(false),
            reports: AtomicU64::new(0),
            hwnd: AtomicIsize::new(0),
        }
    }
}

impl Default for CaptureState {
    fn default() -> Self {
        Self::new()
    }
}

/// Everything the window procedure touches. Owned solely by the capture
/// thread; `WM_INPUT`, `WM_INPUT_DEVICE_CHANGE` and the foreground hook all
/// arrive on this same thread, so no synchronisation is needed for `devices`
/// or `bstate`.
struct Ctx {
    clock: Clock,
    ring: Arc<Ring>,
    ctx_tx: ContextSender,
    state: Arc<CaptureState>,
    devices: DeviceTable,
    /// Bitmask of currently-held buttons, maintained across reports.
    bstate: u8,
    /// Last known cursor position, reused when `GetCursorPos` fails.
    last_pos: POINT,
    /// Scratch buffer for `GetRawInputData`, so the hot path never allocates.
    scratch: Vec<u8>,
}

thread_local! {
    /// Set once, before the message loop starts, and cleared after it ends.
    static CTX: Cell<*mut Ctx> = const { Cell::new(std::ptr::null_mut()) };
}

#[inline(always)]
fn ctx() -> *mut Ctx {
    CTX.with(|c| c.get())
}

pub struct CaptureHandle {
    pub state: Arc<CaptureState>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl CaptureHandle {
    /// Ask the capture thread to finish and wait for it.
    pub fn stop(mut self) {
        let hwnd = self.state.hwnd.load(Ordering::Acquire);
        if hwnd != 0 {
            // SAFETY: posting to a window owned by the capture thread; the
            // message is queued, not executed here.
            unsafe { PostMessageW(hwnd as HWND, WM_CLOSE, 0, 0) };
        }
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }

    pub fn set_paused(&self, paused: bool) {
        self.state.paused.store(paused, Ordering::Relaxed);
    }
}

/// Start capturing. Returns once the thread is running and registered.
pub fn spawn(clock: Clock, ring: Arc<Ring>, ctx_tx: ContextSender) -> std::io::Result<CaptureHandle> {
    let state = Arc::new(CaptureState::new());
    let st = Arc::clone(&state);
    let (ready_tx, ready_rx) = std::sync::mpsc::channel::<Result<(), String>>();

    let thread = std::thread::Builder::new()
        .name("capture".into())
        .spawn(move || run(clock, ring, ctx_tx, st, ready_tx))?;

    match ready_rx.recv() {
        Ok(Ok(())) => Ok(CaptureHandle { state, thread: Some(thread) }),
        Ok(Err(e)) => Err(std::io::Error::other(e)),
        Err(_) => Err(std::io::Error::other("capture thread died during startup")),
    }
}

fn run(
    clock: Clock,
    ring: Arc<Ring>,
    ctx_tx: ContextSender,
    state: Arc<CaptureState>,
    ready: std::sync::mpsc::Sender<Result<(), String>>,
) {
    // The thread spends essentially all its time blocked in GetMessageW. Raising
    // it to TIME_CRITICAL costs nothing at idle and means a busy machine cannot
    // delay the one handler whose timing is the product.
    // SAFETY: both calls act on the current thread and cannot fail meaningfully.
    unsafe { SetThreadPriority(GetCurrentThread(), THREAD_PRIORITY_TIME_CRITICAL) };

    let mut boxed = Box::new(Ctx {
        clock,
        ring,
        ctx_tx,
        state: Arc::clone(&state),
        devices: DeviceTable::new(),
        bstate: 0,
        last_pos: POINT { x: 0, y: 0 },
        scratch: vec![0u8; 1024],
    });
    CTX.with(|c| c.set(boxed.as_mut() as *mut Ctx));

    let hwnd = match create_window() {
        Ok(h) => h,
        Err(e) => {
            let _ = ready.send(Err(e));
            CTX.with(|c| c.set(std::ptr::null_mut()));
            return;
        }
    };

    if let Err(e) = register_raw_input(hwnd) {
        // SAFETY: window we just created and still own.
        unsafe { DestroyWindow(hwnd) };
        let _ = ready.send(Err(e));
        CTX.with(|c| c.set(std::ptr::null_mut()));
        return;
    }

    // SAFETY: valid HWND; failure here is non-fatal (we simply get no
    // display-state notifications), so the result is ignored deliberately.
    unsafe {
        RegisterPowerSettingNotification(
            hwnd as _,
            &GUID_CONSOLE_DISPLAY_STATE,
            DEVICE_NOTIFY_WINDOW_HANDLE,
        );
        SetTimer(hwnd, SYNC_TIMER_ID, SYNC_INTERVAL_MS, None);
    }

    // Foreground tracking. The closure runs on this thread and does nothing
    // but stamp and forward -- resolution happens on the writer.
    let hook = unsafe {
        foreground::install_hook(move |hw| {
            let c = ctx();
            if c.is_null() {
                return;
            }
            let c = &mut *c;
            let _ = c.ctx_tx.send(ContextRecord::Foreground {
                t_ns: c.clock.now_ns(),
                hwnd: hw as isize,
                pid: None,
                exe: None,
                title: None,
            });
        })
    };

    // Initial snapshots, so the timeline starts with known context.
    {
        // SAFETY: ctx pointer was just published on this thread.
        let c = unsafe { &mut *ctx() };
        let t = c.clock.now_ns();
        let _ = c.ctx_tx.send(ContextRecord::DeviceChange {
            t_ns: t,
            devices: c.devices.devices().to_vec(),
        });
        let _ = c.ctx_tx.send(ContextRecord::DisplayChange {
            t_ns: t,
            monitors: monitors::enumerate(),
        });
        let _ = c.ctx_tx.send(ContextRecord::Foreground {
            t_ns: t,
            hwnd: foreground::current() as isize,
            pid: None,
            exe: None,
            title: None,
        });
    }

    state.hwnd.store(hwnd as isize, Ordering::Release);
    let _ = ready.send(Ok(()));

    // SAFETY: standard message pump. GetMessageW returns 0 on WM_QUIT, -1 on
    // error; both end the loop.
    unsafe {
        let mut msg: MSG = std::mem::zeroed();
        loop {
            let r = GetMessageW(&mut msg, std::ptr::null_mut(), 0, 0);
            if r == 0 || r == -1 {
                break;
            }
            TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
        KillTimer(hwnd, SYNC_TIMER_ID);
        foreground::remove_hook(hook);
    }

    state.hwnd.store(0, Ordering::Release);
    CTX.with(|c| c.set(std::ptr::null_mut()));
    drop(boxed);
}

fn create_window() -> Result<HWND, String> {
    let class: Vec<u16> = "MouseProfilerCapture\0".encode_utf16().collect();
    // SAFETY: GetModuleHandleW(null) returns this module; never fails.
    let hinst = unsafe { GetModuleHandleW(std::ptr::null()) };

    // SAFETY: zeroed WNDCLASSW with the required fields filled in.
    let mut wc: WNDCLASSW = unsafe { std::mem::zeroed() };
    wc.lpfnWndProc = Some(wndproc);
    wc.hInstance = hinst;
    wc.lpszClassName = class.as_ptr();
    // SAFETY: `wc` is fully initialised; a duplicate registration is tolerated.
    unsafe { RegisterClassW(&wc) };

    // SAFETY: registered class, no parent. Not shown: no WS_VISIBLE and no
    // ShowWindow call, so it never appears on screen.
    let hwnd = unsafe {
        CreateWindowExW(
            WS_EX_TOOLWINDOW,
            class.as_ptr(),
            class.as_ptr(),
            WS_OVERLAPPED,
            0,
            0,
            0,
            0,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            hinst,
            std::ptr::null(),
        )
    };
    if hwnd.is_null() {
        return Err("CreateWindowExW failed".into());
    }
    Ok(hwnd)
}

fn register_raw_input(hwnd: HWND) -> Result<(), String> {
    let rid = RAWINPUTDEVICE {
        usUsagePage: USAGE_PAGE_GENERIC,
        usUsage: USAGE_MOUSE,
        // INPUTSINK: keep receiving input while unfocused -- mandatory, the
        // recorder is never the foreground window.
        // DEVNOTIFY: deliver WM_INPUT_DEVICE_CHANGE on hotplug.
        dwFlags: RIDEV_INPUTSINK | RIDEV_DEVNOTIFY,
        hwndTarget: hwnd,
    };
    // SAFETY: one correctly-sized RAWINPUTDEVICE.
    let ok = unsafe {
        RegisterRawInputDevices(&rid, 1, std::mem::size_of::<RAWINPUTDEVICE>() as u32)
    };
    if ok == 0 {
        return Err("RegisterRawInputDevices failed".into());
    }
    Ok(())
}

unsafe extern "system" fn wndproc(hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    match msg {
        WM_INPUT => {
            let c = ctx();
            if !c.is_null() {
                on_input(&mut *c, lp as HRAWINPUT);
            }
            // Required: the system needs DefWindowProc to release the buffer.
            DefWindowProcW(hwnd, msg, wp, lp)
        }
        WM_INPUT_DEVICE_CHANGE => {
            let c = ctx();
            if !c.is_null() {
                let c = &mut *c;
                if c.devices.refresh() {
                    let _ = c.ctx_tx.send(ContextRecord::DeviceChange {
                        t_ns: c.clock.now_ns(),
                        devices: c.devices.devices().to_vec(),
                    });
                }
            }
            0
        }
        WM_DISPLAYCHANGE => {
            let c = ctx();
            if !c.is_null() {
                let c = &mut *c;
                let _ = c.ctx_tx.send(ContextRecord::DisplayChange {
                    t_ns: c.clock.now_ns(),
                    monitors: monitors::enumerate(),
                });
            }
            0
        }
        WM_POWERBROADCAST => {
            let c = ctx();
            if !c.is_null() {
                let c = &mut *c;
                let name = match wp as u32 {
                    PBT_APMSUSPEND => Some("suspend"),
                    PBT_APMRESUMEAUTOMATIC => Some("resume_automatic"),
                    PBT_APMRESUMESUSPEND => Some("resume_user"),
                    PBT_POWERSETTINGCHANGE => Some("display_state_change"),
                    _ => None,
                };
                if let Some(n) = name {
                    let _ = c.ctx_tx.send(ContextRecord::Power {
                        t_ns: c.clock.now_ns(),
                        event: n.into(),
                    });
                }
            }
            1 // TRUE: grant the request
        }
        WM_TIMER if wp == SYNC_TIMER_ID => {
            let c = ctx();
            if !c.is_null() {
                on_sync(&mut *c);
            }
            0
        }
        WM_CLOSE => {
            DestroyWindow(hwnd);
            0
        }
        WM_DESTROY => {
            PostQuitMessage(0);
            0
        }
        _ => DefWindowProcW(hwnd, msg, wp, lp),
    }
}

/// The hot path.
#[inline]
unsafe fn on_input(c: &mut Ctx, h: HRAWINPUT) {
    // Clock first, before any other work, so the stamp is as close to arrival
    // as this thread can observe.
    let t_ns = c.clock.now_ns();

    let mut size: u32 = c.scratch.len() as u32;
    let n = GetRawInputData(
        h,
        RID_INPUT,
        c.scratch.as_mut_ptr() as *mut _,
        &mut size,
        std::mem::size_of::<RAWINPUTHEADER>() as u32,
    );
    if n == u32::MAX || n == 0 {
        return;
    }

    let raw = &*(c.scratch.as_ptr() as *const RAWINPUT);
    if raw.header.dwType != RIM_TYPEMOUSE {
        return;
    }

    c.state.reports.fetch_add(1, Ordering::Relaxed);
    if c.state.paused.load(Ordering::Relaxed) {
        return;
    }

    let m = &raw.data.mouse;
    let dev = c.devices.index_of(raw.header.hDevice);

    // One cursor read per report, shared by every event it produces.
    let mut pos = POINT { x: 0, y: 0 };
    let pos_ok = GetCursorPos(&mut pos) != 0;
    if pos_ok {
        c.last_pos = pos;
    } else {
        pos = c.last_pos;
    }
    let stale = if pos_ok { 0 } else { F_POS_STALE };

    let mut base_flags = stale;
    if m.usFlags & MOUSE_MOVE_ABSOLUTE_F != 0 {
        base_flags |= F_ABSOLUTE;
    }
    if m.usFlags & MOUSE_VIRTUAL_DESKTOP_F != 0 {
        base_flags |= F_VIRTUALDESK;
    }
    if m.usFlags & MOUSE_ATTRIBUTES_CHANGED_F != 0 {
        base_flags |= F_ATTRCHANGED;
    }
    if m.usFlags & MOUSE_MOVE_NOCOALESCE_F != 0 {
        base_flags |= F_NOCOALESCE;
    }

    let button_flags = m.Anonymous.Anonymous.usButtonFlags;
    let button_data = m.Anonymous.Anonymous.usButtonData as i16;

    // --- movement ---------------------------------------------------------
    // A report with no displacement and no transitions still happens (some
    // devices poll idle); skipping it keeps the log honest about real motion.
    if m.lLastX != 0 || m.lLastY != 0 || (base_flags & F_ABSOLUTE) != 0 {
        c.ring.push(Event {
            t_ns,
            kind: EV_MOVE,
            flags: base_flags,
            dev,
            btn: B_NONE,
            dx: m.lLastX,
            dy: m.lLastY,
            x: pos.x,
            y: pos.y,
            wheel: 0,
            mods: 0,
            bstate: c.bstate,
        });
    }

    if button_flags == 0 {
        return;
    }

    // --- button transitions ----------------------------------------------
    // Sample modifiers at most once per report, and only when something was
    // actually pressed -- see the note on `Event::mods`.
    let mut mods = 0u8;
    let mut mods_read = false;

    for (bit, btn, down) in TRANSITIONS {
        if button_flags & bit == 0 {
            continue;
        }
        if !mods_read {
            mods = read_modifiers();
            mods_read = true;
        }
        if down {
            c.bstate |= 1 << btn;
        } else {
            c.bstate &= !(1 << btn);
        }
        c.ring.push(Event {
            t_ns,
            kind: EV_BUTTON,
            flags: base_flags | if down { F_DOWN } else { 0 },
            dev,
            btn,
            dx: 0,
            dy: 0,
            x: pos.x,
            y: pos.y,
            wheel: 0,
            mods,
            bstate: c.bstate,
        });
    }

    // --- wheel ------------------------------------------------------------
    if button_flags & (RI_WHEEL | RI_HWHEEL) != 0 {
        if !mods_read {
            mods = read_modifiers();
        }
        let horizontal = button_flags & RI_HWHEEL != 0;
        c.ring.push(Event {
            t_ns,
            kind: EV_WHEEL,
            flags: base_flags | if horizontal { F_HWHEEL } else { 0 },
            dev,
            btn: B_NONE,
            dx: 0,
            dy: 0,
            x: pos.x,
            y: pos.y,
            wheel: button_data,
            mods,
            bstate: c.bstate,
        });
    }
}

/// Periodic absolute anchor, so integrated deltas can be re-registered against
/// truth during analysis.
unsafe fn on_sync(c: &mut Ctx) {
    if c.state.paused.load(Ordering::Relaxed) {
        return;
    }
    let t_ns = c.clock.now_ns();
    let mut pos = POINT { x: 0, y: 0 };
    let ok = GetCursorPos(&mut pos) != 0;
    if ok {
        c.last_pos = pos;
    } else {
        pos = c.last_pos;
    }
    c.ring.push(Event {
        t_ns,
        kind: EV_SYNC,
        flags: if ok { 0 } else { F_POS_STALE },
        dev: 0,
        btn: B_NONE,
        dx: 0,
        dy: 0,
        x: pos.x,
        y: pos.y,
        wheel: 0,
        mods: 0,
        bstate: c.bstate,
    });
}

/// Modifier snapshot. Four `GetAsyncKeyState` calls, which read a cached
/// user-mode table -- cheap, but still only done at clicks and wheel events.
///
/// Note this reads modifier *state*, never key identity: no keystroke is
/// observed, logged or inferred.
#[inline]
unsafe fn read_modifiers() -> u8 {
    let mut m = 0u8;
    // High bit set = currently down.
    if GetAsyncKeyState(VK_CONTROL as i32) < 0 {
        m |= M_CTRL;
    }
    if GetAsyncKeyState(VK_SHIFT as i32) < 0 {
        m |= M_SHIFT;
    }
    if GetAsyncKeyState(VK_MENU as i32) < 0 {
        m |= M_ALT;
    }
    if GetAsyncKeyState(VK_LWIN as i32) < 0 || GetAsyncKeyState(VK_RWIN as i32) < 0 {
        m |= M_WIN;
    }
    m
}
