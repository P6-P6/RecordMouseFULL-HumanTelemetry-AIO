//! MouseProfiler -- high-fidelity personal mouse-behaviour recorder.
//!
//! Captures, stores and organises the raw physical mouse signal. Analysis and
//! modelling are explicitly out of scope and belong to a separate program that
//! reads this one's output.
//!
//! Run with no arguments to open the recorder. `--help` lists the CLI tools.

// Release builds target the `windows` subsystem so launching the recorder does
// not flash a console (spec section 56). Debug builds keep the console, which
// is where the writer's diagnostics go. CLI subcommands reattach to the parent
// console explicitly -- see `console::attach`.
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod clock;
mod console;
mod context;
mod event;
mod export;
mod input;
mod ring;
mod storage;
mod ui;
mod windows;

use clock::Clock;
use ring::Ring;
use std::path::{Path, PathBuf};
use std::sync::atomic::Ordering;
use std::sync::Arc;
use storage::session::{recover_interrupted, Session};
use storage::writer::WriterConfig;

use windows_sys::Win32::UI::HiDpi::{
    SetProcessDpiAwarenessContext, DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2,
};

fn main() {
    // Must happen before any window or coordinate work. Without it Windows
    // lies about cursor coordinates on scaled displays, silently rescaling
    // every pixel this program records.
    // SAFETY: no preconditions; failure only means a lower awareness mode.
    unsafe {
        SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);
    }

    let args: Vec<String> = std::env::args().skip(1).collect();
    let cmd = args.first().map(|s| s.as_str()).unwrap_or("record");

    // Everything except the GUI writes to a terminal, so it needs the console
    // the user launched it from.
    if !matches!(cmd, "record" | "") {
        console::attach();
    }

    let result = match cmd {
        "record" | "" => cmd_record(&args),
        "list" => cmd_list(&args),
        "verify" => cmd_verify(&args),
        "export" => cmd_export(&args),
        "info" => cmd_info(),
        "startup" => cmd_startup(&args),
        "-h" | "--help" | "help" => {
            print_help();
            Ok(())
        }
        other => {
            eprintln!("unknown command: {other}\n");
            print_help();
            std::process::exit(2);
        }
    };

    if let Err(e) = result {
        eprintln!("error: {e}");
        std::process::exit(1);
    }
}

fn print_help() {
    println!(
        "HumanTelemetry {} -- RecordMouseFULL-HumanTelemetry-AIO

USAGE:
    HumanTelemetry.exe [COMMAND] [OPTIONS]

    With no command it opens the recorder window and starts recording.

COMMANDS:
    record              Open the recorder (default)
    list                List recorded sessions
    verify [SESSION]    Read every segment back and check it against the header
    export [SESSION]    Write a session out as CSV
    info                Show devices, monitors and pointer-ballistics settings
    startup on|off      Enable/disable starting at Windows sign-in

OPTIONS:
    --data <DIR>        Data root (default:
                        %LOCALAPPDATA%\\RecordMouseFULL-HumanTelemetry-AIO)
    --titles            Also record foreground window titles (off by default)
    --duration <SECS>   Stop after N seconds (headless; no window)
    --tray              Start hidden in the tray
    --out <FILE>        Output path for `export`

NOTES:
    Data defaults to local disk, never the project folder. At 1 kHz this writes
    continuously and a cloud-synced directory would re-upload it all day.
",
        storage::session::APP_VERSION
    );
}

fn flag_value(args: &[String], name: &str) -> Option<String> {
    args.iter().position(|a| a == name).and_then(|i| args.get(i + 1)).cloned()
}

fn has_flag(args: &[String], name: &str) -> bool {
    args.iter().any(|a| a == name)
}

fn data_root(args: &[String]) -> PathBuf {
    if let Some(d) = flag_value(args, "--data") {
        return PathBuf::from(d);
    }
    let base = std::env::var("LOCALAPPDATA")
        .map(PathBuf::from)
        .unwrap_or_else(|_| std::env::temp_dir());
    base.join("RecordMouseFULL-HumanTelemetry-AIO")
}

/// The mice Windows can see. The dashboard pairs these with live event counts
/// so it can show which one is actually in the user's hand.
fn detected_devices() -> Vec<input::devices::DeviceInfo> {
    input::devices::DeviceTable::new().devices().to_vec()
}

// ---------------------------------------------------------------------------
// record
// ---------------------------------------------------------------------------

fn cmd_record(args: &[String]) -> std::io::Result<()> {
    let root = data_root(args);
    std::fs::create_dir_all(&root)?;

    // Crash recovery first: mark anything that never shut down cleanly before
    // starting a new session. Nothing is repaired or removed.
    let rec = recover_interrupted(&root)?;
    if !rec.newly_marked_interrupted.is_empty() {
        println!(
            "[recovery] {} previous session(s) marked interrupted",
            rec.newly_marked_interrupted.len()
        );
    }

    let clock = Clock::new();
    let ring = Arc::new(Ring::new());
    let (ctx_tx, ctx_rx) = std::sync::mpsc::channel();

    let session = Session::create_populated(&root, "startup", clock.freq())?;

    let capture = input::raw_input::spawn(clock, Arc::clone(&ring), ctx_tx)?;
    let state = Arc::clone(&capture.state);

    let cfg = WriterConfig { capture_window_titles: has_flag(args, "--titles") };
    let (writer, status) = storage::writer::spawn(
        root.clone(),
        session,
        Arc::clone(&ring),
        ctx_rx,
        Arc::clone(&state),
        clock,
        cfg,
    )?;

    // Headless mode: no window, stop after N seconds. This is what a soak test
    // or a scheduled fixed-length capture uses.
    if let Some(secs) = flag_value(args, "--duration").and_then(|s| s.parse::<u64>().ok()) {
        console::attach();
        println!("recording for {secs}s (headless)");
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(secs);
        while std::time::Instant::now() < deadline {
            std::thread::sleep(std::time::Duration::from_millis(250));
        }
        capture.set_paused(true);
        capture.stop();
        writer.stop();

        let dir = status.session_dir();
        println!("session : {}", status.session_id());
        println!("events  : {}", status.events_written.load(Ordering::Relaxed));
        println!("dropped : {}", status.events_dropped.load(Ordering::Relaxed));
        println!("data    : {}", dir.display());
        return Ok(());
    }

    ui::window::run(
        capture,
        writer,
        status,
        ring,
        detected_devices(),
        has_flag(args, "--tray"),
    );
    Ok(())
}

// ---------------------------------------------------------------------------
// list / verify / export / info / startup
// ---------------------------------------------------------------------------

fn sessions_dir(args: &[String]) -> PathBuf {
    data_root(args).join("sessions")
}

fn all_sessions(args: &[String]) -> std::io::Result<Vec<PathBuf>> {
    let d = sessions_dir(args);
    if !d.exists() {
        return Ok(Vec::new());
    }
    let mut v: Vec<PathBuf> = std::fs::read_dir(&d)?
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter(|p| p.is_dir() && p.join("session.json").exists())
        .collect();
    v.sort();
    Ok(v)
}

/// Flags that consume the argument after them, so a positional scan does not
/// mistake their value for a session id.
const VALUE_FLAGS: [&str; 3] = ["--data", "--out", "--duration"];

/// First positional argument after the command, ignoring flags and their values.
fn positional(args: &[String]) -> Option<&String> {
    let mut i = 1; // args[0] is the command
    while i < args.len() {
        let a = &args[i];
        if VALUE_FLAGS.contains(&a.as_str()) {
            i += 2;
            continue;
        }
        if a.starts_with("--") {
            i += 1;
            continue;
        }
        if !a.is_empty() {
            return Some(a);
        }
        i += 1;
    }
    None
}

/// Resolve a session argument: an id, a path, or nothing (meaning the latest).
fn pick_session(args: &[String]) -> std::io::Result<PathBuf> {
    let named = positional(args);
    let all = all_sessions(args)?;
    if let Some(n) = named {
        let p = Path::new(n);
        if p.join("session.json").exists() {
            return Ok(p.to_path_buf());
        }
        if let Some(hit) =
            all.iter().find(|s| s.file_name().map(|f| f == n.as_str()).unwrap_or(false))
        {
            return Ok(hit.clone());
        }
        return Err(std::io::Error::new(
            std::io::ErrorKind::NotFound,
            format!("no session matching {n}"),
        ));
    }
    all.last().cloned().ok_or_else(|| {
        std::io::Error::new(std::io::ErrorKind::NotFound, "no sessions recorded yet")
    })
}

fn cmd_list(args: &[String]) -> std::io::Result<()> {
    let all = all_sessions(args)?;
    if all.is_empty() {
        println!("no sessions in {}", sessions_dir(args).display());
        return Ok(());
    }
    println!("{:<34} {:>12} {:>9} {:>10}  state", "session", "events", "dropped", "duration");
    for dir in &all {
        match export::verify_session(dir) {
            Ok(r) => {
                let state = if r.interrupted {
                    "INTERRUPTED"
                } else if r.clean_shutdown {
                    "clean"
                } else {
                    "in progress"
                };
                println!(
                    "{:<34} {:>12} {:>9} {:>9.0}s  {}",
                    r.session_id, r.events_found, r.dropped_claimed, r.duration_s, state
                );
            }
            Err(e) => println!(
                "{:<34} <unreadable: {e}>",
                dir.file_name().unwrap_or_default().to_string_lossy()
            ),
        }
    }
    Ok(())
}

fn cmd_verify(args: &[String]) -> std::io::Result<()> {
    let dir = pick_session(args)?;
    let r = export::verify_session(&dir)?;

    println!("session           : {}", r.session_id);
    println!("events on disk    : {}", r.events_found);
    println!("events in header  : {}", r.events_claimed);
    println!("dropped (reported): {}", r.dropped_claimed);
    println!("duration          : {:.1} s", r.duration_s);
    println!("truncated segments: {}", r.truncated_segments);
    println!("clean shutdown    : {}", r.clean_shutdown);
    println!("interrupted       : {}", r.interrupted);
    println!(
        "timestamp order   : {}",
        if r.monotonic_violations == 0 { "monotonic" } else { "VIOLATED" }
    );

    let mut ok = true;
    if r.monotonic_violations > 0 {
        println!("\nFAIL: {} timestamp regressions", r.monotonic_violations);
        ok = false;
    }
    // A session still in progress, or one that crashed, legitimately has fewer
    // events in the header than on disk. Only a clean session must agree.
    if r.clean_shutdown && r.events_found != r.events_claimed {
        println!(
            "\nFAIL: header claims {} events, disk holds {}",
            r.events_claimed, r.events_found
        );
        ok = false;
    }
    if r.truncated_segments > 0 && r.clean_shutdown {
        println!("\nFAIL: clean session must not have truncated segments");
        ok = false;
    }
    println!("\n{}", if ok { "OK" } else { "PROBLEMS FOUND" });
    if !ok {
        std::process::exit(1);
    }
    Ok(())
}

fn cmd_export(args: &[String]) -> std::io::Result<()> {
    let dir = pick_session(args)?;
    let out = flag_value(args, "--out")
        .map(PathBuf::from)
        .unwrap_or_else(|| dir.join("events.csv"));

    let stats = export::session_to_csv(&dir, &out)?;
    println!("exported {} events from {} segment(s)", stats.events, stats.segments);
    if stats.truncated_segments > 0 {
        println!(
            "note: {} segment(s) had a truncated tail (interrupted session)",
            stats.truncated_segments
        );
    }
    println!("-> {}", out.display());
    Ok(())
}

fn cmd_startup(args: &[String]) -> std::io::Result<()> {
    match positional(args).map(|s| s.as_str()) {
        Some("on") => windows::startup::enable()
            .map(|_| println!("startup task registered"))
            .map_err(std::io::Error::other),
        Some("off") => windows::startup::disable()
            .map(|_| println!("startup task removed"))
            .map_err(std::io::Error::other),
        _ => {
            println!(
                "start with Windows: {}",
                if windows::startup::is_enabled() { "on" } else { "off" }
            );
            println!("use `startup on` or `startup off` to change it");
            Ok(())
        }
    }
}

fn cmd_info() -> std::io::Result<()> {
    let devices = input::devices::DeviceTable::new();
    println!("MICE");
    for d in devices.devices().iter().filter(|d| !d.synthetic) {
        println!("  [{}] {}", d.index, d.name);
        println!(
            "       vid={} pid={} buttons={} reported_rate={}Hz hwheel={}",
            d.vendor_id.map(|v| format!("{v:04X}")).unwrap_or_else(|| "-".into()),
            d.product_id.map(|v| format!("{v:04X}")).unwrap_or_else(|| "-".into()),
            d.buttons,
            d.reported_sample_rate,
            d.has_horizontal_wheel
        );
    }
    if devices.devices().iter().all(|d| d.synthetic) {
        println!("  (none detected)");
    }

    println!("\nMONITORS");
    for m in context::monitors::enumerate() {
        println!(
            "  [{}] {} {}x{} @ ({},{})  dpi={} scale={:.2}{}",
            m.index,
            m.device,
            m.width(),
            m.height(),
            m.left,
            m.top,
            m.dpi_x,
            m.scaling,
            if m.primary { "  [primary]" } else { "" }
        );
    }

    let b = input::ballistics::Ballistics::snapshot();
    println!("\nPOINTER BALLISTICS");
    println!("  pointer speed (1-20)     : {}", b.pointer_speed);
    println!("  enhance pointer precision: {}", if b.is_linear() { "off" } else { "ON" });
    println!("  thresholds               : {} / {}", b.mouse_threshold1, b.mouse_threshold2);
    println!("  SmoothMouseXCurve        : {:?}", b.curve_x);
    println!("  SmoothMouseYCurve        : {:?}", b.curve_y);
    if !b.is_linear() {
        println!(
            "\n  Acceleration is ON, so raw->cursor is non-linear. The curve above is\n  \
             recorded with every session, which is what makes it invertible later."
        );
    }

    println!("\nSTARTUP");
    println!(
        "  start with Windows       : {}",
        if windows::startup::is_enabled() { "on" } else { "off" }
    );
    Ok(())
}
