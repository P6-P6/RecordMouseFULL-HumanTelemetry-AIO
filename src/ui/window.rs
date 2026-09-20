//! The dashboard window and the application message loop.
//!
//! Plain Win32 rather than egui/iced, and not to save disk space. This process
//! is meant to sit running for weeks: an immediate-mode GUI repaints on a
//! continuous loop and keeps a GPU surface alive to do it. These controls
//! repaint only when a value changes, and the refresh timer is stopped
//! outright while the window is hidden, so a minimised recorder costs nothing
//! at all.
//!
//! Closing the window hides it to the tray; only Exit on the tray menu ends
//! recording. That is the usual contract for a background recorder, and it
//! stops an accidental click on the X from silently ending a long capture.

use crate::input::devices::DeviceInfo;
use crate::input::raw_input::CaptureHandle;
use crate::ring::Ring;
use crate::storage::writer::{SharedStatus, WriterHandle};
use crate::ui::tray::{Tray, TrayState, WM_TRAY};
use crate::windows::startup;

use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::Instant;

use windows_sys::Win32::Foundation::{HWND, LPARAM, LRESULT, POINT, WPARAM};
use windows_sys::Win32::Graphics::Gdi::{
    CreateFontW, DeleteObject, UpdateWindow, ANTIALIASED_QUALITY, CLIP_DEFAULT_PRECIS,
    COLOR_BTNFACE, DEFAULT_CHARSET, FF_DONTCARE, HFONT, OUT_DEFAULT_PRECIS, VARIABLE_PITCH,
};
use windows_sys::Win32::System::SystemServices::SS_LEFT;
use windows_sys::Win32::UI::Controls::{BST_CHECKED, BST_UNCHECKED};
use windows_sys::Win32::System::LibraryLoader::GetModuleHandleW;
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

/// Indices into the value-label array.
const V_STATUS: usize = 0;
const V_SESSION: usize = 1;
const V_ELAPSED: usize = 2;
const V_REPORTS: usize = 3;
const V_EVENTS: usize = 4;
const V_DROPPED: usize = 5;
const V_QUEUE: usize = 6;
const V_RATE: usize = 7;
const V_DEVICE: usize = 8;
const V_FOLDER: usize = 9;
const V_COUNT: usize = 10;

pub struct App {
    pub capture: CaptureHandle,
    pub writer: Option<WriterHandle>,
    pub status: Arc<SharedStatus>,
    pub ring: Arc<Ring>,
    pub devices: Vec<DeviceInfo>,

    labels: [HWND; V_COUNT],
    btn_pause: HWND,
    btn_startup: HWND,
    tray: Option<Tray>,
    font: HFONT,
    font_big: HFONT,

    started: Instant,
    last_reports: u64,
    last_tick: Instant,
    /// Smoothed instantaneous report rate.
    rate_hz: f64,
    /// Highest rate seen, which is the closest thing to the device's true
    /// polling ceiling: hand movement is bursty, so a one-second average is
    /// always an undercount.
    rate_peak: f64,
    /// Mean over sampling windows where the mouse was actually moving.
    rate_sum: f64,
    rate_samples: u64,
    /// The raw-events diagnostics window, when open.
    raw_hwnd: HWND,
    raw_edit: HWND,
    visible: bool,
    shutting_down: bool,
}

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
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
    let class = wide("HumanTelemetryWindow");

    // SAFETY: zeroed WNDCLASSW with the required fields populated below.
    let mut wc: WNDCLASSW = unsafe { std::mem::zeroed() };
    wc.lpfnWndProc = Some(wndproc);
    wc.hInstance = hinst;
    wc.lpszClassName = class.as_ptr();
    // SAFETY: standard cursor/background lookups with null instance.
    unsafe {
        wc.hCursor = LoadCursorW(std::ptr::null_mut(), IDC_ARROW);
        wc.hbrBackground = (COLOR_BTNFACE + 1) as _;
        RegisterClassW(&wc);
    }

    let title = wide("HumanTelemetry - Mouse Recorder");
    // SAFETY: registered class; sizes are literals.
    let hwnd = unsafe {
        CreateWindowExW(
            0,
            class.as_ptr(),
            title.as_ptr(),
            // No maximise/resize: the layout is fixed, and a resizable window
            // that does nothing when resized is worse than one that cannot be.
            WS_OVERLAPPED | WS_CAPTION | WS_SYSMENU | WS_MINIMIZEBOX,
            CW_USEDEFAULT,
            CW_USEDEFAULT,
            580,
            446,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            hinst,
            std::ptr::null(),
        )
    };
    if hwnd.is_null() {
        eprintln!("could not create window");
        return;
    }

    let mut app = Box::new(App {
        capture,
        writer: Some(writer),
        status,
        ring,
        devices,
        labels: [std::ptr::null_mut(); V_COUNT],
        btn_pause: std::ptr::null_mut(),
        btn_startup: std::ptr::null_mut(),
        tray: None,
        font: std::ptr::null_mut(),
        font_big: std::ptr::null_mut(),
        started: Instant::now(),
        last_reports: 0,
        last_tick: Instant::now(),
        rate_hz: 0.0,
        rate_peak: 0.0,
        rate_sum: 0.0,
        rate_samples: 0,
        raw_hwnd: std::ptr::null_mut(),
        raw_edit: std::ptr::null_mut(),
        visible: !start_hidden,
        shutting_down: false,
    });

    // SAFETY: the Box outlives the window; it is reclaimed after the loop ends.
    unsafe {
        SetWindowLongPtrW(hwnd, GWLP_USERDATA, app.as_mut() as *mut App as isize);
        build_controls(hwnd, &mut app, hinst);
        app.tray = Some(Tray::new(hwnd, TrayState::Recording));
        if app.visible {
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
            // No IsDialogMessage: these are plain controls, and tab traversal
            // through six buttons is not worth a modeless dialog.
            TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }

        // Tray icon must go before the window does.
        app.tray = None;
        if !app.font.is_null() {
            DeleteObject(app.font as _);
        }
        if !app.font_big.is_null() {
            DeleteObject(app.font_big as _);
        }
        SetWindowLongPtrW(hwnd, GWLP_USERDATA, 0);
    }

    // Order matters: stop producing before draining, so the writer's final
    // pass sees a ring that nothing is still filling.
    app.capture.set_paused(true);
    let writer = app.writer.take();
    // `capture` is consumed by stop(); move it out of the box first.
    let App { capture, .. } = *app;
    capture.stop();
    if let Some(w) = writer {
        w.stop();
    }
}

unsafe fn build_controls(hwnd: HWND, app: &mut App, hinst: windows_sys::Win32::Foundation::HMODULE) {
    let face = wide("Segoe UI");
    app.font = CreateFontW(
        -14, 0, 0, 0, 400, 0, 0, 0,
        DEFAULT_CHARSET as u32, OUT_DEFAULT_PRECIS as u32, CLIP_DEFAULT_PRECIS as u32,
        ANTIALIASED_QUALITY as u32, (VARIABLE_PITCH | FF_DONTCARE) as u32, face.as_ptr(),
    );
    app.font_big = CreateFontW(
        -20, 0, 0, 0, 700, 0, 0, 0,
        DEFAULT_CHARSET as u32, OUT_DEFAULT_PRECIS as u32, CLIP_DEFAULT_PRECIS as u32,
        ANTIALIASED_QUALITY as u32, (VARIABLE_PITCH | FF_DONTCARE) as u32, face.as_ptr(),
    );

    let static_cls = wide("STATIC");
    let button_cls = wide("BUTTON");

    let mut mk_static = |text: &str, x: i32, y: i32, w: i32, h: i32, big: bool| -> HWND {
        let t = wide(text);
        let h = CreateWindowExW(
            0,
            static_cls.as_ptr(),
            t.as_ptr(),
            WS_CHILD | WS_VISIBLE | SS_LEFT as u32,
            x, y, w, h,
            hwnd,
            std::ptr::null_mut(),
            hinst,
            std::ptr::null(),
        );
        let f = if big { app.font_big } else { app.font };
        SendMessageW(h, WM_SETFONT, f as WPARAM, 1);
        h
    };

    app.labels[V_STATUS] = mk_static("starting...", 16, 12, 540, 28, true);

    const ROWS: [(&str, usize); 9] = [
        ("Session", V_SESSION),
        ("Recording for", V_ELAPSED),
        ("Input reports", V_REPORTS),
        ("Events written", V_EVENTS),
        ("Dropped", V_DROPPED),
        ("Queue depth", V_QUEUE),
        ("Report rate", V_RATE),
        ("Active device", V_DEVICE),
        ("Data folder", V_FOLDER),
    ];
    let mut y = 52;
    for (name, idx) in ROWS {
        mk_static(name, 16, y, 110, 20, false);
        app.labels[idx] = mk_static("-", 132, y, 424, 20, false);
        y += 24;
    }

    let mut mk_button = |text: &str, id: usize, x: i32, y: i32, w: i32, style: u32| -> HWND {
        let t = wide(text);
        let h = CreateWindowExW(
            0,
            button_cls.as_ptr(),
            t.as_ptr(),
            WS_CHILD | WS_VISIBLE | WS_TABSTOP | style,
            x, y, w, 30,
            hwnd,
            id as _,
            hinst,
            std::ptr::null(),
        );
        SendMessageW(h, WM_SETFONT, app.font as WPARAM, 1);
        h
    };

    let by = 282;
    app.btn_pause = mk_button("Pause", ID_PAUSE, 16, by, 108, BS_PUSHBUTTON as u32);
    mk_button("Save + New Session", ID_NEWSESSION, 132, by, 160, BS_PUSHBUTTON as u32);
    mk_button("Open Data Folder", ID_FOLDER, 300, by, 140, BS_PUSHBUTTON as u32);
    mk_button("Export CSV", ID_EXPORT, 448, by, 108, BS_PUSHBUTTON as u32);

    app.btn_startup = mk_button(
        "Start when I sign in to Windows",
        ID_STARTUP,
        16,
        by + 40,
        260,
        BS_AUTOCHECKBOX as u32,
    );
    SendMessageW(
        app.btn_startup,
        BM_SETCHECK,
        if startup::is_enabled() { BST_CHECKED } else { BST_UNCHECKED } as WPARAM,
        0,
    );
    mk_button("View Raw Events", ID_RAW, 292, by + 40, 144, BS_PUSHBUTTON as u32);
    mk_button("Hide to Tray", ID_HIDE, 448, by + 40, 108, BS_PUSHBUTTON as u32);

    mk_static(
        "Recording is continuous and saved as it happens. Closing this window keeps\n\
         recording in the tray; use Exit on the tray menu to stop.",
        16,
        by + 78,
        540,
        36,
        false,
    );
}

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
    if cur[..cur.len().saturating_sub(1)] == new[..new.len() - 1] {
        return;
    }
    SetWindowTextW(h, new.as_ptr());
}

unsafe fn refresh(_hwnd: HWND, app: &mut App) {
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
        // Light smoothing: the raw number jitters because mouse movement is
        // bursty, and a twitching readout is harder to read than a laggy one.
        app.rate_hz = if app.rate_hz == 0.0 { inst } else { app.rate_hz * 0.6 + inst * 0.4 };
        // Peak and average use the *unsmoothed* value, and only count windows
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

    let state = if errors > 0 {
        TrayState::Error
    } else if paused {
        TrayState::Paused
    } else {
        TrayState::Recording
    };
    if let Some(t) = app.tray.as_mut() {
        t.set_state(state);
    }

    set_text(
        app.labels[V_STATUS],
        match state {
            TrayState::Recording => "RECORDING",
            TrayState::Paused => "PAUSED",
            TrayState::Error => "RECORDING (writer errors - see below)",
        },
    );

    let secs = app.started.elapsed().as_secs();
    set_text(app.labels[V_SESSION], &app.status.session_id());
    set_text(
        app.labels[V_ELAPSED],
        &format!("{:02}:{:02}:{:02}", secs / 3600, (secs / 60) % 60, secs % 60),
    );
    set_text(app.labels[V_REPORTS], &format!("{reports}"));
    set_text(
        app.labels[V_EVENTS],
        &if errors > 0 {
            format!("{events}    ({errors} writer error(s))")
        } else {
            format!("{events}")
        },
    );
    set_text(
        app.labels[V_DROPPED],
        &if dropped > 0 { format!("{dropped}  -- DATA LOSS") } else { "0".into() },
    );
    set_text(
        app.labels[V_QUEUE],
        &format!("{} / {}   (peak {})", app.ring.depth(), crate::ring::CAP, app.ring.peak_depth()),
    );
    set_text(
        app.labels[V_RATE],
        &if paused {
            "-".to_string()
        } else {
            let avg = if app.rate_samples > 0 {
                app.rate_sum / app.rate_samples as f64
            } else {
                0.0
            };
            format!(
                "{:.0} Hz now    peak {:.0} Hz    avg while moving {:.0} Hz",
                app.rate_hz, app.rate_peak, avg
            )
        },
    );
    set_text(app.labels[V_DEVICE], &device_line(app));
    set_text(app.labels[V_FOLDER], &app.status.session_dir().display().to_string());

    set_text(app.btn_pause, if paused { "Resume" } else { "Pause" });
}

/// Which mice Windows can see, and which are actually producing events.
///
/// The bare list of HID ids was misleading: a machine reports several pointer
/// collections (a keyboard's media keys, a wireless dongle) and only one of
/// them is the mouse in your hand. Event counts settle it.
fn device_line(app: &App) -> String {
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
            (n, format!("#{} {} {}btn {}ev", d.index, id, d.buttons, n))
        })
        .collect();
    if parts.is_empty() {
        return "none detected".into();
    }
    // Busiest first: the one in your hand leads.
    parts.sort_by(|a, b| b.0.cmp(&a.0));
    parts.into_iter().map(|(_, s)| s).collect::<Vec<_>>().join("   |   ")
}

// ---------------------------------------------------------------------------
// raw events viewer
// ---------------------------------------------------------------------------

/// Format the tail of the event stream exactly as it was stored.
///
/// This exists to answer one question that a summary screen cannot: is the
/// recorder really capturing full-resolution per-report data, or does it only
/// look like it from the counters?
fn raw_dump(app: &App) -> String {
    use crate::event::*;
    let Ok(q) = app.status.recent.lock() else {
        return "unavailable".into();
    };
    let mut s = String::with_capacity(q.len() * 64);
    s.push_str("      t (s)   dev     dx     dy       x      y   event\r\n");
    s.push_str("--------------------------------------------------------------\r\n");
    for e in q.iter() {
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
    wc.hbrBackground = (COLOR_BTNFACE + 1) as _;
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
    let edit_cls = wide("EDIT");
    let empty = wide("");
    let edit = CreateWindowExW(
        WS_EX_CLIENTEDGE,
        edit_cls.as_ptr(),
        empty.as_ptr(),
        WS_CHILD | WS_VISIBLE | WS_VSCROLL | WS_HSCROLL | (ES_MULTILINE | ES_READONLY) as u32,
        0,
        0,
        620,
        500,
        hwnd,
        std::ptr::null_mut(),
        hinst,
        std::ptr::null(),
    );
    // Monospaced: the columns are the whole point of this view.
    let face = wide("Consolas");
    let font = CreateFontW(
        -13, 0, 0, 0, 400, 0, 0, 0,
        DEFAULT_CHARSET as u32, OUT_DEFAULT_PRECIS as u32, CLIP_DEFAULT_PRECIS as u32,
        ANTIALIASED_QUALITY as u32, (VARIABLE_PITCH | FF_DONTCARE) as u32, face.as_ptr(),
    );
    SendMessageW(edit, WM_SETFONT, font as WPARAM, 1);

    app.raw_hwnd = hwnd;
    app.raw_edit = edit;
    SetWindowLongPtrW(hwnd, GWLP_USERDATA, app as *mut App as isize);
    ShowWindow(hwnd, SW_SHOW);
    refresh_raw(app);
    SetTimer(hwnd, TIMER_REFRESH, 750, None);
}

unsafe fn refresh_raw(app: &App) {
    if app.raw_edit.is_null() {
        return;
    }
    let text = wide(&raw_dump(app));
    SetWindowTextW(app.raw_edit, text.as_ptr());
}

unsafe extern "system" fn raw_wndproc(hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    let ptr = GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *mut App;
    match msg {
        WM_TIMER if !ptr.is_null() => {
            refresh_raw(&*ptr);
            0
        }
        WM_SIZE if !ptr.is_null() => {
            let app = &*ptr;
            if !app.raw_edit.is_null() {
                let w = (lp & 0xFFFF) as i32;
                let h = ((lp >> 16) & 0xFFFF) as i32;
                MoveWindow(app.raw_edit, 0, 0, w, h, 1);
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

unsafe fn show_window(hwnd: HWND, app: &mut App, show: bool) {
    app.visible = show;
    ShowWindow(hwnd, if show { SW_SHOW } else { SW_HIDE });
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
        // SAFETY: MessageBoxW with a null owner is valid from any thread.
        match crate::export::session_to_csv(&dir, &out) {
            Ok(s) => {
                let msg = format!(
                    "Exported {} events to\n{}\n\nNote: this exports what has been flushed to \
                     disk so far. The newest second or so is still buffered.",
                    s.events,
                    out.display()
                );
                message_box(std::ptr::null_mut(), &msg, "Export complete", MB_ICONINFORMATION);
            }
            Err(e) => {
                message_box(std::ptr::null_mut(), &format!("Export failed: {e}"), "Export", MB_ICONERROR);
            }
        }
    });
}

unsafe fn toggle_startup(hwnd: HWND, app: &App) {
    match startup::toggle() {
        Ok(method) => {
            let on = method != startup::Method::Off;
            SendMessageW(
                app.btn_startup,
                BM_SETCHECK,
                if on { BST_CHECKED } else { BST_UNCHECKED } as WPARAM,
                0,
            );
            // Say which mechanism actually took effect. The scheduled task
            // needs elevation and quietly falls back to the registry Run key,
            // and the user is entitled to know which one is running.
            if on {
                let extra = if method == startup::Method::RunKey {
                    "\n\nRegistered via the registry Run key. (The scheduled-task                      route needs administrator rights; the Run key does not, but it                      cannot restart the recorder if it ever crashes.)"
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
            // Put the checkbox back to the truth rather than leaving it lying.
            SendMessageW(
                app.btn_startup,
                BM_SETCHECK,
                if startup::is_enabled() { BST_CHECKED } else { BST_UNCHECKED } as WPARAM,
                0,
            );
            message_box(hwnd, &format!("Could not change startup setting:\n{e}"), "Startup", MB_ICONERROR);
        }
    }
}

unsafe fn show_tray_menu(hwnd: HWND, app: &App) {
    let menu = CreatePopupMenu();
    if menu.is_null() {
        return;
    }
    let paused = app.capture.state.paused.load(Ordering::Relaxed);

    let mut add = |id: usize, text: &str, flags: u32| {
        let t = wide(text);
        AppendMenuW(menu, MF_STRING | flags, id, t.as_ptr());
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
        if startup::is_enabled() { MF_CHECKED } else { MF_UNCHECKED },
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
            let id = (wp & 0xFFFF) as usize;
            match id {
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
                ID_EXPORT => export_current(hwnd, app),
                ID_STARTUP | MENU_STARTUP => toggle_startup(hwnd, app),
                ID_RAW | MENU_RAW => open_raw_window(hwnd, app),
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
