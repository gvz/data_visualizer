use std::cell::UnsafeCell;
use std::sync::atomic::{AtomicU64, Ordering};

use crate::types::TimeWindow;

/// Lock-free single-producer / multi-reader ring with timestamps and values
/// in parallel arrays (SoA). `head` counts total samples ever pushed; slot
/// index is `seq & (cap - 1)`.
///
/// Readers copy optimistically and validate against `head` afterwards
/// (seqlock style): if the producer wrote far enough to have overwritten any
/// slot the reader touched, the reader discards and retries. The newest
/// `cap/8` slots are reserved as an overwrite guard so a reader is never
/// chasing the producer's write position slot-by-slot.
pub struct SoaRing<T: Copy> {
    cap: usize,
    margin: u64,
    ts: Box<[UnsafeCell<i64>]>,
    vals: Box<[UnsafeCell<T>]>,
    head: AtomicU64,
}

// Safety: readers only dereference slots they subsequently validate against
// `head`; invalid (possibly torn) copies are discarded before use. Only one
// thread ever calls `push` (single-producer contract), so there is no
// write-write race on any slot.
unsafe impl<T: Copy + Send> Send for SoaRing<T> {}
unsafe impl<T: Copy + Send> Sync for SoaRing<T> {}

impl<T: Copy + Default> SoaRing<T> {
    /// `min_capacity` rounds up to a power of two, minimum 16.
    pub fn new(min_capacity: usize) -> Self {
        let cap = min_capacity.max(16).next_power_of_two();
        let ts = (0..cap)
            .map(|_| UnsafeCell::new(0i64))
            .collect::<Vec<_>>()
            .into_boxed_slice();
        let vals = (0..cap)
            .map(|_| UnsafeCell::new(T::default()))
            .collect::<Vec<_>>()
            .into_boxed_slice();
        Self { cap, margin: (cap / 8) as u64, ts, vals, head: AtomicU64::new(0) }
    }

    pub fn capacity(&self) -> usize {
        self.cap
    }

    /// Samples a reader is guaranteed to be able to snapshot.
    pub fn visible_capacity(&self) -> usize {
        self.cap - self.margin as usize
    }

    #[inline]
    fn slot(&self, seq: u64) -> usize {
        (seq as usize) & (self.cap - 1)
    }

    /// Single producer only. Lock-free, allocation-free.
    ///
    /// Writes ts then val into the slot, then advances `head` with Release
    /// ordering so readers see a consistent snapshot once they load `head`
    /// with Acquire.
    pub fn push(&self, ts: i64, val: T) {
        let head = self.head.load(Ordering::Relaxed);
        let idx = self.slot(head);
        // Safety: single producer — no concurrent writer; readers validate
        // via `head` and discard any torn copies before using them.
        unsafe {
            *self.ts[idx].get() = ts;
            *self.vals[idx].get() = val;
        }
        self.head.store(head + 1, Ordering::Release);
    }

    /// Returns the most recently pushed (ts, val), or None if the ring is
    /// empty. Retries if the producer laps the slot during the read.
    pub fn latest(&self) -> Option<(i64, T)> {
        loop {
            let head = self.head.load(Ordering::Acquire);
            if head == 0 {
                return None;
            }
            let seq = head - 1;
            let idx = self.slot(seq);
            // Safety: see module-level safety comment; we validate head2 below.
            let ts = unsafe { *self.ts[idx].get() };
            let val = unsafe { *self.vals[idx].get() };
            let head2 = self.head.load(Ordering::Acquire);
            // Slot `seq` is dirty once the producer starts writing seq+cap,
            // i.e. head2 > seq + (cap - 1), equivalently head2 >= seq + cap.
            if head2 < seq + self.cap as u64 {
                return Some((ts, val));
            }
            // Producer lapped — retry.
        }
    }

    /// Copies all samples with ts in [window.start_ns, window.end_ns) into
    /// the output vectors (cleared first), oldest first. Assumes timestamps
    /// were pushed in non-decreasing order. Retries if lapped by producer.
    pub fn read_window(&self, window: TimeWindow, out_ts: &mut Vec<i64>, out_vals: &mut Vec<T>) {
        loop {
            out_ts.clear();
            out_vals.clear();

            let head = self.head.load(Ordering::Acquire);
            if head == 0 {
                return;
            }

            // Visible window: [valid_lo, head). The margin ensures we never
            // read a slot the producer is actively writing into.
            let visible_cap = self.cap as u64 - self.margin;
            let valid_lo = head.saturating_sub(visible_cap);

            // Binary search for the first seq whose timestamp >= window.start_ns.
            let (mut lo, mut hi) = (valid_lo, head);
            while lo < hi {
                let mid = lo + (hi - lo) / 2;
                // Safety: see module-level safety comment; validated after loop.
                let ts = unsafe { *self.ts[self.slot(mid)].get() };
                if ts < window.start_ns {
                    lo = mid + 1;
                } else {
                    hi = mid;
                }
            }

            // Linear scan forward from `lo` until ts >= end_ns.
            for seq in lo..head {
                // Safety: see module-level safety comment; validated after loop.
                let ts = unsafe { *self.ts[self.slot(seq)].get() };
                if ts >= window.end_ns {
                    break;
                }
                out_ts.push(ts);
                out_vals.push(unsafe { *self.vals[self.slot(seq)].get() });
            }

            // Validate: every slot we may have touched has seq >= valid_lo.
            // Those slots stay clean as long as head hasn't advanced past
            // valid_lo + cap (which would begin overwriting slot valid_lo).
            let head2 = self.head.load(Ordering::Acquire);
            if head2 < valid_lo + self.cap as u64 {
                return;
            }
            // Lapped mid-read — discard and retry.
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::TimeWindow;
    use std::sync::Arc;

    const ALL: TimeWindow = TimeWindow { start_ns: i64::MIN, end_ns: i64::MAX };

    #[test]
    fn empty_ring_reads_nothing() {
        let r: SoaRing<f64> = SoaRing::new(16);
        assert_eq!(r.latest(), None);
        let (mut ts, mut vals) = (Vec::new(), Vec::new());
        r.read_window(ALL, &mut ts, &mut vals);
        assert!(ts.is_empty() && vals.is_empty());
    }

    #[test]
    fn push_then_read_all_in_order() {
        let r: SoaRing<f64> = SoaRing::new(256);
        for i in 0..100i64 {
            r.push(i, i as f64);
        }
        let (mut ts, mut vals) = (Vec::new(), Vec::new());
        r.read_window(ALL, &mut ts, &mut vals);
        assert_eq!(ts.len(), 100);
        assert_eq!(ts[0], 0);
        assert_eq!(ts[99], 99);
        assert_eq!(vals[99], 99.0);
    }

    #[test]
    fn window_selects_subrange() {
        let r: SoaRing<i64> = SoaRing::new(256);
        for i in 0..100i64 {
            r.push(i, i * 10);
        }
        let (mut ts, mut vals) = (Vec::new(), Vec::new());
        r.read_window(TimeWindow { start_ns: 25, end_ns: 75 }, &mut ts, &mut vals);
        assert_eq!(ts.first(), Some(&25));
        assert_eq!(ts.last(), Some(&74)); // end exclusive
        assert_eq!(vals.first(), Some(&250));
    }

    #[test]
    fn wraparound_keeps_newest_visible_capacity() {
        let r: SoaRing<i64> = SoaRing::new(64); // cap 64, margin 8 → visible 56
        assert_eq!(r.capacity(), 64);
        assert_eq!(r.visible_capacity(), 56);
        for i in 0..300i64 {
            r.push(i, i);
        }
        let (mut ts, mut vals) = (Vec::new(), Vec::new());
        r.read_window(ALL, &mut ts, &mut vals);
        assert_eq!(ts.len(), 56);
        assert_eq!(*ts.last().unwrap(), 299);
        assert_eq!(*ts.first().unwrap(), 299 - 55);
        for w in ts.windows(2) {
            assert_eq!(w[1], w[0] + 1);
        }
    }

    #[test]
    fn latest_returns_last_pushed() {
        let r: SoaRing<f64> = SoaRing::new(16);
        r.push(5, 1.25);
        r.push(9, 2.5);
        assert_eq!(r.latest(), Some((9, 2.5)));
    }

    #[test]
    fn concurrent_producer_reader_no_torn_reads() {
        let ring = Arc::new(SoaRing::<f64>::new(4096));
        let producer = {
            let ring = ring.clone();
            std::thread::spawn(move || {
                for i in 0..1_000_000i64 {
                    ring.push(i, i as f64);
                }
            })
        };
        let (mut ts, mut vals) = (Vec::new(), Vec::new());
        while !producer.is_finished() {
            ring.read_window(ALL, &mut ts, &mut vals);
            for (i, (&t, &v)) in ts.iter().zip(vals.iter()).enumerate() {
                assert_eq!(v, t as f64, "torn ts/val pair at index {i}");
                if i > 0 {
                    assert!(t == ts[i - 1] + 1, "gap or reorder inside snapshot");
                }
            }
        }
        producer.join().unwrap();
        ring.read_window(ALL, &mut ts, &mut vals);
        assert_eq!(*ts.last().unwrap(), 999_999);
    }
}
