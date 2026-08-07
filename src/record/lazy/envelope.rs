use crate::types::TimeWindow;

#[derive(Clone, Copy)]
struct Cell {
    any: bool,
    t_min: i64,
    v_min: f64,
    t_max: i64,
    v_max: f64,
}

impl Default for Cell {
    fn default() -> Self {
        Cell { any: false, t_min: 0, v_min: 0.0, t_max: 0, v_max: 0.0 }
    }
}

/// Per-channel decimated min/max overview. Each channel gets `buckets`
/// fixed-width time buckets spanning `[start_ns, start_ns + duration_ns]`;
/// each bucket keeps the min and max sample (with their timestamps) that fell
/// into it. Built once at load, read for near-whole-file windows.
pub struct Envelope {
    nchannels: usize,
    start_ns: i64,
    duration_ns: i64,
    buckets: usize,
    cells: Vec<Cell>,
}

fn bucket_of(start_ns: i64, duration_ns: i64, buckets: usize, ts: i64) -> usize {
    if buckets == 0 {
        return 0;
    }
    let span = (duration_ns as i128) + 1; // inclusive of the end sample
    let off = (ts as i128 - start_ns as i128).clamp(0, span - 1);
    ((off * buckets as i128) / span) as usize
}

impl Envelope {
    pub fn new(nchannels: usize, start_ns: i64, duration_ns: i64, buckets: usize) -> Self {
        let buckets = buckets.max(1);
        Self {
            nchannels,
            start_ns,
            duration_ns,
            buckets,
            cells: vec![Cell::default(); nchannels * buckets],
        }
    }

    #[inline]
    fn idx(&self, ch: usize, b: usize) -> usize {
        ch * self.buckets + b
    }

    pub fn fold_numeric(&mut self, ch: usize, ts: i64, val: f64) {
        if ch >= self.nchannels {
            return;
        }
        let b = bucket_of(self.start_ns, self.duration_ns, self.buckets, ts);
        let i = self.idx(ch, b);
        let cell = &mut self.cells[i];
        if !cell.any {
            *cell = Cell { any: true, t_min: ts, v_min: val, t_max: ts, v_max: val };
        } else {
            if val < cell.v_min {
                cell.v_min = val;
                cell.t_min = ts;
            }
            if val > cell.v_max {
                cell.v_max = val;
                cell.t_max = ts;
            }
        }
    }

    pub fn merge(&mut self, other: &Envelope) {
        for i in 0..self.cells.len().min(other.cells.len()) {
            let o = other.cells[i];
            if !o.any {
                continue;
            }
            let c = &mut self.cells[i];
            if !c.any {
                *c = o;
            } else {
                if o.v_min < c.v_min {
                    c.v_min = o.v_min;
                    c.t_min = o.t_min;
                }
                if o.v_max > c.v_max {
                    c.v_max = o.v_max;
                    c.t_max = o.t_max;
                }
            }
        }
    }

    pub fn read(&self, ch: usize, window: TimeWindow) -> Vec<(i64, f64)> {
        let mut out = Vec::new();
        if ch >= self.nchannels {
            return out;
        }
        for b in 0..self.buckets {
            let cell = self.cells[self.idx(ch, b)];
            if !cell.any {
                continue;
            }
            // Emit the two extremes in timestamp order; clip each to the window.
            let pair = if cell.t_min <= cell.t_max {
                [(cell.t_min, cell.v_min), (cell.t_max, cell.v_max)]
            } else {
                [(cell.t_max, cell.v_max), (cell.t_min, cell.v_min)]
            };
            for (t, v) in pair {
                if t >= window.start_ns && t < window.end_ns {
                    // Deduplicate a single-sample bucket (t_min == t_max, v equal).
                    if out.last() != Some(&(t, v)) {
                        out.push((t, v));
                    }
                }
            }
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::TimeWindow;

    #[test]
    fn folds_min_max_per_bucket() {
        // 4 buckets over [0, 40): each bucket is 10 wide.
        let mut e = Envelope::new(1, 0, 40, 4);
        // Bucket 0 gets samples at t=0 (v=5) and t=9 (v=1): min 1 @9, max 5 @0.
        e.fold_numeric(0, 0, 5.0);
        e.fold_numeric(0, 9, 1.0);
        // Bucket 2 gets a single sample.
        e.fold_numeric(0, 25, 7.0);
        let pts = e.read(0, TimeWindow { start_ns: i64::MIN, end_ns: i64::MAX });
        // Bucket 0 emits its two extremes in time order, bucket 2 emits one point.
        assert_eq!(pts, vec![(0, 5.0), (9, 1.0), (25, 7.0)]);
    }

    #[test]
    fn read_clips_to_window_and_skips_empty() {
        let mut e = Envelope::new(1, 0, 40, 4);
        e.fold_numeric(0, 5, 1.0); // bucket 0
        e.fold_numeric(0, 35, 2.0); // bucket 3
                                    // Window covering only the last bucket.
        let pts = e.read(0, TimeWindow { start_ns: 30, end_ns: 40 });
        assert_eq!(pts, vec![(35, 2.0)]);
    }

    #[test]
    fn merge_unions_extremes() {
        let mut a = Envelope::new(1, 0, 40, 4);
        a.fold_numeric(0, 0, 5.0);
        let mut b = Envelope::new(1, 0, 40, 4);
        b.fold_numeric(0, 1, -3.0);
        b.fold_numeric(0, 2, 9.0);
        a.merge(&b);
        let pts = a.read(0, TimeWindow { start_ns: i64::MIN, end_ns: i64::MAX });
        // Bucket 0 now spans min -3 @1 .. max 9 @2, emitted in time order.
        assert_eq!(pts, vec![(1, -3.0), (2, 9.0)]);
    }
}
