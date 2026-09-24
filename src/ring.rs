//! Single-producer / single-consumer ring buffer.
//!
//! The point is not throughput -- 1 kHz through an uncontended mutex would be
//! fine. The point is that the producer must never *block*. If the capture
//! thread ever waits on a lock the writer holds across a disk flush, events
//! queue up in the OS message queue and their timestamps skew, which silently
//! corrupts exactly the inter-event timing the whole project is built on.
//!
//! Exactly one producer (the capture thread) and one consumer (the writer
//! thread) may touch this. That invariant is what makes the `UnsafeCell` sound.

use crate::event::Event;
use std::cell::UnsafeCell;
use std::sync::atomic::{AtomicU64, Ordering};

/// 65536 events -- about 65 s of headroom at 1 kHz. Power of two so the
/// index wrap is a mask.
pub const CAP: usize = 1 << 16;
const MASK: u64 = (CAP as u64) - 1;

pub struct Ring {
    buf: UnsafeCell<Box<[Event]>>,
    /// Producer writes, consumer reads.
    head: AtomicU64,
    /// Consumer writes, producer reads.
    tail: AtomicU64,
    dropped: AtomicU64,
    /// Timestamps of the first and last event ever dropped.
    ///
    /// A bare count tells you events vanished but not which stretch of the
    /// recording to distrust, and that is exactly what you need when it
    /// happens. Written only on the drop path, which is meant never to run.
    first_drop_ns: AtomicU64,
    last_drop_ns: AtomicU64,
    /// High-water mark of occupancy, for the health report.
    peak: AtomicU64,
}

// SAFETY: the SPSC discipline above means head is only ever mutated by the
// producer and tail only by the consumer; the buffer slot a producer writes is
// provably not one the consumer is reading, because the producer refuses to
// advance past tail + CAP.
unsafe impl Sync for Ring {}
unsafe impl Send for Ring {}

impl Ring {
    pub fn new() -> Self {
        Self {
            buf: UnsafeCell::new(vec![Event::default(); CAP].into_boxed_slice()),
            head: AtomicU64::new(0),
            tail: AtomicU64::new(0),
            dropped: AtomicU64::new(0),
            first_drop_ns: AtomicU64::new(0),
            last_drop_ns: AtomicU64::new(0),
            peak: AtomicU64::new(0),
        }
    }

    /// Producer side. Never blocks. Drops the event if the consumer has fallen
    /// a full buffer behind, and counts it so the session header can report
    /// honestly rather than pretending the data is complete.
    #[inline(always)]
    pub fn push(&self, ev: Event) {
        let head = self.head.load(Ordering::Relaxed);
        let tail = self.tail.load(Ordering::Acquire);
        let used = head.wrapping_sub(tail);
        if used >= CAP as u64 {
            self.dropped.fetch_add(1, Ordering::Relaxed);
            // Record when, not just how many. Two relaxed stores on a path
            // that should never execute.
            let _ = self.first_drop_ns.compare_exchange(
                0,
                ev.t_ns,
                Ordering::Relaxed,
                Ordering::Relaxed,
            );
            self.last_drop_ns.store(ev.t_ns, Ordering::Relaxed);
            return;
        }
        if used > self.peak.load(Ordering::Relaxed) {
            self.peak.store(used, Ordering::Relaxed);
        }
        // SAFETY: `used < CAP` proves this slot is behind the consumer and not
        // aliased. The Release store below publishes it.
        unsafe {
            let buf = &mut *self.buf.get();
            *buf.get_unchecked_mut((head & MASK) as usize) = ev;
        }
        self.head.store(head.wrapping_add(1), Ordering::Release);
    }

    /// Consumer side. Copies out up to `out.len()` events, returns the count.
    pub fn drain(&self, out: &mut [Event]) -> usize {
        let tail = self.tail.load(Ordering::Relaxed);
        let head = self.head.load(Ordering::Acquire);
        let avail = head.wrapping_sub(tail).min(out.len() as u64) as usize;
        if avail == 0 {
            return 0;
        }
        // SAFETY: slots in [tail, head) were published by the producer with a
        // Release store that this Acquire load synchronises with.
        unsafe {
            let buf = &*self.buf.get();
            for i in 0..avail {
                out[i] = *buf.get_unchecked(((tail.wrapping_add(i as u64)) & MASK) as usize);
            }
        }
        self.tail.store(tail.wrapping_add(avail as u64), Ordering::Release);
        avail
    }

    pub fn dropped(&self) -> u64 {
        self.dropped.load(Ordering::Relaxed)
    }

    /// Timestamp range over which events were lost, if any.
    pub fn drop_range(&self) -> Option<(u64, u64)> {
        let f = self.first_drop_ns.load(Ordering::Relaxed);
        if f == 0 {
            return None;
        }
        Some((f, self.last_drop_ns.load(Ordering::Relaxed)))
    }

    pub fn peak_depth(&self) -> u64 {
        self.peak.load(Ordering::Relaxed)
    }

    pub fn depth(&self) -> u64 {
        self.head
            .load(Ordering::Relaxed)
            .wrapping_sub(self.tail.load(Ordering::Relaxed))
    }
}

impl Default for Ring {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ev(t: u64) -> Event {
        Event { t_ns: t, ..Default::default() }
    }

    #[test]
    fn a_ring_that_never_drops_reports_no_range() {
        let r = Ring::new();
        for i in 0..100 {
            r.push(ev(i));
        }
        assert_eq!(r.dropped(), 0);
        assert!(r.drop_range().is_none());
    }

    #[test]
    fn drains_in_order() {
        let r = Ring::new();
        for i in 0..100 {
            r.push(ev(i));
        }
        let mut out = vec![Event::default(); 256];
        assert_eq!(r.drain(&mut out), 100);
        for i in 0..100u64 {
            assert_eq!(out[i as usize].t_ns, i);
        }
        assert_eq!(r.dropped(), 0);
    }

    #[test]
    fn counts_drops_instead_of_blocking_or_corrupting() {
        let r = Ring::new();
        for i in 0..(CAP as u64 + 500) {
            r.push(ev(i));
        }
        assert_eq!(r.dropped(), 500);

        // And records *when* the loss happened, not merely that it did.
        let (first, last) = r.drop_range().expect("a drop range must be recorded");
        assert_eq!(first, CAP as u64, "first dropped event is the one after the ring filled");
        assert_eq!(last, CAP as u64 + 499);

        // The events that did survive must still be the first CAP, in order --
        // a full ring drops new events, it does not overwrite old ones.
        let mut out = vec![Event::default(); CAP];
        assert_eq!(r.drain(&mut out), CAP);
        assert_eq!(out[0].t_ns, 0);
        assert_eq!(out[CAP - 1].t_ns, CAP as u64 - 1);
    }

    #[test]
    fn wraps_past_the_mask_boundary() {
        let r = Ring::new();
        let mut out = vec![Event::default(); 64];
        // Push/drain far enough to cross the power-of-two wrap several times.
        for round in 0..(CAP / 32 + 10) as u64 {
            for i in 0..32u64 {
                r.push(ev(round * 32 + i));
            }
            assert_eq!(r.drain(&mut out), 32);
            assert_eq!(out[0].t_ns, round * 32);
        }
        assert_eq!(r.dropped(), 0);
    }

    #[test]
    fn survives_a_real_producer_consumer_race() {
        use std::sync::Arc;
        const N: u64 = 400_000;
        let r = Arc::new(Ring::new());
        let rp = Arc::clone(&r);
        let prod = std::thread::spawn(move || {
            for i in 0..N {
                rp.push(ev(i));
            }
        });
        let mut seen = 0u64;
        let mut next = 0u64;
        let mut out = vec![Event::default(); 1024];
        while seen + r.dropped() < N {
            let n = r.drain(&mut out);
            for e in out.iter().take(n) {
                // Ordering must hold across the whole run, gaps included.
                assert!(e.t_ns >= next, "out of order: {} after {}", e.t_ns, next);
                next = e.t_ns;
            }
            seen += n as u64;
            if n == 0 {
                std::hint::spin_loop();
            }
        }
        prod.join().unwrap();
        assert_eq!(seen + r.dropped(), N);
    }
}
