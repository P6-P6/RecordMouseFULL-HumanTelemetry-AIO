//! The dashboard window and the application message loop.
//!
//! Plain Win32 rather than egui/iced, and not to save disk space. This process
//! is meant to sit running for weeks: an immediate-mode GUI repaints on a
//! continuous loop and keeps a GPU surface alive to do it. These controls
//! repaint only when a value changes, and the refresh timer is stopped
//! outright while the window is hidden, so a minimised recorder costs nothing.
//!
//! LAYOUT
//! ------
//! Rows are grouped under CAPTURE / HEALTH / HARDWARE / SESSION headings, and
//! every value sits in one monospaced column. The earlier version put a
//! 27-character session id and a 90-character folder path in the same type
//! size as "Dropped: 0", so nothing stood out; here the numbers that matter
//! are bright and aligned, paths are dim, and `Dropped` turns red the moment
//! it is non-zero.
//!
//! Closing the window hides it to the tray; only Exit on the tray menu ends
//! recording. That is the usual contract for a background recorder, and it
//! stops an accidental click on the X from silently ending a long capture.

use crate::input::devices::DeviceInfo;
use crate::input::raw_input::CaptureHandle;
use crate::ring::Ring;
use crate::storage::writer::{SharedStatus, WriterHandle};
use crate::ui::theme::{self, Theme};
use crate::ui::tray::{Tray, TrayState, WM_TRAY};
use crate::windows::startup;

use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::Instant;

use windows_sys::Win32::Foundation::{COLORREF, HWND, LPARAM, LRESULT, POINT, RECT, WPARAM};
use windows_sys::Win32::Graphics::Gdi::{
    BeginPaint, CreateSolidBrush, DeleteObject, DrawTextW, EndPaint, FillRect, InvalidateRect,
    LineTo, MoveToEx, SelectObject, SetBkMode, SetTextColor, UpdateWindow, DT_LEFT, DT_SINGLELINE,
    DT_VCENTER, HDC, PAINTSTRUCT, TRANSPARENT,
};
use windows_sys::Win32::System::LibraryLoader::GetModuleHandleW;
use windows_sys::Win32::UI::Controls::{ODS_SELECTED, DRAWITEMSTRUCT};
use windows_sys::Win32::UI::Shell::ShellExecuteW;
use windows_sys::Win32::UI::WindowsAndMessaging::*;

// ---- control ids ----------------------------------------------------------
const ID_PAUSE: usize = 1001;
const ID_NEWSESSION: usize = 1002;
const ID_FOLDER: usize = 1003;
const ID_EXPORT: usize = 1004;
const ID_STARTUP: usize = 1005;
const ID_HIDE: usize = 1006;
const ID_RAW: usize = 1007;

// Static-control id ranges, so WM_CTLCOLORSTATIC can pick a colour by role.
const ID_STATUS: usize = 3000;
const ID_ELAPSED: usize = 3001;
const ID_HEAD_BASE: usize = 3100;
const ID_LABEL_BASE: usize = 3200;
const ID_VALUE_BASE: usize = 3300;
const ID_HINT: usize = 3400;

// ---- tray menu ids --------------------------------------------------------
const MENU_SHOW: usize = 2001;
const MENU_PAUSE: usize = 2002;
const MENU_NEWSESSION: usize = 2003;
const MENU_FOLDER: usize = 2004;
const MENU_STARTUP: usize = 2005;
const MENU_RAW: usize = 2006;
const MENU_EXIT: usize = 2099;

const TIMER_REFRESH: usize = 1;
const REFRESH_MS: u32 = 500;

// ---- value slots ----------------------------------------------------------
const V_REPORTS: usize = 0;
const V_EVENTS: usize = 1;
const V_DROPPED: usize = 2;
const V_RATE: usize = 3;
const V_QUEUE: usize = 4;
const V_LATENCY: usize = 5;
const V_MOUSE: usize = 6;
const V_OTHER: usize = 7;
const V_MACHINE: usize = 8;
const V_STARTED: usize = 9;
const V_ONDISK: usize = 10;
const V_FOLDER: usize = 11;
const V_COUNT: usize = 12;

// ---- geometry -------------------------------------------------------------
const W: i32 = 560;
const MARGIN: i32 = 24;
const LABEL_X: i32 = 24;
const LABEL_W: i32 = 132;
const VALUE_X: i32 = 164;
const VALUE_W: i32 = 372;
const ROW_H: i32 = 23;

/// Horizontal rules, drawn in WM_PAINT. Filled in during layout.
struct Rule(i32);

pub struct App {
    pub capture: CaptureHandle,
    pub writer: Option<WriterHandle>,
    pub status: Arc<SharedStatus>,
    pub ring: Arc<Ring>,
    pub devices: Vec<DeviceInfo>,
    /// "PCNAME / user" -- which machine produced this data. Resolved once at
    /// startup; it cannot change while the process runs.
    machine: String,

    theme: Theme,
    labels: [HWND; V_COUNT],
    status_label: HWND,
    elapsed_label: HWND,
    btn_pause: HWND,
    btn_startup: HWND,
    rules: Vec<Rule>,

    tray: Option<Tray>,
    started: Instant,
    last_reports: u64,
    last_tick: Instant,
    rate_hz: f64,
    rate_peak: f64,
    rate_sum: f64,
    rate_samples: u64,
    /// Drives the colour of the status line and the Dropped row.
    state: TrayState,
    dropped_now: u64,
    startup_on: bool,

    raw_hwnd: HWND,
    raw_edit: HWND,
    visible: bool,
    shutting_down: bool,
}

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

fn commas(n: u64) -> String {
    let s = n.to_string();
    let mut out = String::with_capacity(s.len() + s.len() / 3);
    for (i, c) in s.chars().enumerate() {
        if i > 0 && (s.len() - i) % 3 == 0 {
            out.push(',');
        }
        out.push(c);
    }
    out
}

fn human_bytes(b: u64) -> String {
    const U: [&str; 4] = ["B", "KB", "MB", "GB"];
    let mut v = b as f64;
    let mut i = 0;
    while v >= 1024.0 && i < U.len() - 1 {
        v /= 1024.0;
        i += 1;
    }
    if i == 0 {
        format!("{b} B")
    } else {
        format!("{v:.1} {}", U[i])
    }
}

/// Run the GUI. Returns when the user exits; consumes the handles so the
/// recorder is always shut down in order on the way out.
pub fn run(
    capture: CaptureHandle,
    writer: WriterHandle,
    status: Arc<SharedStatus>,
    ring: Arc<Ring>,
    devices: Vec<DeviceInfo>,
    start_hidden: bool,
) {
    // SAFETY: GetModuleHandleW(null) returns this module and cannot fail.
    let hinst = unsafe { GetModuleHandleW(std::ptr::null()) };
    let theme = Theme::new();
    let class = wide("HumanTelemetryWindow");

    // SAFETY: zeroed WNDCLASSW with the required fields populated below.
    let mut wc: WNDCLASSW = unsafe { std::mem::zeroed() };
    wc.lpfnWndProc = Some(wndproc);
    wc.hInstance = hinst;
    wc.lpszClassName = class.as_ptr();
    // SAFETY: standard cursor lookup; the class brush is our dark background.
    unsafe {
        wc.hCursor = LoadCursorW(std::ptr::null_mut(), IDC_ARROW);
        wc.hbrBackground = theme.bg;
        RegisterClassW(&wc);
    }

    let style = WS_OVERLAPPED | WS_CAPTION | WS_SYSMENU | WS_MINIMIZEBOX;
    // Size the *client* area, then grow for the frame, so the layout constants
    // mean what they say regardless of DPI or caption height.
    let mut rc = RECT { left: 0, top: 0, right: W, bottom: 664 };
    // SAFETY: valid rect and style.
    unsafe { AdjustWindowRect(&mut rc, style, 0) };

    let title = wide("HumanTelemetry");
    // SAFETY: registered class; no parent.
    let hwnd = unsafe {
        CreateWindowExW(
            0,
            class.as_ptr(),
            title.as_ptr(),
            style,
            CW_USEDEFAULT,
            CW_USEDEFAULT,
            rc.right - rc.left,
            rc.bottom - rc.top,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            hinst,
            std::ptr::null(),
        )
    };
    if hwnd.is_null() {
        eprintln!("could not create window");
        // SAFETY: nothing else references the theme objects yet.
        unsafe { theme.destroy() };
        return;
    }

    let mut app = Box::new(App {
        capture,
        writer: Some(writer),
        status,
        ring,
        devices,
        machine: {
            let os = crate::storage::session::OsInfo::probe();
            if os.user_name.is_empty() {
                os.computer_name
            } else {
                format!("{} / {}", os.computer_name, os.user_name)
            }
        },
        theme,
        labels: [std::ptr::null_mut(); V_COUNT],
        status_label: std::ptr::null_mut(),
        elapsed_label: std::ptr::null_mut(),
        btn_pause: std::ptr::null_mut(),
        btn_startup: std::ptr::null_mut(),
        rules: Vec::new(),
        tray: None,
        started: Instant::now(),
        last_reports: 0,
        last_tick: Instant::now(),
        rate_hz: 0.0,
        rate_peak: 0.0,
        rate_sum: 0.0,
        rate_samples: 0,
        state: TrayState::Recording,
        dropped_now: 0,
        startup_on: startup::is_enabled(),
        raw_hwnd: std::ptr::null_mut(),
        raw_edit: std::ptr::null_mut(),
        visible: !start_hidden,
        shutting_down: false,
    });

    // SAFETY: the Box outlives the window; it is reclaimed after the loop ends.
    unsafe {
        SetWindowLongPtrW(hwnd, GWLP_USERDATA, app.as_mut() as *mut App as isize);
        theme::dark_titlebar(hwnd);
        build_controls(hwnd, &mut app, hinst);
        app.tray = Some(Tray::new(hwnd, TrayState::Recording));
        if app.visible {
            // ShowWindow twice, deliberately.
            //
            // A process's *first* ShowWindow call ignores the nCmdShow it is
            // given and uses the value the launching process put in
            // STARTUPINFO instead. So a parent that starts us hidden or
            // minimised -- a shortcut set to "minimized", Task Scheduler, some
            // launchers, PowerShell's Start-Process -- leaves the dashboard
            // invisible no matter what we pass, and the recorder looks like it
            // failed to start. The second call honours the parameter.
            ShowWindow(hwnd, SW_SHOW);
            ShowWindow(hwnd, SW_SHOW);
            UpdateWindow(hwnd);
            SetTimer(hwnd, TIMER_REFRESH, REFRESH_MS, None);
        }
        refresh(hwnd, &mut app);

        let mut msg: MSG = std::mem::zeroed();
        loop {
            let r = GetMessageW(&mut msg, std::ptr::null_mut(), 0, 0);
            if r == 0 || r == -1 {
                break;
            }
            TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }

        // Tray icon must go before the window does.
        app.tray = None;
        app.theme.destroy();
        SetWindowLongPtrW(hwnd, GWLP_USERDATA, 0);
    }

    // Order matters: stop producing before draining, so the writer's final
    // pass sees a ring that nothing is still filling.
    app.capture.set_paused(true);
    let writer = app.writer.take();
    let App { capture, .. } = *app;
    capture.stop();
    if let Some(w) = writer {
        w.stop();
    }
}

// ---------------------------------------------------------------------------
// layout
// ---------------------------------------------------------------------------

unsafe fn mk_static(
    parent: HWND,
    hinst: windows_sys::Win32::Foundation::HMODULE,
    id: usize,
    text: &str,
    x: i32,
    y: i32,
    w: i32,
    h: i32,
    font: windows_sys::Win32::Graphics::Gdi::HFONT,
    right: bool,
) -> HWND {
    let t = wide(text);
    // SS_LEFT is 0; SS_RIGHT is 2. Declared inline rather than imported
    // because windows-sys keeps the STATIC_STYLES constants in a module that
    // moves between versions.
    let align: u32 = if right { 2 } else { 0 };
    let h = CreateWindowExW(
        0,
        wide("STATIC").as_ptr(),
        t.as_ptr(),
        WS_CHILD | WS_VISIBLE | align,
        x,
        y,
        w,
        h,
        parent,
        id as _,
        hinst,
        std::ptr::null(),
    );
    SendMessageW(h, WM_SETFONT, font as WPARAM, 1);
    h
}

unsafe fn mk_button(
    parent: HWND,
    hinst: windows_sys::Win32::Foundation::HMODULE,
    id: usize,
    text: &str,
    x: i32,
    y: i32,
    w: i32,
    font: windows_sys::Win32::Graphics::Gdi::HFONT,
) -> HWND {
    let t = wide(text);
    // BS_OWNERDRAW (0x0B): push buttons ignore WM_CTLCOLORBTN, so a dark
    // button has to be painted by us. See WM_DRAWITEM below.
    let h = CreateWindowExW(
        0,
        wide("BUTTON").as_ptr(),
        t.as_ptr(),
        WS_CHILD | WS_VISIBLE | WS_TABSTOP | 0x0B,
        x,
        y,
        w,
        30,
        parent,
        id as _,
        hinst,
        std::ptr::null(),
    );
    SendMessageW(h, WM_SETFONT, font as WPARAM, 1);
    h
}

unsafe fn build_controls(
    hwnd: HWND,
    app: &mut App,
    hinst: windows_sys::Win32::Foundation::HMODULE,
) {
    let f = app.theme.font;
    let fh = app.theme.font_head;
    let fm = app.theme.font_mono;
    let fb = app.theme.font_big;

    app.status_label =
        mk_static(hwnd, hinst, ID_STATUS, "STARTING", MARGIN, 18, 300, 30, fb, false);
    app.elapsed_label =
        mk_static(hwnd, hinst, ID_ELAPSED, "00:00:00", 336, 24, 200, 22, fm, true);

    let mut head_id = ID_HEAD_BASE;
    let mut label_id = ID_LABEL_BASE;
    let mut y = 62;

    // (heading, [(row label, value slot)])
    let sections: [(&str, &[(&str, usize)]); 4] = [
        (
            "CAPTURE",
            &[
                ("Input reports", V_REPORTS),
                ("Events stored", V_EVENTS),
                ("Dropped", V_DROPPED),
                ("Report rate", V_RATE),
            ],
        ),
        ("HEALTH", &[("Queue", V_QUEUE), ("Write latency", V_LATENCY)]),
        ("HARDWARE", &[("Active mouse", V_MOUSE), ("Also present", V_OTHER)]),
        (
            "SESSION",
            &[
                ("This PC", V_MACHINE),
                ("Started", V_STARTED),
                ("On disk", V_ONDISK),
                ("Folder", V_FOLDER),
            ],
        ),
    ];

    for (i, (head, rows)) in sections.iter().enumerate() {
        if i > 0 {
            app.rules.push(Rule(y - 14));
        }
        mk_static(hwnd, hinst, head_id, head, MARGIN, y, 300, 18, fh, false);
        head_id += 1;
        y += 24;

        for (name, slot) in rows.iter() {
            mk_static(hwnd, hinst, label_id, name, LABEL_X, y + 2, LABEL_W, 20, f, false);
            label_id += 1;
            app.labels[*slot] = mk_static(
                hwnd,
                hinst,
                ID_VALUE_BASE + slot,
                "-",
                VALUE_X,
                y + 2,
                VALUE_W,
                20,
                fm,
                false,
            );
            y += ROW_H;
        }
        y += 16;
    }

    app.rules.push(Rule(y - 6));
    let by = y + 8;
    app.btn_pause = mk_button(hwnd, hinst, ID_PAUSE, "Pause", MARGIN, by, 104, f);
    mk_button(hwnd, hinst, ID_NEWSESSION, "Save + New Session", 136, by, 156, f);
    mk_button(hwnd, hinst, ID_RAW, "Raw Events", 300, by, 110, f);
    mk_button(hwnd, hinst, ID_EXPORT, "Export CSV", 418, by, 118, f);

    app.btn_startup = mk_button(
        hwnd,
        hinst,
        ID_STARTUP,
        "Start when I sign in to Windows",
        MARGIN,
        by + 38,
        250,
        f,
    );
    mk_button(hwnd, hinst, ID_FOLDER, "Open Folder", 300, by + 38, 110, f);
    mk_button(hwnd, hinst, ID_HIDE, "Hide to Tray", 418, by + 38, 118, f);

    mk_static(
        hwnd,
        hinst,
        ID_HINT,
        "Recording saves continuously. Closing this window keeps recording in the tray;\n\
         use Exit on the tray menu to stop.",
        MARGIN,
        by + 82,
        512,
        34,
        f,
        false,
    );
}

// ---------------------------------------------------------------------------
// painting
// ---------------------------------------------------------------------------

unsafe fn on_paint(hwnd: HWND, app: &App) {
    let mut ps: PAINTSTRUCT = std::mem::zeroed();
    let hdc = BeginPaint(hwnd, &mut ps);
    let pen = CreateSolidBrush(theme::SEPARATOR);
    for Rule(y) in &app.rules {
        let r = RECT { left: MARGIN, top: *y, right: W - MARGIN, bottom: *y + 1 };
        FillRect(hdc, &r, pen);
    }
    DeleteObject(pen as _);
    EndPaint(hwnd, &ps);
}

/// Read a control's caption.
unsafe fn control_text(h: HWND) -> Vec<u16> {
    let len = GetWindowTextLengthW(h).max(0);
    let mut buf = vec![0u16; len as usize + 1];
    GetWindowTextW(h, buf.as_mut_ptr(), buf.len() as i32);
    buf
}

/// Paint one owner-drawn control: either a push button or the checkbox.
unsafe fn on_drawitem(app: &App, dis: &DRAWITEMSTRUCT) {
    if GetDlgCtrlID(dis.hwndItem) as usize == ID_STARTUP {
        draw_checkbox(app, dis);
    } else {
        draw_button(app, dis);
    }
}

unsafe fn draw_button(app: &App, dis: &DRAWITEMSTRUCT) {
    let pressed = dis.itemState & ODS_SELECTED != 0;
    let face = if pressed { app.theme.btn_face_down } else { app.theme.btn_face };

    // Border first, then the face inset by one pixel: a 1px frame without
    // needing a pen and a separate rectangle call.
    FillRect(dis.hDC, &dis.rcItem, app.theme.btn_border);
    let mut inner = RECT {
        left: dis.rcItem.left + 1,
        top: dis.rcItem.top + 1,
        right: dis.rcItem.right - 1,
        bottom: dis.rcItem.bottom - 1,
    };
    FillRect(dis.hDC, &inner, face);

    SetBkMode(dis.hDC, TRANSPARENT as i32);
    SetTextColor(dis.hDC, theme::BTN_TEXT);
    let text = control_text(dis.hwndItem);
    DrawTextW(
        dis.hDC,
        text.as_ptr(),
        -1,
        &mut inner,
        1 /* DT_CENTER */ | DT_SINGLELINE | DT_VCENTER,
    );
}

/// A real checkbox: a square that fills in and shows a tick, with the label
/// beside it -- not a push button wearing a text glyph.
///
/// Owner-drawn rather than `BS_AUTOCHECKBOX` because a stock checkbox is
/// painted by the theme engine and stays light-on-light on a dark window.
/// Drawing it means also owning the state, which lives in `App::startup_on`
/// and is re-read from the system whenever a toggle fails.
unsafe fn draw_checkbox(app: &App, dis: &DRAWITEMSTRUCT) {
    // No button face: a checkbox sits on the window, it is not a raised
    // control.
    FillRect(dis.hDC, &dis.rcItem, app.theme.bg);

    const BOX: i32 = 17;
    let top = dis.rcItem.top + (dis.rcItem.bottom - dis.rcItem.top - BOX) / 2;
    let bx = RECT {
        left: dis.rcItem.left,
        top,
        right: dis.rcItem.left + BOX,
        bottom: top + BOX,
    };

    let on = app.startup_on;
    let pressed = dis.itemState & ODS_SELECTED != 0;

    // Border, then interior inset by one pixel.
    FillRect(dis.hDC, &bx, if on { app.theme.accent } else { app.theme.check_border });
    let inner = RECT {
        left: bx.left + 1,
        top: bx.top + 1,
        right: bx.right - 1,
        bottom: bx.bottom - 1,
    };
    if on {
        FillRect(dis.hDC, &inner, app.theme.accent);
    } else {
        FillRect(
            dis.hDC,
            &inner,
            if pressed { app.theme.btn_face_down } else { app.theme.check_empty },
        );
    }

    if on {
        // The tick: two strokes, proportioned to the box rather than
        // hard-coded, so the size constant above is the only thing to change.
        let old = SelectObject(dis.hDC, app.theme.check_pen as _);
        let x = bx.left;
        let y = bx.top;
        MoveToEx(dis.hDC, x + BOX * 25 / 100, y + BOX * 52 / 100, std::ptr::null_mut());
        LineTo(dis.hDC, x + BOX * 43 / 100, y + BOX * 70 / 100);
        LineTo(dis.hDC, x + BOX * 76 / 100, y + BOX * 30 / 100);
        SelectObject(dis.hDC, old);
    }

    SetBkMode(dis.hDC, TRANSPARENT as i32);
    SetTextColor(dis.hDC, if on { theme::VALUE } else { theme::LABEL });
    let text = control_text(dis.hwndItem);
    let mut tr = RECT {
        left: bx.right + 10,
        top: dis.rcItem.top,
        right: dis.rcItem.right,
        bottom: dis.rcItem.bottom,
    };
    DrawTextW(dis.hDC, text.as_ptr(), -1, &mut tr, DT_LEFT | DT_SINGLELINE | DT_VCENTER);
}

/// Colour for a static control, chosen from its id.
fn static_color(app: &App, id: usize) -> COLORREF {
    match id {
        ID_STATUS => match app.state {
            TrayState::Recording => theme::OK,
            TrayState::Paused => theme::IDLE,
            TrayState::Error => theme::WARN,
        },
        ID_ELAPSED => theme::DIM,
        ID_HINT => theme::DIM,
        i if (ID_HEAD_BASE..ID_LABEL_BASE).contains(&i) => theme::ACCENT,
        i if (ID_LABEL_BASE..ID_VALUE_BASE).contains(&i) => theme::LABEL,
        i if i == ID_VALUE_BASE + V_DROPPED => {
            if app.dropped_now > 0 {
                theme::BAD
            } else {
                theme::VALUE
            }
        }
        // Paths are reference material, not a headline.
        i if i == ID_VALUE_BASE + V_FOLDER || i == ID_VALUE_BASE + V_OTHER => theme::DIM,
        _ => theme::VALUE,
    }
}

// ---------------------------------------------------------------------------
// refresh
// ---------------------------------------------------------------------------

unsafe fn set_text(h: HWND, s: &str) {
    if h.is_null() {
        return;
    }
    // Avoid a repaint when nothing changed: the timer fires twice a second and
    // most of these values are static most of the time.
    let len = GetWindowTextLengthW(h);
    let mut cur = vec![0u16; len as usize + 1];
    if len > 0 {
        GetWindowTextW(h, cur.as_mut_ptr(), cur.len() as i32);
    }
    let new = wide(s);
    if cur == new {
        return;
    }
    SetWindowTextW(h, new.as_ptr());
}

fn device_rows(app: &App) -> (String, String) {
    let mut parts: Vec<(u64, String)> = app
        .devices
        .iter()
        .filter(|d| !d.synthetic)
        .map(|d| {
            let n = app
                .status
                .per_device
                .get(d.index as usize)
                .map(|a| a.load(Ordering::Relaxed))
                .unwrap_or(0);
            let id = match (d.vendor_id, d.product_id) {
                (Some(v), Some(p)) => format!("{v:04X}:{p:04X}"),
                _ => "HID".to_string(),
            };
            (n, format!("{id}  {} btn", d.buttons))
        })
        .collect();
    if parts.is_empty() {
        return ("none detected".into(), "-".into());
    }
    // Busiest first: the one in your hand leads.
    parts.sort_by(|a, b| b.0.cmp(&a.0));
    let (n, ref top) = parts[0];
    let active = if n > 0 {
        format!("{top}   {} events", commas(n))
    } else {
        format!("{top}   (idle)")
    };
    let others: Vec<String> = parts[1..].iter().map(|(_, s)| s.clone()).collect();
    (active, if others.is_empty() { "-".into() } else { others.join(",  ") })
}

/// The device's true report rate, from the median gap between stored
/// movement timestamps.
///
/// WHY NOT COUNTER DELTAS
/// ----------------------
/// The obvious measure -- reports counted over a wall-clock window -- is
/// wrong in a way that flatters the hardware. If the capture thread is briefly
/// starved, its `WM_INPUT` messages queue in the OS and then arrive in a burst;
/// the counter jumps and the computed rate spikes to a number the device is
/// physically incapable of. That is how this window came to claim a peak of
/// 999 Hz for a mouse whose stored timestamps are a flat 8 ms apart.
///
/// The median inter-report gap cannot be fooled that way: a 125 Hz device
/// produces 8 ms gaps no matter how the reports are delivered to us.
fn measured_rate(app: &App) -> Option<(f64, f64)> {
    use crate::event::EV_MOVE;
    let q = app.status.recent.lock().ok()?;
    let mut gaps: Vec<f64> = Vec::new();
    let mut last: Option<u64> = None;
    for e in q.iter().filter(|e| e.kind == EV_MOVE) {
        if let Some(l) = last {
            let d = e.t_ns.saturating_sub(l);
            // Discard idle gaps: a pause between movements says nothing about
            // the polling rate. 100 ms is far longer than any real report gap.
            if d > 0 && d < 100_000_000 {
                gaps.push(d as f64 / 1e6);
            }
        }
        last = Some(e.t_ns);
    }
    if gaps.len() < 8 {
        return None;
    }
    gaps.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    let med = gaps[gaps.len() / 2];
    if med <= 0.0 {
        return None;
    }
    Some((1000.0 / med, med))
}

/// Bytes currently on disk for the active session.
fn session_bytes(app: &App) -> u64 {
    let dir = app.status.session_dir();
    if dir.as_os_str().is_empty() {
        return 0;
    }
    std::fs::read_dir(&dir)
        .map(|rd| {
            rd.filter_map(|e| e.ok())
                .filter_map(|e| e.metadata().ok())
                .map(|m| m.len())
                .sum()
        })
        .unwrap_or(0)
}

unsafe fn refresh(hwnd: HWND, app: &mut App) {
    let paused = app.capture.state.paused.load(Ordering::Relaxed);
    let reports = app.capture.state.reports.load(Ordering::Relaxed);
    let dropped = app.status.events_dropped.load(Ordering::Relaxed);
    let errors = app.status.writer_errors.load(Ordering::Relaxed);
    let events = app.status.events_written.load(Ordering::Relaxed);

    // Live report rate, which is the honest measure of the mouse's polling
    // rate -- the value the driver advertises is routinely 0 or nominal.
    let dt = app.last_tick.elapsed().as_secs_f64();
    if dt >= 0.45 {
        let d = reports.saturating_sub(app.last_reports) as f64;
        let inst = d / dt;
        app.rate_hz = if app.rate_hz == 0.0 { inst } else { app.rate_hz * 0.6 + inst * 0.4 };
        // Peak and average use the unsmoothed value, and only count windows
        // where the mouse actually moved -- averaging in the idle zeroes would
        // say more about how often the user pauses than about the hardware.
        if inst > 0.0 {
            if inst > app.rate_peak {
                app.rate_peak = inst;
            }
            app.rate_sum += inst;
            app.rate_samples += 1;
        }
        app.last_reports = reports;
        app.last_tick = Instant::now();
    }

    let new_state = if errors > 0 {
        TrayState::Error
    } else if paused {
        TrayState::Paused
    } else {
        TrayState::Recording
    };
    let state_changed = new_state as u8 as usize != app.state as u8 as usize;
    app.state = new_state;
    let dropped_changed = (dropped > 0) != (app.dropped_now > 0);
    app.dropped_now = dropped;

    if let Some(t) = app.tray.as_mut() {
        t.set_state(new_state);
    }

    set_text(
        app.status_label,
        match new_state {
            TrayState::Recording => "\u{25CF}  RECORDING",
            TrayState::Paused => "\u{25CF}  PAUSED",
            TrayState::Error => "\u{25CF}  WRITER ERROR",
        },
    );
    // A colour change needs an explicit repaint; the text may be identical.
    if state_changed {
        InvalidateRect(app.status_label, std::ptr::null(), 1);
    }
    if dropped_changed {
        InvalidateRect(app.labels[V_DROPPED], std::ptr::null(), 1);
    }

    let secs = app.started.elapsed().as_secs();
    set_text(
        app.elapsed_label,
        &format!("{:02}:{:02}:{:02}", secs / 3600, (secs / 60) % 60, secs % 60),
    );

    set_text(app.labels[V_REPORTS], &commas(reports));
    set_text(app.labels[V_EVENTS], &commas(events));
    set_text(app.labels[V_DROPPED], &commas(dropped));
    set_text(
        app.labels[V_RATE],
        &if paused {
            "-".to_string()
        } else {
            match measured_rate(app) {
                Some((hz, gap)) => format!("{hz:.0} Hz    median gap {gap:.2} ms"),
                None => format!("{:.0}/s live    (move to measure)", app.rate_hz),
            }
        },
    );
    set_text(
        app.labels[V_QUEUE],
        &format!(
            "{} / {}    peak {}",
            app.ring.depth(),
            commas(crate::ring::CAP as u64),
            app.ring.peak_depth()
        ),
    );
    set_text(
        app.labels[V_LATENCY],
        &format!(
            "{:.1} ms    errors {}",
            app.status.last_write_latency_us.load(Ordering::Relaxed) as f64 / 1000.0,
            errors
        ),
    );

    let (active, others) = device_rows(app);
    set_text(app.labels[V_MOUSE], &active);
    set_text(app.labels[V_OTHER], &others);

    set_text(app.labels[V_MACHINE], &app.machine);

    // Start time, in LOCAL time.
    //
    // The session id is UTC, and slicing the clock out of it while deriving the
    // weekday and day part from local time put "21:28 ... afternoon" on screen:
    // two different timezones in one row. Everything here comes off the same
    // local conversion now. The UTC id is still in the folder path below, and
    // both are in session.json.
    let start_ms = app.status.session_start_unix_ms.load(Ordering::Relaxed);
    let started = if start_ms > 0 {
        let off = crate::clock::local_offset_minutes();
        let local_iso = crate::clock::iso8601_local(start_ms, off);
        let hhmmss = local_iso.get(11..19).unwrap_or("").to_string();
        let local_ms = (start_ms as i64 + off as i64 * 60_000).max(0) as u64;
        format!(
            "{}   {}, {}",
            hhmmss,
            crate::clock::WEEKDAY_NAMES[crate::clock::weekday(local_ms) as usize],
            crate::clock::day_part(crate::clock::local_hour(start_ms, off))
        )
    } else {
        "-".to_string()
    };
    set_text(app.labels[V_STARTED], &started);
    set_text(app.labels[V_ONDISK], &human_bytes(session_bytes(app)));

    let dir = app.status.session_dir().display().to_string();
    // Keep the tail: the session folder matters, the profile prefix does not.
    let shown = if dir.len() > 52 { format!("...{}", &dir[dir.len() - 49..]) } else { dir };
    set_text(app.labels[V_FOLDER], &shown);

    set_text(app.btn_pause, if paused { "Resume" } else { "Pause" });
    let _ = hwnd;
}

// ---------------------------------------------------------------------------
// actions
// ---------------------------------------------------------------------------

unsafe fn show_window(hwnd: HWND, app: &mut App, show: bool) {
    app.visible = show;
    // Twice when showing, for the same STARTUPINFO reason as at startup: if the
    // process began with --tray this may still be its first ShowWindow call,
    // and "Open Dashboard" would silently do nothing.
    ShowWindow(hwnd, if show { SW_SHOW } else { SW_HIDE });
    if show {
        ShowWindow(hwnd, SW_SHOW);
    }
    if show {
        SetForegroundWindow(hwnd);
        SetTimer(hwnd, TIMER_REFRESH, REFRESH_MS, None);
        refresh(hwnd, app);
    } else {
        // Nothing to repaint while hidden; stop the timer entirely.
        KillTimer(hwnd, TIMER_REFRESH);
    }
}

unsafe fn open_folder(app: &App) {
    let dir = app.status.session_dir();
    let p = wide(&dir.display().to_string());
    let verb = wide("open");
    ShellExecuteW(
        std::ptr::null_mut(),
        verb.as_ptr(),
        p.as_ptr(),
        std::ptr::null(),
        std::ptr::null(),
        SW_SHOWNORMAL,
    );
}

unsafe fn message_box(hwnd: HWND, text: &str, caption: &str, icon: u32) {
    let t = wide(text);
    let c = wide(caption);
    MessageBoxW(hwnd, t.as_ptr(), c.as_ptr(), MB_OK | icon);
}

/// Export runs on a worker thread: a long session takes seconds to flatten and
/// the UI must not freeze while it does.
unsafe fn export_current(hwnd: HWND, app: &App) {
    let dir = app.status.session_dir();
    if dir.as_os_str().is_empty() {
        message_box(hwnd, "No active session yet.", "Export", MB_ICONWARNING);
        return;
    }
    std::thread::spawn(move || {
        let out = dir.join("events.csv");
        match crate::export::session_to_csv(&dir, &out) {
            Ok(s) => {
                let msg = format!(
                    "Exported {} events to\n{}\n\nNote: this exports what has been flushed to \
                     disk so far. The newest second or so is still buffered.",
                    s.events,
                    out.display()
                );
                // SAFETY: MessageBoxW with a null owner is valid from any thread.
                message_box(std::ptr::null_mut(), &msg, "Export complete", MB_ICONINFORMATION);
            }
            Err(e) => message_box(
                std::ptr::null_mut(),
                &format!("Export failed: {e}"),
                "Export",
                MB_ICONERROR,
            ),
        }
    });
}

unsafe fn toggle_startup(hwnd: HWND, app: &mut App) {
    match startup::toggle() {
        Ok(method) => {
            let on = method != startup::Method::Off;
            app.startup_on = on;
            InvalidateRect(app.btn_startup, std::ptr::null(), 1);
            if on {
                // Say which mechanism actually took effect. The scheduled task
                // needs elevation and quietly falls back to the registry Run
                // key; the user is entitled to know which one is running.
                let extra = if method == startup::Method::RunKey {
                    "\n\nRegistered via the registry Run key. (The scheduled-task route needs \
                     administrator rights; the Run key does not, but it cannot restart the \
                     recorder if it ever crashes.)"
                } else {
                    "\n\nRegistered as a scheduled task."
                };
                message_box(
                    hwnd,
                    &format!("HumanTelemetry will start when you sign in.{extra}"),
                    "Startup",
                    MB_ICONINFORMATION,
                );
            }
        }
        Err(e) => {
            // Put the control back to the truth rather than leaving it lying.
            app.startup_on = startup::is_enabled();
            InvalidateRect(app.btn_startup, std::ptr::null(), 1);
            message_box(
                hwnd,
                &format!("Could not change startup setting:\n{e}"),
                "Startup",
                MB_ICONERROR,
            );
        }
    }
}

// ---------------------------------------------------------------------------
// raw events viewer
// ---------------------------------------------------------------------------

/// Format the tail of the event stream exactly as it was stored.
///
/// This exists to answer one question a summary screen cannot: is the recorder
/// really capturing full-resolution per-report data, or does it only look like
/// it from the counters?
fn raw_dump(app: &App) -> String {
    use crate::event::*;
    let Ok(q) = app.status.recent.lock() else {
        return "unavailable".into();
    };
    let mut s = String::with_capacity(q.len() * 64);
    s.push_str("      t (s)   dev     dx     dy       x      y   event\r\n");
    s.push_str("--------------------------------------------------------------\r\n");
    let skip = q.len().saturating_sub(crate::storage::writer::RECENT_SHOWN);
    for e in q.iter().skip(skip) {
        let what = match e.kind {
            EV_MOVE => "MOVE".to_string(),
            EV_WHEEL => format!(
                "{}WHEEL {:+}",
                if e.flags & F_HWHEEL != 0 { "H" } else { "V" },
                e.wheel
            ),
            EV_SYNC => "SYNC".to_string(),
            EV_BUTTON => format!(
                "{}_{}",
                button_name(e.btn).to_uppercase(),
                if e.flags & F_DOWN != 0 { "DOWN" } else { "UP" }
            ),
            _ => "?".to_string(),
        };
        s.push_str(&format!(
            "{:>12.6}   #{:<3} {:>6} {:>6}   {:>5}  {:>5}   {}\r\n",
            e.t_ns as f64 / 1e9,
            e.dev,
            e.dx,
            e.dy,
            e.x,
            e.y,
            what
        ));
    }
    s
}

unsafe fn open_raw_window(parent: HWND, app: &mut App) {
    if !app.raw_hwnd.is_null() {
        ShowWindow(app.raw_hwnd, SW_SHOW);
        SetForegroundWindow(app.raw_hwnd);
        return;
    }
    let hinst = GetModuleHandleW(std::ptr::null());
    let class = wide("HumanTelemetryRaw");
    let mut wc: WNDCLASSW = std::mem::zeroed();
    wc.lpfnWndProc = Some(raw_wndproc);
    wc.hInstance = hinst;
    wc.lpszClassName = class.as_ptr();
    wc.hCursor = LoadCursorW(std::ptr::null_mut(), IDC_ARROW);
    wc.hbrBackground = app.theme.bg;
    RegisterClassW(&wc);

    let title = wide("Raw Events - last 200 records, as stored");
    let hwnd = CreateWindowExW(
        0,
        class.as_ptr(),
        title.as_ptr(),
        WS_OVERLAPPEDWINDOW,
        CW_USEDEFAULT,
        CW_USEDEFAULT,
        640,
        540,
        parent,
        std::ptr::null_mut(),
        hinst,
        std::ptr::null(),
    );
    if hwnd.is_null() {
        return;
    }
    theme::dark_titlebar(hwnd);

    // ES_MULTILINE | ES_READONLY = 0x0004 | 0x0800
    let edit = CreateWindowExW(
        0,
        wide("EDIT").as_ptr(),
        wide("").as_ptr(),
        WS_CHILD | WS_VISIBLE | WS_VSCROLL | WS_HSCROLL | 0x0004 | 0x0800,
        0,
        0,
        620,
        500,
        hwnd,
        std::ptr::null_mut(),
        hinst,
        std::ptr::null(),
    );
    SendMessageW(edit, WM_SETFONT, app.theme.font_mono as WPARAM, 1);

    app.raw_hwnd = hwnd;
    app.raw_edit = edit;
    SetWindowLongPtrW(hwnd, GWLP_USERDATA, app as *mut App as isize);
    ShowWindow(hwnd, SW_SHOW);
    refresh_raw(app);
    SetTimer(hwnd, TIMER_REFRESH, 1500, None);
}

unsafe fn refresh_raw(app: &App) {
    if app.raw_edit.is_null() {
        return;
    }
    // EM_GETFIRSTVISIBLELINE / EM_LINESCROLL. Replacing the text resets the
    // view to the top, which made the window unreadable while it was live:
    // you could not scroll back without it yanking you away twice a second.
    const EM_GETFIRSTVISIBLELINE: u32 = 0x00CE;
    const EM_LINESCROLL: u32 = 0x00B6;

    let first = SendMessageW(app.raw_edit, EM_GETFIRSTVISIBLELINE, 0, 0);
    let text = wide(&raw_dump(app));
    SetWindowTextW(app.raw_edit, text.as_ptr());
    if first > 0 {
        SendMessageW(app.raw_edit, EM_LINESCROLL, 0, first);
    }
    // Force a full erase-and-repaint. An edit control repaints text without
    // erasing first, so without this each refresh draws on top of the last
    // and the glyphs smear into an unreadable bold mess.
    InvalidateRect(app.raw_edit, std::ptr::null(), 1);
}

unsafe extern "system" fn raw_wndproc(hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    let ptr = GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *mut App;
    match msg {
        WM_TIMER if !ptr.is_null() => {
            refresh_raw(&*ptr);
            0
        }
        // Dark background for the read-only edit control.
        //
        // Deliberately OPAQUE, not TRANSPARENT. An edit control repaints its
        // text without erasing first, so a transparent background mode makes
        // every refresh overdraw the previous frame and the glyphs smear into
        // an unreadable bold mess. OPAQUE + SetBkColor makes each character
        // cell paint its own background, which is what actually clears it.
        WM_CTLCOLORSTATIC | WM_CTLCOLOREDIT if !ptr.is_null() => {
            let app = &*ptr;
            const OPAQUE_BK: i32 = 2;
            SetBkMode(wp as HDC, OPAQUE_BK);
            SetTextColor(wp as HDC, theme::VALUE);
            windows_sys::Win32::Graphics::Gdi::SetBkColor(wp as HDC, theme::BG);
            app.theme.bg as LRESULT
        }
        WM_SIZE if !ptr.is_null() => {
            let app = &*ptr;
            if !app.raw_edit.is_null() {
                MoveWindow(
                    app.raw_edit,
                    0,
                    0,
                    (lp & 0xFFFF) as i32,
                    ((lp >> 16) & 0xFFFF) as i32,
                    1,
                );
            }
            0
        }
        WM_CLOSE => {
            KillTimer(hwnd, TIMER_REFRESH);
            if !ptr.is_null() {
                let app = &mut *ptr;
                app.raw_hwnd = std::ptr::null_mut();
                app.raw_edit = std::ptr::null_mut();
            }
            DestroyWindow(hwnd);
            0
        }
        _ => DefWindowProcW(hwnd, msg, wp, lp),
    }
}

// ---------------------------------------------------------------------------
// tray menu + window procedure
// ---------------------------------------------------------------------------

unsafe fn show_tray_menu(hwnd: HWND, app: &App) {
    let menu = CreatePopupMenu();
    if menu.is_null() {
        return;
    }
    let paused = app.capture.state.paused.load(Ordering::Relaxed);
    let mut add = |id: usize, text: &str, flags: u32| {
        AppendMenuW(menu, MF_STRING | flags, id, wide(text).as_ptr());
    };
    add(MENU_SHOW, "Open Dashboard", 0);
    add(MENU_PAUSE, if paused { "Resume Recording" } else { "Pause Recording" }, 0);
    add(MENU_NEWSESSION, "Save + New Session", 0);
    add(MENU_FOLDER, "Open Data Folder", 0);
    add(MENU_RAW, "View Raw Events", 0);
    AppendMenuW(menu, MF_SEPARATOR, 0, std::ptr::null());
    add(
        MENU_STARTUP,
        "Start with Windows",
        if app.startup_on { MF_CHECKED } else { MF_UNCHECKED },
    );
    AppendMenuW(menu, MF_SEPARATOR, 0, std::ptr::null());
    add(MENU_EXIT, "Exit", 0);

    let mut pt = POINT { x: 0, y: 0 };
    GetCursorPos(&mut pt);
    // Required so the menu dismisses when the user clicks elsewhere.
    SetForegroundWindow(hwnd);
    let cmd = TrackPopupMenu(
        menu,
        TPM_RIGHTBUTTON | TPM_RETURNCMD,
        pt.x,
        pt.y,
        0,
        hwnd,
        std::ptr::null(),
    );
    DestroyMenu(menu);
    if cmd != 0 {
        PostMessageW(hwnd, WM_COMMAND, cmd as WPARAM, 0);
    }
}

unsafe fn begin_shutdown(hwnd: HWND, app: &mut App) {
    if app.shutting_down {
        return;
    }
    app.shutting_down = true;
    KillTimer(hwnd, TIMER_REFRESH);
    app.tray = None;
    DestroyWindow(hwnd);
}

unsafe extern "system" fn wndproc(hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    let ptr = GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *mut App;
    if ptr.is_null() {
        return DefWindowProcW(hwnd, msg, wp, lp);
    }
    let app = &mut *ptr;

    match msg {
        WM_PAINT => {
            on_paint(hwnd, app);
            0
        }
        WM_CTLCOLORSTATIC => {
            let id = GetDlgCtrlID(lp as HWND) as usize;
            SetBkMode(wp as HDC, TRANSPARENT as i32);
            SetTextColor(wp as HDC, static_color(app, id));
            windows_sys::Win32::Graphics::Gdi::SetBkColor(wp as HDC, theme::BG);
            app.theme.bg as LRESULT
        }
        WM_DRAWITEM => {
            let dis = &*(lp as *const DRAWITEMSTRUCT);
            on_drawitem(app, dis);
            1
        }
        WM_TIMER if wp == TIMER_REFRESH => {
            if app.visible {
                refresh(hwnd, app);
            }
            0
        }
        WM_TRAY => {
            match lp as u32 {
                WM_RBUTTONUP => show_tray_menu(hwnd, app),
                WM_LBUTTONDBLCLK => show_window(hwnd, app, true),
                _ => {}
            }
            0
        }
        WM_COMMAND => {
            match (wp & 0xFFFF) as usize {
                ID_PAUSE | MENU_PAUSE => {
                    let now = !app.capture.state.paused.load(Ordering::Relaxed);
                    app.capture.set_paused(now);
                    refresh(hwnd, app);
                }
                ID_NEWSESSION | MENU_NEWSESSION => {
                    if let Some(w) = app.writer.as_ref() {
                        w.new_session("user_requested");
                    }
                }
                ID_FOLDER | MENU_FOLDER => open_folder(app),
                ID_RAW | MENU_RAW => open_raw_window(hwnd, app),
                ID_EXPORT => export_current(hwnd, app),
                ID_STARTUP | MENU_STARTUP => toggle_startup(hwnd, app),
                ID_HIDE => show_window(hwnd, app, false),
                MENU_SHOW => show_window(hwnd, app, true),
                MENU_EXIT => begin_shutdown(hwnd, app),
                _ => {}
            }
            0
        }
        // Closing hides; only Exit really stops. See the module note.
        WM_CLOSE => {
            show_window(hwnd, app, false);
            0
        }
        // Logoff/shutdown: let it proceed, but flush on the way out.
        WM_QUERYENDSESSION => 1,
        WM_ENDSESSION => {
            if wp != 0 {
                app.capture.set_paused(true);
                if let Some(w) = app.writer.take() {
                    w.stop();
                }
            }
            0
        }
        WM_DESTROY => {
            PostQuitMessage(0);
            0
        }
        _ => DefWindowProcW(hwnd, msg, wp, lp),
    }
}
