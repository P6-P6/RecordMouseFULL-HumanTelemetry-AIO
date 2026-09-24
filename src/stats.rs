//! Dataset audit: `HumanTelemetry.exe stats`.
//!
//! Answers "how much have I actually collected, and is it any good" without
//! exporting 69 megabytes of CSV and writing a throwaway script each time --
//! which is how this question got answered three times before the command
//! existed.
//!
//! Everything here is derived on the fly from the stored events. Nothing is
//! read from a precomputed field, because there are none: the recorder stores
//! raw reports and nothing else.

use crate::clock::{day_part, local_hour, local_weekday_name};
use crate::event::*;
use crate::storage::segment::read_segment;
use crate::storage::session::SessionHeader;

use std::collections::{BTreeSet, HashMap};
use std::path::Path;

/// A pause longer than this ends a movement segment.
const SEG_GAP_MS: f64 = 120.0;
/// A gap longer than this means the machine was asleep or off, not idle.
/// Counting it as recording time overstates the dataset by whole days.
const SLEEP_GAP_NS: u64 = 10 * 60 * 1_000_000_000;
/// Wheel events further apart than this belong to separate scroll bursts.
const SCROLL_GAP_MS: f64 = 400.0;

/// Distance bins from spec section 30.
const BINS: [(f64, f64, &str); 7] = [
    (0.0, 25.0, "0-25"),
    (25.0, 75.0, "25-75"),
    (75.0, 150.0, "75-150"),
    (150.0, 300.0, "150-300"),
    (300.0, 600.0, "300-600"),
    (600.0, 1200.0, "600-1200"),
    (1200.0, f64::INFINITY, "1200+"),
];

#[derive(Default)]
pub struct Stats {
    pub sessions: usize,
    pub clean: usize,
    pub interrupted: usize,
    pub in_progress: usize,
    pub bytes: u64,

    pub reports_received: u64,
    pub events_written: u64,
    pub dropped: u64,
    pub writer_errors: u64,
    pub peak_queue: u64,

    pub kinds: HashMap<u8, u64>,
    pub active_ns: u64,
    pub asleep_ns: u64,

    pub movements: u64,
    pub clicks: u64,
    pub drags: u64,
    pub scroll_bursts: u64,
    pub button_presses: HashMap<u8, u64>,

    /// Click hold durations in ms, for a median.
    holds: Vec<f64>,
    /// Inter-report gaps in ms, for the measured polling rate.
    gaps: Vec<f64>,
    /// Per-segment straight-line distance in px.
    distances: Vec<f64>,

    pub day_parts: HashMap<String, u64>,
    pub hours: BTreeSet<u8>,
    pub weekdays: BTreeSet<String>,
    pub machines: BTreeSet<String>,
    pub devices: BTreeSet<String>,

    pub first_session: String,
    pub last_session: String,
}

fn median(v: &mut [f64]) -> f64 {
    if v.is_empty() {
        return 0.0;
    }
    v.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    v[v.len() / 2]
}

pub fn collect(root: &Path) -> std::io::Result<Stats> {
    let mut s = Stats::default();
    let dir = root.join("sessions");
    if !dir.exists() {
        return Ok(s);
    }

    let mut paths: Vec<_> = std::fs::read_dir(&dir)?
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter(|p| p.is_dir() && p.join("session.json").exists())
        .collect();
    paths.sort();

    for p in &paths {
        let Ok(text) = std::fs::read_to_string(p.join("session.json")) else {
            continue;
        };
        let Ok(h) = serde_json::from_str::<SessionHeader>(&text) else {
            continue;
        };
        s.sessions += 1;
        if h.interrupted {
            s.interrupted += 1;
        } else if h.clean_shutdown {
            s.clean += 1;
        } else {
            s.in_progress += 1;
        }

        if s.first_session.is_empty() {
            s.first_session = h.start_wall_local.clone();
        }
        s.last_session = h.start_wall_local.clone();

        s.reports_received += h.reports_received;
        s.events_written += h.events_written;
        s.dropped += h.events_dropped;
        s.writer_errors += h.writer_errors;
        s.peak_queue = s.peak_queue.max(h.peak_queue_depth);

        if !h.os.computer_name.is_empty() {
            s.machines.insert(h.os.computer_name.clone());
        }
        for d in h.devices.iter().filter(|d| !d.synthetic) {
            match (d.vendor_id, d.product_id) {
                (Some(v), Some(pid)) => {
                    s.devices.insert(format!("{v:04X}:{pid:04X}"));
                }
                _ => {
                    s.devices.insert("HID".into());
                }
            }
        }

        // Time-of-day coverage comes off the header, which already stores the
        // local values -- but recompute from the anchor when a header predates
        // those fields, so old sessions still count.
        if !h.start_day_part.is_empty() {
            *s.day_parts.entry(h.start_day_part.clone()).or_default() += 1;
            s.hours.insert(h.start_local_hour);
            s.weekdays.insert(h.start_weekday.clone());
        } else if h.start_unix_ms > 0 {
            let off = crate::clock::local_offset_minutes();
            let hour = local_hour(h.start_unix_ms, off);
            *s.day_parts.entry(day_part(hour).to_string()).or_default() += 1;
            s.hours.insert(hour);
            s.weekdays.insert(local_weekday_name(h.start_unix_ms, off).to_string());
        }

        if let Ok(rd) = std::fs::read_dir(p) {
            for f in rd.filter_map(|e| e.ok()) {
                if let Ok(m) = f.metadata() {
                    s.bytes += m.len();
                }
            }
        }

        // ---- the events themselves ---------------------------------------
        let mut names: Vec<String> = h.segments.clone();
        if names.is_empty() {
            let mut found: Vec<String> = std::fs::read_dir(p)?
                .filter_map(|e| e.ok())
                .map(|e| e.file_name().to_string_lossy().into_owned())
                .filter(|n| n.ends_with(".mpseg"))
                .collect();
            found.sort();
            names = found;
        }

        let mut last_t: Option<u64> = None;
        let mut seg_first: Option<(f64, i32, i32)> = None;
        let mut seg_last: Option<(f64, i32, i32)> = None;
        let mut down_at: HashMap<u8, f64> = HashMap::new();
        let mut dragging = false;
        let mut last_wheel: Option<f64> = None;

        for name in &names {
            let path = p.join(name);
            if !path.exists() {
                continue;
            }
            let Ok(seg) = read_segment(&path) else { continue };
            for e in &seg.events {
                *s.kinds.entry(e.kind).or_default() += 1;
                let t_ms = e.t_ns as f64 / 1e6;

                if let Some(prev) = last_t {
                    let d = e.t_ns.saturating_sub(prev);
                    if d >= SLEEP_GAP_NS {
                        s.asleep_ns += d;
                    } else {
                        s.active_ns += d;
                    }
                }
                last_t = Some(e.t_ns);

                match e.kind {
                    EV_MOVE => {
                        if let Some((pt, _, _)) = seg_last {
                            let gap = t_ms - pt;
                            if gap > 0.0 && gap < 100.0 {
                                s.gaps.push(gap);
                            }
                            if gap > SEG_GAP_MS {
                                // close the previous segment
                                if let (Some((_, x0, y0)), Some((_, x1, y1))) =
                                    (seg_first, seg_last)
                                {
                                    let dx = (x1 - x0) as f64;
                                    let dy = (y1 - y0) as f64;
                                    s.distances.push((dx * dx + dy * dy).sqrt());
                                }
                                seg_first = None;
                                dragging = false;
                            }
                        }
                        if seg_first.is_none() {
                            seg_first = Some((t_ms, e.x, e.y));
                            s.movements += 1;
                        }
                        seg_last = Some((t_ms, e.x, e.y));
                        if e.bstate != 0 {
                            if !dragging {
                                s.drags += 1;
                                dragging = true;
                            }
                        } else {
                            dragging = false;
                        }
                    }
                    EV_BUTTON => {
                        if e.flags & F_DOWN != 0 {
                            *s.button_presses.entry(e.btn).or_default() += 1;
                            down_at.insert(e.btn, t_ms);
                        } else if let Some(d) = down_at.remove(&e.btn) {
                            let hold = t_ms - d;
                            if hold > 0.0 && hold < 5000.0 {
                                s.holds.push(hold);
                                s.clicks += 1;
                            }
                        }
                    }
                    EV_WHEEL => {
                        if last_wheel.map(|w| t_ms - w > SCROLL_GAP_MS).unwrap_or(true) {
                            s.scroll_bursts += 1;
                        }
                        last_wheel = Some(t_ms);
                    }
                    _ => {}
                }
            }
        }
        if let (Some((_, x0, y0)), Some((_, x1, y1))) = (seg_first, seg_last) {
            let dx = (x1 - x0) as f64;
            let dy = (y1 - y0) as f64;
            s.distances.push((dx * dx + dy * dy).sqrt());
        }
    }

    Ok(s)
}

fn human_bytes(b: u64) -> String {
    const U: [&str; 4] = ["B", "KB", "MB", "GB"];
    let mut v = b as f64;
    let mut i = 0;
    while v >= 1024.0 && i < U.len() - 1 {
        v /= 1024.0;
        i += 1;
    }
    format!("{v:.1} {}", U[i])
}

fn commas(n: u64) -> String {
    let s = n.to_string();
    let mut out = String::new();
    for (i, c) in s.chars().enumerate() {
        if i > 0 && (s.len() - i) % 3 == 0 {
            out.push(',');
        }
        out.push(c);
    }
    out
}

pub fn report(mut s: Stats) {
    let hours = s.active_ns as f64 / 3.6e12;
    let asleep = s.asleep_ns as f64 / 3.6e12;

    println!("DATASET");
    println!("  sessions            {:>12}   ({} clean, {} interrupted, {} in progress)",
             s.sessions, s.clean, s.interrupted, s.in_progress);
    if !s.first_session.is_empty() {
        println!("  first               {:>12}", s.first_session.get(..19).unwrap_or(&s.first_session));
        println!("  latest              {:>12}", s.last_session.get(..19).unwrap_or(&s.last_session));
    }
    println!("  on disk             {:>12}", human_bytes(s.bytes));
    println!("  recording time      {:>12}   (excludes {:.1} h asleep/off)",
             format!("{hours:.1} h"), asleep);

    println!("\nHEALTH");
    println!("  input reports       {:>12}", commas(s.reports_received));
    println!("  events written      {:>12}", commas(s.events_written));
    println!("  events dropped      {:>12}{}", commas(s.dropped),
             if s.dropped > 0 { "   <-- DATA LOSS" } else { "" });
    println!("  writer errors       {:>12}", commas(s.writer_errors));
    println!("  peak queue depth    {:>12} / {}", commas(s.peak_queue), crate::ring::CAP);

    println!("\nEVENTS BY KIND");
    let total: u64 = s.kinds.values().sum();
    for (k, label) in [(EV_MOVE, "move"), (EV_SYNC, "sync"), (EV_BUTTON, "button"), (EV_WHEEL, "wheel")] {
        let v = s.kinds.get(&k).copied().unwrap_or(0);
        let pct = if total > 0 { 100.0 * v as f64 / total as f64 } else { 0.0 };
        println!("  {label:<18}  {:>12}  {pct:>5.1}%", commas(v));
    }

    println!("\nBEHAVIOURAL UNITS");
    println!("  movement segments   {:>12}", commas(s.movements));
    println!("  clicks              {:>12}", commas(s.clicks));
    for (b, label) in [(B_LEFT, "left"), (B_RIGHT, "right"), (B_MIDDLE, "middle"), (B_X1, "x1"), (B_X2, "x2")] {
        let v = s.button_presses.get(&b).copied().unwrap_or(0);
        if v > 0 {
            println!("    {label:<16}  {:>12}  presses", commas(v));
        }
    }
    println!("  drags               {:>12}", commas(s.drags));
    println!("  scroll bursts       {:>12}", commas(s.scroll_bursts));

    let hold = median(&mut s.holds);
    if hold > 0.0 {
        println!("  click hold median   {:>12}", format!("{hold:.1} ms"));
    }
    let gap = median(&mut s.gaps);
    if gap > 0.0 {
        println!("  report rate         {:>12}   (median gap {:.2} ms)",
                 format!("{:.0} Hz", 1000.0 / gap), gap);
    }

    println!("\nDISTANCE COVERAGE");
    let n = s.distances.len();
    for (lo, hi, label) in BINS {
        let c = s.distances.iter().filter(|&&d| d >= lo && d < hi).count();
        let pct = if n > 0 { 100.0 * c as f64 / n as f64 } else { 0.0 };
        let bar = "#".repeat(((pct / 2.0) as usize).min(40));
        let thin = if c > 0 && c < 100 { "  thin" } else { "" };
        println!("  {label:<10} px  {c:>8}  {pct:>5.1}%  {bar}{thin}");
    }

    println!("\nTIME-OF-DAY COVERAGE");
    for d in ["morning", "afternoon", "evening", "night", "late_night"] {
        let c = s.day_parts.get(d).copied().unwrap_or(0);
        let bar = "#".repeat((c as usize).min(40));
        let gap = if c == 0 { "  <-- NO DATA" } else { "" };
        println!("  {d:<12} {c:>4}  {bar}{gap}");
    }
    println!("  hours of day        {:>2} / 24", s.hours.len());
    println!("  weekdays            {:>2} / 7    {:?}", s.weekdays.len(), s.weekdays);

    println!("\nENVIRONMENT");
    println!("  machines            {:?}", s.machines);
    println!("  mice seen           {:?}", s.devices);
}
