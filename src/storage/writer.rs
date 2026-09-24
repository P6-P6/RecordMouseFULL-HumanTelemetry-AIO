//! The writer thread: ring -> compressed segments, context -> JSONL.
//!
//! Everything slow lives here. This thread is allowed to allocate, block on
//! disk, call into Win32 to resolve window handles, and stall for tens of
//! milliseconds; the capture thread is insulated from all of it by the ring.
//!
//! It also owns session lifecycle, because sessions roll over *during* a run
//! (spec section 10: user asks for a new one, the machine wakes from sleep, the
//! mouse changes). Putting that here means a rollover is just "finish this
//! header, open the next" without the capture thread ever noticing.
//!
//! Health accounting lives here too. Spec section 43 is blunt about it -- never
//! hide data loss -- so dropped counts go into the session header *and* get
//! sampled into the context timeline, which means a loss shows up at the point
//! in time it happened rather than only as a number at the end.

use crate::clock::Clock;
use crate::context::{foreground, ContextReceiver, ContextRecord};
use crate::event::Event;
use crate::input::raw_input::CaptureState;
use crate::ring::Ring;
use crate::storage::segment::{SegmentWriter, FRAME_EVENTS, FRAME_INTERVAL_MS};
use crate::storage::session::Session;

use std::io::{BufWriter, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// Poll interval. The ring holds ~65 s at 1 kHz, so this is latency to disk,
/// not a risk of loss. 20 ms keeps the thread near-idle on battery.
const POLL_MS: u64 = 20;
/// How often health is sampled into the context timeline and the header saved.
const HEALTH_INTERVAL: Duration = Duration::from_secs(30);

/// How often buffered data is forced all the way onto the physical disk.
///
/// Frames are written and `flush`ed every second already, which protects
/// against the *process* dying. It does not protect against the *machine*
/// dying: a flush only hands bytes to the Windows cache, and an abrupt power
/// cut loses whatever the OS had not written back yet -- potentially tens of
/// seconds.
///
/// `sync_all` is the real durability barrier. Once a minute costs one fsync on
/// a file measured in kilobytes, which is nothing next to bounding worst-case
/// loss from "however long Windows felt like caching" to 60 seconds.
const SYNC_INTERVAL: Duration = Duration::from_secs(60);

/// How old a session must be before an *automatic* rollover may replace it.
///
/// Without this, waking from sleep produced a session containing seven events
/// and lasting one second, because the hourly and resume triggers both fired.
/// A user-requested "Save + New Session" ignores this.
const MIN_SESSION_LIFETIME: Duration = Duration::from_secs(30);
/// Roll to a new segment past this size, so no single file grows unbounded.
const ROTATE_BYTES: u64 = 256 * 1024 * 1024;

#[derive(Clone)]
pub struct WriterConfig {
    /// Spec section 47: window titles are the most sensitive thing this
    /// program can see, so capturing them is opt-in and the switch is honoured
    /// at the point of collection -- not read then discarded.
    pub capture_window_titles: bool,
}

impl Default for WriterConfig {
    fn default() -> Self {
        Self { capture_window_titles: false }
    }
}

/// Asked of the writer from the UI thread.
pub enum WriterCmd {
    /// Close the current session cleanly and start a fresh one.
    NewSession(String),
}

/// How many of the most recent events are kept for diagnostics.
///
/// The raw-events window only shows the last `RECENT_SHOWN`, but the report-rate
/// median is computed over the whole buffer: a 200-event window is small enough
/// that one slow patch of movement drags the median from 1 ms to 8 ms, which
/// made the dashboard disagree with the stored data. 2000 events is ~64 KB and
/// gives a median that holds still.
pub const RECENT_CAPACITY: usize = 2000;
/// How many of those the raw-events window displays.
pub const RECENT_SHOWN: usize = 200;
/// Upper bound on distinct device slots tracked for attribution.
pub const MAX_DEVICES: usize = 16;

/// Live state, for the tray and status window to read without touching the
/// writer's internals.
#[derive(Default)]
pub struct SharedStatus {
    pub session_id: Mutex<String>,
    pub session_dir: Mutex<PathBuf>,
    pub events_written: AtomicU64,
    pub events_dropped: AtomicU64,
    pub writer_errors: AtomicU64,
    pub session_start_unix_ms: AtomicU64,
    pub sessions_started: AtomicU64,
    pub last_write_latency_us: AtomicU64,
    pub last_batch_size: AtomicU64,
    /// Events attributed to each device-table index, so the UI can show which
    /// physical mouse is actually producing data rather than merely which ones
    /// Windows can see.
    pub per_device: [AtomicU64; MAX_DEVICES],
    /// Tail of the event stream, for the raw-events window. Populated by the
    /// writer, which already has the events in hand -- the capture thread is
    /// never asked to do this.
    pub recent: Mutex<std::collections::VecDeque<Event>>,
}

impl SharedStatus {
    fn adopt(&self, s: &Session) {
        if let Ok(mut g) = self.session_id.lock() {
            *g = s.header.session_id.clone();
        }
        if let Ok(mut g) = self.session_dir.lock() {
            *g = s.dir.clone();
        }
        self.session_start_unix_ms.store(s.header.start_unix_ms, Ordering::Relaxed);
        self.sessions_started.fetch_add(1, Ordering::Relaxed);
    }

    pub fn session_id(&self) -> String {
        self.session_id.lock().map(|g| g.clone()).unwrap_or_default()
    }

    pub fn session_dir(&self) -> PathBuf {
        self.session_dir.lock().map(|g| g.clone()).unwrap_or_default()
    }
}

pub struct WriterHandle {
    stop: Arc<AtomicBool>,
    cmd_tx: std::sync::mpsc::Sender<WriterCmd>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl WriterHandle {
    /// Ask for a session rollover. Non-blocking.
    pub fn new_session(&self, reason: &str) {
        let _ = self.cmd_tx.send(WriterCmd::NewSession(reason.to_string()));
    }

    /// Signal the writer to drain and finish. Blocks until it has.
    pub fn stop(mut self) {
        self.stop.store(true, Ordering::Release);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

#[allow(clippy::too_many_arguments)]
pub fn spawn(
    root: PathBuf,
    first: Session,
    ring: Arc<Ring>,
    ctx_rx: ContextReceiver,
    state: Arc<CaptureState>,
    clock: Clock,
    cfg: WriterConfig,
) -> std::io::Result<(WriterHandle, Arc<SharedStatus>)> {
    let stop = Arc::new(AtomicBool::new(false));
    let status = Arc::new(SharedStatus::default());
    let (cmd_tx, cmd_rx) = std::sync::mpsc::channel();

    let s = Arc::clone(&stop);
    let st = Arc::clone(&status);
    let thread = std::thread::Builder::new()
        .name("writer".into())
        .spawn(move || run(root, first, ring, ctx_rx, cmd_rx, state, clock, cfg, s, st))?;

    Ok((WriterHandle { stop, cmd_tx, thread: Some(thread) }, status))
}

/// One session's open files and running totals.
struct Active {
    session: Session,
    seg: SegmentWriter,
    seg_index: u32,
    ctx_out: Option<BufWriter<std::fs::File>>,
    /// Events committed to segments already rotated away.
    carried_events: u64,
    /// Ring counter values when this session began, so per-session totals are
    /// differences rather than the process-wide running count.
    base_reports: u64,
    base_dropped: u64,
}

impl Active {
    fn open(session: Session, base_reports: u64, base_dropped: u64) -> std::io::Result<Self> {
        let seg = SegmentWriter::create(&session.segment_path(0))?;
        let mut session = session;
        session.header.segments.push(seg_name(seg.path()));
        let ctx_out = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(session.context_path())
            .ok()
            .map(BufWriter::new);
        Ok(Self {
            session,
            seg,
            seg_index: 0,
            ctx_out,
            carried_events: 0,
            base_reports,
            base_dropped,
        })
    }

    fn events_written(&self) -> u64 {
        self.carried_events + self.seg.events_written()
    }

    fn close(
        mut self,
        reports_now: u64,
        dropped_now: u64,
        peak: u64,
        max_lat: u64,
        drop_range: Option<(u64, u64)>,
    ) -> Session {
        if let Err(e) = self.seg.sync() {
            eprintln!("[writer] final sync failed: {e}");
            self.session.header.writer_errors += 1;
        }
        if let Some(mut o) = self.ctx_out.take() {
            let _ = o.flush();
            let _ = o.get_ref().sync_all();
        }
        self.session.header.reports_received = reports_now.saturating_sub(self.base_reports);
        self.session.header.events_written = self.events_written();
        self.session.header.events_dropped = dropped_now.saturating_sub(self.base_dropped);
        self.session.header.first_drop_t_ns = drop_range.map(|(f, _)| f);
        self.session.header.last_drop_t_ns = drop_range.map(|(_, l)| l);
        self.session.header.peak_queue_depth = peak;
        self.session.header.max_write_latency_us = max_lat;
        if let Err(e) = self.session.finish() {
            eprintln!("[writer] could not finalise session header: {e}");
        }
        self.session
    }
}

#[allow(clippy::too_many_arguments)]
fn run(
    root: PathBuf,
    first: Session,
    ring: Arc<Ring>,
    ctx_rx: ContextReceiver,
    cmd_rx: std::sync::mpsc::Receiver<WriterCmd>,
    state: Arc<CaptureState>,
    clock: Clock,
    cfg: WriterConfig,
    stop: Arc<AtomicBool>,
    status: Arc<SharedStatus>,
) {
    let reports_now = || state.reports.load(Ordering::Relaxed);

    let mut active = match Active::open(first, 0, 0) {
        Ok(a) => a,
        Err(e) => {
            eprintln!("[writer] cannot open session: {e}");
            return;
        }
    };
    status.adopt(&active.session);

    let mut scratch = vec![Event::default(); 8192];
    let mut pending: Vec<Event> = Vec::with_capacity(FRAME_EVENTS * 2);
    let mut last_frame = Instant::now();
    let mut last_health = Instant::now();
    let mut last_sync = Instant::now();
    let mut session_age = Instant::now();
    let mut max_latency_us: u64 = 0;

    // Sessions roll over on the local clock hour. Two reasons: it bounds how
    // much any single session can lose or contain, and it means every session
    // carries one time-of-day label, so "does this person move differently
    // late at night" is a group-by rather than a windowing problem.
    let mut session_hour = {
        let off = crate::clock::local_offset_minutes();
        crate::clock::local_hour(crate::clock::unix_millis(), off)
    };

    loop {
        let finishing = stop.load(Ordering::Acquire);

        // ---- events ------------------------------------------------------
        loop {
            let n = ring.drain(&mut scratch);
            if n == 0 {
                break;
            }
            pending.extend_from_slice(&scratch[..n]);
            if pending.len() >= FRAME_EVENTS * 4 {
                break;
            }
        }

        let due = pending.len() >= FRAME_EVENTS
            || last_frame.elapsed() >= Duration::from_millis(FRAME_INTERVAL_MS);
        if !pending.is_empty() && (due || finishing) {
            let t0 = Instant::now();
            match active.seg.write_frame(&pending) {
                Ok(()) => {
                    if let Err(e) = active.seg.flush() {
                        eprintln!("[writer] flush failed: {e}");
                        active.session.header.writer_errors += 1;
                    }
                }
                Err(e) => {
                    eprintln!("[writer] frame write failed: {e}");
                    active.session.header.writer_errors += 1;
                }
            }
            let lat = t0.elapsed().as_micros() as u64;
            max_latency_us = max_latency_us.max(lat);
            status.last_write_latency_us.store(lat, Ordering::Relaxed);
            status.last_batch_size.store(pending.len() as u64, Ordering::Relaxed);

            // Attribution and the diagnostics tail, both cheap and both only
            // possible here where the batch is already materialised.
            for e in &pending {
                if let Some(slot) = status.per_device.get(e.dev as usize) {
                    slot.fetch_add(1, Ordering::Relaxed);
                }
            }
            if let Ok(mut q) = status.recent.lock() {
                for e in &pending {
                    if q.len() == RECENT_CAPACITY {
                        q.pop_front();
                    }
                    q.push_back(*e);
                }
            }
            pending.clear();
            last_frame = Instant::now();

            if active.seg.bytes_written() >= ROTATE_BYTES {
                rotate_segment(&mut active);
            }
        }

        // ---- context -----------------------------------------------------
        let mut wake_rotate = false;
        while let Ok(rec) = ctx_rx.try_recv() {
            let resolved = match rec {
                ContextRecord::Foreground { t_ns, hwnd, .. } => {
                    // Blocking Win32 work, safely off the capture thread.
                    let (pid, exe, title) =
                        foreground::resolve(hwnd as _, cfg.capture_window_titles);
                    ContextRecord::Foreground { t_ns, hwnd, pid, exe, title }
                }
                other => other,
            };
            match &resolved {
                ContextRecord::DeviceChange { devices, .. } => {
                    active.session.header.devices = devices.clone();
                }
                ContextRecord::DisplayChange { monitors, .. } => {
                    active.session.header.monitors = monitors.clone();
                }
                ContextRecord::Power { event, .. } => {
                    // Spec section 10: resuming from sleep is a session
                    // boundary.
                    //
                    // Note QPC *does* keep advancing while suspended on many
                    // machines -- measured here as a clean 41.6-hour jump in
                    // t_ns across one sleep -- so the timestamps either side
                    // remain comparable and nothing is corrupted. The split is
                    // for analysis convenience: it keeps a multi-day gap out
                    // of the middle of a session, and gives the far side its
                    // own time-of-day label.
                    if event.starts_with("resume") {
                        wake_rotate = true;
                    }
                }
                _ => {}
            }
            write_context(&mut active, &resolved);
        }

        // ---- durability --------------------------------------------------
        if last_sync.elapsed() >= SYNC_INTERVAL {
            if let Err(e) = active.seg.sync() {
                eprintln!("[writer] periodic sync failed: {e}");
                active.session.header.writer_errors += 1;
            }
            if let Some(o) = active.ctx_out.as_mut() {
                let _ = o.flush();
                let _ = o.get_ref().sync_all();
            }
            last_sync = Instant::now();
        }

        // ---- rotation ----------------------------------------------------
        let now_hour = {
            let off = crate::clock::local_offset_minutes();
            crate::clock::local_hour(crate::clock::unix_millis(), off)
        };

        // A user request always wins; it is an explicit instruction.
        let mut user_reason: Option<String> = None;
        while let Ok(cmd) = cmd_rx.try_recv() {
            match cmd {
                WriterCmd::NewSession(r) => user_reason = Some(r),
            }
        }

        // Resume is checked before the hour, and both are automatic.
        //
        // Waking from a long sleep trips both conditions, but the power
        // notification arrives a beat after the clock has already moved on, so
        // ordering alone is not enough -- an hourly rollover fired, then the
        // resume rollover fired a second later, leaving a stray 7-event
        // session between them. MIN_SESSION_LIFETIME suppresses the second
        // one, and any other automatic rotation that lands on a session too
        // young to be worth keeping.
        let automatic = if wake_rotate {
            Some("resume_from_sleep".to_string())
        } else if now_hour != session_hour {
            Some("hourly_rollover".to_string())
        } else {
            None
        };

        let rotate_reason = match (user_reason, automatic) {
            (Some(u), _) => Some(u),
            (None, Some(a)) if session_age.elapsed() >= MIN_SESSION_LIFETIME => Some(a),
            (None, Some(_)) => {
                // Too young to roll. Absorb the hour change so it does not
                // retrigger every iteration until the next hour.
                session_hour = now_hour;
                None
            }
            (None, None) => None,
        };

        if let Some(reason) = rotate_reason {
            if !finishing {
                session_hour = now_hour;
                // Flush whatever is still buffered into the outgoing session
                // before closing it, so no event lands in the wrong one.
                if !pending.is_empty() {
                    let _ = active.seg.write_frame(&pending);
                    pending.clear();
                }
                let peak = ring.peak_depth();
                let old = active.close(
                    reports_now(),
                    ring.dropped(),
                    peak,
                    max_latency_us,
                    ring.drop_range(),
                );
                println!("[writer] session {} closed ({reason})", old.header.session_id);

                match Session::create_populated(&root, &reason, &clock) {
                    Ok(next) => match Active::open(next, reports_now(), ring.dropped()) {
                        Ok(a) => {
                            active = a;
                            status.adopt(&active.session);
                            max_latency_us = 0;
                            last_sync = Instant::now();
                            session_age = Instant::now();
                        }
                        Err(e) => {
                            eprintln!("[writer] cannot open replacement session: {e}");
                            return;
                        }
                    },
                    Err(e) => {
                        eprintln!("[writer] cannot create replacement session: {e}");
                        return;
                    }
                }
            }
        }

        // ---- health ------------------------------------------------------
        if last_health.elapsed() >= HEALTH_INTERVAL || finishing {
            let reports = reports_now();
            let dropped = ring.dropped();
            let written = active.events_written();

            let health = ContextRecord::Health {
                t_ns: clock.now_ns(),
                reports,
                written,
                dropped,
                queue_depth: ring.depth(),
                peak_queue_depth: ring.peak_depth(),
                write_latency_us: max_latency_us,
            };
            write_context(&mut active, &health);
            if let Some(o) = active.ctx_out.as_mut() {
                let _ = o.flush();
            }

            active.session.header.reports_received = reports.saturating_sub(active.base_reports);
            active.session.header.events_written = written;
            active.session.header.events_dropped = dropped.saturating_sub(active.base_dropped);
            if let Some((f, l)) = ring.drop_range() {
                active.session.header.first_drop_t_ns = Some(f);
                active.session.header.last_drop_t_ns = Some(l);
            }
            active.session.header.peak_queue_depth = ring.peak_depth();
            active.session.header.max_write_latency_us = max_latency_us;
            if let Err(e) = active.session.save() {
                eprintln!("[writer] header save failed: {e}");
                active.session.header.writer_errors += 1;
            }
            last_health = Instant::now();
        }

        // Publish for the UI.
        status.events_written.store(active.events_written(), Ordering::Relaxed);
        status
            .events_dropped
            .store(ring.dropped().saturating_sub(active.base_dropped), Ordering::Relaxed);
        status
            .writer_errors
            .store(active.session.header.writer_errors, Ordering::Relaxed);

        if finishing && pending.is_empty() && ring.depth() == 0 {
            break;
        }
        if !finishing {
            std::thread::sleep(Duration::from_millis(POLL_MS));
        }
    }

    let peak = ring.peak_depth();
    let dr = ring.drop_range();
    active.close(reports_now(), ring.dropped(), peak, max_latency_us, dr);
}

fn rotate_segment(active: &mut Active) {
    let _ = active.seg.sync();
    let next_index = active.seg_index + 1;
    match SegmentWriter::create(&active.session.segment_path(next_index)) {
        Ok(next) => {
            active.carried_events += active.seg.events_written();
            active.session.header.segments.push(seg_name(next.path()));
            active.seg = next;
            active.seg_index = next_index;
        }
        Err(e) => {
            eprintln!("[writer] segment rotation failed, staying put: {e}");
            active.session.header.writer_errors += 1;
        }
    }
}

fn seg_name(p: &Path) -> String {
    p.file_name().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default()
}

fn write_context(active: &mut Active, rec: &ContextRecord) {
    let Some(o) = active.ctx_out.as_mut() else { return };
    match serde_json::to_string(rec) {
        Ok(line) => {
            if writeln!(o, "{line}").is_err() {
                active.session.header.writer_errors += 1;
            }
        }
        Err(_) => active.session.header.writer_errors += 1,
    }
}
