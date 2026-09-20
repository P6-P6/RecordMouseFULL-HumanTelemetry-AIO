//! Append-only, independently-framed event log.
//!
//! WHY NOT SQLITE FOR RAW EVENTS
//! -----------------------------
//! 1 kHz is ~86 million events per day. SQLite is the right tool for sessions,
//! device tables and derived features -- all of which want indexes and queries
//! -- and the wrong tool for an immutable firehose that is only ever appended
//! and only ever read back sequentially. Spec section 11 allows either; this is
//! option B, and it is the one that survives week-long recordings.
//!
//! WHY INDEPENDENT FRAMES, NOT A SINGLE ZSTD STREAM
//! ------------------------------------------------
//! A streaming encoder gets marginally better ratios by carrying context
//! between blocks. It also means a power cut truncates the stream mid-block and
//! everything after the last flush is unrecoverable. Each frame here is a
//! self-contained zstd blob with its own length prefix, so a torn tail costs at
//! most one batch -- a fraction of a second -- and every frame before it still
//! decodes. For a recorder meant to run for weeks unattended that trade is not
//! close.

use crate::event::{Event, EVENT_SIZE};
use std::fs::File;
use std::io::{BufWriter, Read, Write};
use std::path::{Path, PathBuf};

pub const MAGIC: &[u8; 8] = b"MPSEG\x00\x00\x01";
pub const FORMAT_VERSION: u32 = 1;
const HEADER_LEN: usize = 32;
const ZSTD_LEVEL: i32 = 3;

/// Frames are flushed when either bound is hit, whichever comes first.
pub const FRAME_EVENTS: usize = 8192; // ~8 s at 1 kHz
pub const FRAME_INTERVAL_MS: u64 = 1000;

pub struct SegmentWriter {
    out: BufWriter<File>,
    path: PathBuf,
    events_written: u64,
    bytes_written: u64,
    frames: u64,
}

impl SegmentWriter {
    pub fn create(path: &Path) -> std::io::Result<Self> {
        let file = File::create(path)?;
        let mut out = BufWriter::with_capacity(1 << 20, file);

        let mut hdr = [0u8; HEADER_LEN];
        hdr[0..8].copy_from_slice(MAGIC);
        hdr[8..12].copy_from_slice(&FORMAT_VERSION.to_le_bytes());
        hdr[12..16].copy_from_slice(&(EVENT_SIZE as u32).to_le_bytes());
        out.write_all(&hdr)?;

        Ok(Self {
            out,
            path: path.to_path_buf(),
            events_written: 0,
            bytes_written: HEADER_LEN as u64,
            frames: 0,
        })
    }

    /// Compress and append one frame. No-op for an empty batch.
    pub fn write_frame(&mut self, events: &[Event]) -> std::io::Result<()> {
        if events.is_empty() {
            return Ok(());
        }
        let mut raw = Vec::with_capacity(events.len() * EVENT_SIZE);
        for e in events {
            raw.extend_from_slice(&e.to_bytes());
        }
        let comp = zstd::bulk::compress(&raw, ZSTD_LEVEL)?;

        self.out.write_all(&(raw.len() as u32).to_le_bytes())?;
        self.out.write_all(&(comp.len() as u32).to_le_bytes())?;
        self.out.write_all(&comp)?;

        self.events_written += events.len() as u64;
        self.bytes_written += 8 + comp.len() as u64;
        self.frames += 1;
        Ok(())
    }

    /// Push buffered bytes to the OS.
    ///
    /// Note this is `flush`, not `sync_all`: it hands data to the page cache
    /// but does not force a platter/NAND commit. fsync per batch would be tens
    /// of milliseconds of stall, and the frame design already bounds crash loss
    /// to one frame. A real power cut can still cost the OS write-back window;
    /// that is an accepted, documented trade, not an oversight.
    pub fn flush(&mut self) -> std::io::Result<()> {
        self.out.flush()
    }

    /// Force a durable commit. Used at clean shutdown only.
    pub fn sync(&mut self) -> std::io::Result<()> {
        self.out.flush()?;
        self.out.get_ref().sync_all()
    }

    pub fn events_written(&self) -> u64 {
        self.events_written
    }
    pub fn bytes_written(&self) -> u64 {
        self.bytes_written
    }
    pub fn frames(&self) -> u64 {
        self.frames
    }
    pub fn path(&self) -> &Path {
        &self.path
    }
}

/// Read a segment back.
///
/// Stops cleanly at the first torn frame rather than erroring, because a
/// truncated tail is the *expected* shape of a segment from a session that
/// ended in a crash or power cut. `truncated` reports whether that happened so
/// callers never mistake partial data for complete data.
pub struct SegmentReader {
    pub events: Vec<Event>,
    pub truncated: bool,
    pub frames: u64,
}

pub fn read_segment(path: &Path) -> std::io::Result<SegmentReader> {
    let mut buf = Vec::new();
    File::open(path)?.read_to_end(&mut buf)?;

    if buf.len() < HEADER_LEN || &buf[0..8] != MAGIC {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "not a MouseProfiler segment",
        ));
    }
    let ev_size = u32::from_le_bytes(buf[12..16].try_into().unwrap()) as usize;
    if ev_size != EVENT_SIZE {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!("segment uses {ev_size}-byte events, this build expects {EVENT_SIZE}"),
        ));
    }

    let mut events = Vec::new();
    let mut pos = HEADER_LEN;
    let mut frames = 0u64;
    let mut truncated = false;

    while pos + 8 <= buf.len() {
        let raw_len = u32::from_le_bytes(buf[pos..pos + 4].try_into().unwrap()) as usize;
        let comp_len = u32::from_le_bytes(buf[pos + 4..pos + 8].try_into().unwrap()) as usize;
        pos += 8;
        if pos + comp_len > buf.len() {
            truncated = true;
            break;
        }
        let raw = match zstd::bulk::decompress(&buf[pos..pos + comp_len], raw_len) {
            Ok(r) => r,
            Err(_) => {
                truncated = true;
                break;
            }
        };
        pos += comp_len;
        for chunk in raw.chunks_exact(EVENT_SIZE) {
            events.push(Event::from_bytes(chunk));
        }
        frames += 1;
    }
    if pos < buf.len() {
        truncated = true;
    }

    Ok(SegmentReader { events, truncated, frames })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::event::{EV_MOVE, EV_SYNC};

    fn tmp(name: &str) -> PathBuf {
        let mut p = std::env::temp_dir();
        p.push(format!("mp_test_{}_{}.mpseg", name, std::process::id()));
        p
    }

    fn sample(n: u64) -> Vec<Event> {
        (0..n)
            .map(|i| Event {
                t_ns: i * 1_000_000,
                kind: if i % 100 == 0 { EV_SYNC } else { EV_MOVE },
                dx: (i as i32 % 17) - 8,
                dy: (i as i32 % 11) - 5,
                x: 500 + i as i32,
                y: 400,
                ..Default::default()
            })
            .collect()
    }

    #[test]
    fn roundtrips_every_event_through_compression() {
        let p = tmp("roundtrip");
        let evs = sample(20_000);
        {
            let mut w = SegmentWriter::create(&p).unwrap();
            for chunk in evs.chunks(FRAME_EVENTS) {
                w.write_frame(chunk).unwrap();
            }
            w.sync().unwrap();
            assert_eq!(w.events_written(), 20_000);
        }
        let r = read_segment(&p).unwrap();
        assert!(!r.truncated);
        assert_eq!(r.events.len(), 20_000);
        assert_eq!(r.events, evs);
        std::fs::remove_file(&p).ok();
    }

    #[test]
    fn a_torn_tail_costs_one_frame_not_the_file() {
        let p = tmp("torn");
        let evs = sample(20_000);
        {
            let mut w = SegmentWriter::create(&p).unwrap();
            for chunk in evs.chunks(FRAME_EVENTS) {
                w.write_frame(chunk).unwrap();
            }
            w.sync().unwrap();
        }
        // Simulate a power cut mid-frame.
        let full = std::fs::metadata(&p).unwrap().len();
        let f = std::fs::OpenOptions::new().write(true).open(&p).unwrap();
        f.set_len(full - 64).unwrap();
        drop(f);

        let r = read_segment(&p).unwrap();
        assert!(r.truncated, "truncation must be reported, never hidden");
        // Everything before the torn frame survives.
        assert!(r.events.len() >= 16_384, "lost too much: {}", r.events.len());
        assert_eq!(r.events[..r.events.len()], evs[..r.events.len()]);
        std::fs::remove_file(&p).ok();
    }

    #[test]
    fn rejects_a_foreign_file() {
        let p = tmp("foreign");
        std::fs::write(&p, b"this is not a segment file at all, not even close").unwrap();
        assert!(read_segment(&p).is_err());
        std::fs::remove_file(&p).ok();
    }

    #[test]
    fn compresses_real_looking_motion_substantially() {
        let p = tmp("ratio");
        let evs = sample(50_000);
        {
            let mut w = SegmentWriter::create(&p).unwrap();
            for chunk in evs.chunks(FRAME_EVENTS) {
                w.write_frame(chunk).unwrap();
            }
            w.sync().unwrap();
        }
        let on_disk = std::fs::metadata(&p).unwrap().len();
        let uncompressed = (evs.len() * EVENT_SIZE) as u64;
        assert!(
            on_disk * 3 < uncompressed,
            "expected >3x compression, got {uncompressed} -> {on_disk}"
        );
        std::fs::remove_file(&p).ok();
    }
}
