//! Sideband context, recorded on the same QPC timeline as events.
//!
//! Spec section 8 asks for foreground process, window title, monitor and
//! modifier state on every event. Doing that literally means several syscalls
//! inside the `WM_INPUT` handler at 1 kHz. Instead each of these is recorded
//! once, when it *changes*, carrying the same `t_ns` clock the events use.
//! Analysis reconstructs per-event context with an interval join -- every field
//! the spec asked for, none of the hot-path cost.
//!
//! Context records are low-rate (a few per minute at most), so they travel over
//! an ordinary `mpsc` channel and are written as JSONL: human-readable, append-
//! only, and trivially greppable when you want to know what you were doing at
//! 14:32 last Tuesday.

pub mod foreground;
pub mod monitors;

use crate::input::devices::DeviceInfo;
use monitors::MonitorInfo;
use serde::{Deserialize, Serialize};

/// One thing that changed, and when.
///
/// `hwnd` is recorded raw rather than resolved on the capture thread on
/// purpose: `GetWindowTextW` against a hung application blocks the calling
/// thread, and the capture thread is the one thread that must never block. The
/// writer thread resolves it a few milliseconds later. The timestamp is still
/// exact; only the name lookup is deferred.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ContextRecord {
    Foreground {
        t_ns: u64,
        hwnd: isize,
        /// Filled in by the writer thread.
        #[serde(skip_serializing_if = "Option::is_none")]
        pid: Option<u32>,
        #[serde(skip_serializing_if = "Option::is_none")]
        exe: Option<String>,
        /// Omitted entirely when title capture is disabled in settings.
        #[serde(skip_serializing_if = "Option::is_none")]
        title: Option<String>,
    },
    DeviceChange {
        t_ns: u64,
        devices: Vec<DeviceInfo>,
    },
    DisplayChange {
        t_ns: u64,
        monitors: Vec<MonitorInfo>,
    },
    Power {
        t_ns: u64,
        event: String,
    },
    RecordingState {
        t_ns: u64,
        state: String,
    },
    /// Periodic health sample, so data loss is visible in the timeline itself
    /// rather than only in a summary at the end.
    Health {
        t_ns: u64,
        /// WM_INPUT reports seen; see `CaptureState::reports`.
        reports: u64,
        written: u64,
        dropped: u64,
        queue_depth: u64,
        peak_queue_depth: u64,
        write_latency_us: u64,
    },
}

impl ContextRecord {
    pub fn t_ns(&self) -> u64 {
        match self {
            Self::Foreground { t_ns, .. }
            | Self::DeviceChange { t_ns, .. }
            | Self::DisplayChange { t_ns, .. }
            | Self::Power { t_ns, .. }
            | Self::RecordingState { t_ns, .. }
            | Self::Health { t_ns, .. } => *t_ns,
        }
    }
}

pub type ContextSender = std::sync::mpsc::Sender<ContextRecord>;
pub type ContextReceiver = std::sync::mpsc::Receiver<ContextRecord>;
