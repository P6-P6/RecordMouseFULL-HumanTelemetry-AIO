//! Dataset export.
//!
//! The recorder's own format is compressed and binary because it has to be.
//! Analysis, though, happens in pandas/numpy, and the existing public work in
//! this area (generative-mouse-trajectories, HumanMoveMouse and friends) all
//! speaks CSV or JSONL. Exporting into that shape means this recorder's data
//! drops straight into tooling that already exists, instead of stranding it.
//!
//! Note what the CSV carries that those projects cannot: `raw_dx`/`raw_dy`, the
//! pre-ballistics motor signal. Their recorders only ever saw cursor pixels.

use crate::event::{button_name, Event, EV_BUTTON, EV_MOVE, EV_SYNC, EV_WHEEL, F_DOWN};
use crate::storage::segment::read_segment;
use crate::storage::session::SessionHeader;
use std::io::{BufWriter, Write};
use std::path::Path;

pub struct ExportStats {
    pub events: u64,
    pub segments: u32,
    pub truncated_segments: u32,
}

/// Flatten one session directory into a single CSV.
pub fn session_to_csv(session_dir: &Path, out_path: &Path) -> std::io::Result<ExportStats> {
    let header: SessionHeader =
        serde_json::from_str(&std::fs::read_to_string(session_dir.join("session.json"))?)
            .map_err(std::io::Error::other)?;

    let out = std::fs::File::create(out_path)?;
    let mut w = BufWriter::with_capacity(1 << 20, out);

    writeln!(
        w,
        "t_ns,kind,device,raw_dx,raw_dy,cursor_x,cursor_y,button,pressed,wheel,\
         mods_ctrl,mods_shift,mods_alt,mods_win,buttons_held,flags"
    )?;

    let mut stats = ExportStats { events: 0, segments: 0, truncated_segments: 0 };

    // Segment order is the order they were created, which is chronological.
    let mut names = header.segments.clone();
    if names.is_empty() {
        // Fall back to scanning, in case the header never got its final save.
        let mut found: Vec<String> = std::fs::read_dir(session_dir)?
            .filter_map(|e| e.ok())
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .filter(|n| n.ends_with(".mpseg"))
            .collect();
        found.sort();
        names = found;
    }

    for name in &names {
        let path = session_dir.join(name);
        if !path.exists() {
            continue;
        }
        let seg = read_segment(&path)?;
        stats.segments += 1;
        if seg.truncated {
            stats.truncated_segments += 1;
        }
        for e in &seg.events {
            write_row(&mut w, e)?;
            stats.events += 1;
        }
    }

    w.flush()?;
    Ok(stats)
}

fn write_row<W: Write>(w: &mut W, e: &Event) -> std::io::Result<()> {
    let kind = match e.kind {
        EV_MOVE => "move",
        EV_BUTTON => "button",
        EV_WHEEL => "wheel",
        EV_SYNC => "sync",
        _ => "unknown",
    };
    let (button, pressed) = if e.kind == EV_BUTTON {
        (button_name(e.btn), if e.flags & F_DOWN != 0 { "1" } else { "0" })
    } else {
        ("", "")
    };
    let m = |bit: u8| if e.mods & bit != 0 { 1 } else { 0 };
    writeln!(
        w,
        "{},{},{},{},{},{},{},{},{},{},{},{},{},{},{},{}",
        e.t_ns,
        kind,
        e.dev,
        e.dx,
        e.dy,
        e.x,
        e.y,
        button,
        pressed,
        e.wheel,
        m(crate::event::M_CTRL),
        m(crate::event::M_SHIFT),
        m(crate::event::M_ALT),
        m(crate::event::M_WIN),
        e.bstate,
        e.flags
    )
}

/// Read back every segment in a session and report what is actually there.
///
/// This is the acceptance check, not a convenience: it proves the bytes on disk
/// decode, and it compares the count against what the header claims so a
/// mismatch is loud rather than silent.
pub struct VerifyReport {
    pub session_id: String,
    pub events_found: u64,
    pub events_claimed: u64,
    pub dropped_claimed: u64,
    pub truncated_segments: u32,
    pub clean_shutdown: bool,
    pub interrupted: bool,
    pub monotonic_violations: u64,
    pub duration_s: f64,
}

pub fn verify_session(session_dir: &Path) -> std::io::Result<VerifyReport> {
    let header: SessionHeader =
        serde_json::from_str(&std::fs::read_to_string(session_dir.join("session.json"))?)
            .map_err(std::io::Error::other)?;

    let mut names = header.segments.clone();
    if names.is_empty() {
        let mut found: Vec<String> = std::fs::read_dir(session_dir)?
            .filter_map(|e| e.ok())
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .filter(|n| n.ends_with(".mpseg"))
            .collect();
        found.sort();
        names = found;
    }

    let mut found = 0u64;
    let mut truncated = 0u32;
    let mut violations = 0u64;
    let mut last_t = 0u64;
    let mut first_t = None;

    for name in &names {
        let path = session_dir.join(name);
        if !path.exists() {
            continue;
        }
        let seg = read_segment(&path)?;
        if seg.truncated {
            truncated += 1;
        }
        for e in &seg.events {
            if first_t.is_none() {
                first_t = Some(e.t_ns);
            }
            // QPC is monotonic; a regression means a bug in the capture path.
            if e.t_ns < last_t {
                violations += 1;
            }
            last_t = e.t_ns;
            found += 1;
        }
    }

    let duration_s = match first_t {
        Some(f) if last_t > f => (last_t - f) as f64 / 1e9,
        _ => 0.0,
    };

    Ok(VerifyReport {
        session_id: header.session_id,
        events_found: found,
        events_claimed: header.events_written,
        dropped_claimed: header.events_dropped,
        truncated_segments: truncated,
        clean_shutdown: header.clean_shutdown,
        interrupted: header.interrupted,
        monotonic_violations: violations,
        duration_s,
    })
}
