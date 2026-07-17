/// Downsample for plotting: at most ~2 points per bucket (the bucket's min and
/// max, in timestamp order), so the drawn envelope matches the raw data.
/// X output is seconds relative to `t0` (raw ns as f64 loses precision).
/// Input shorter than 2×max_buckets passes through unchanged.
pub fn decimate_minmax(ts: &[i64], vals: &[f64], t0: i64, max_buckets: usize) -> Vec<[f64; 2]> {
    debug_assert_eq!(ts.len(), vals.len());
    let x = |t: i64| (t - t0) as f64 / 1e9;
    if max_buckets == 0 || ts.is_empty() {
        return Vec::new();
    }
    if ts.len() <= 2 * max_buckets {
        return ts.iter().zip(vals).map(|(&t, &v)| [x(t), v]).collect();
    }
    let bucket = ts.len().div_ceil(max_buckets);
    let mut out = Vec::with_capacity(2 * max_buckets + 2);
    let mut start = 0;
    while start < ts.len() {
        let end = (start + bucket).min(ts.len());
        let (mut imin, mut imax) = (start, start);
        for i in start..end {
            if vals[i] < vals[imin] {
                imin = i;
            }
            if vals[i] > vals[imax] {
                imax = i;
            }
        }
        let (a, b) = if imin <= imax { (imin, imax) } else { (imax, imin) };
        out.push([x(ts[a]), vals[a]]);
        if b != a {
            out.push([x(ts[b]), vals[b]]);
        }
        start = end;
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn small_input_passes_through() {
        let ts = [0i64, 1_000_000_000, 2_000_000_000];
        let vals = [1.0, 2.0, 3.0];
        let out = decimate_minmax(&ts, &vals, 0, 100);
        assert_eq!(out, vec![[0.0, 1.0], [1.0, 2.0], [2.0, 3.0]]);
    }

    #[test]
    fn envelope_preserved_on_large_input() {
        // 10k samples of a sine with a spike; decimated output must still
        // contain the global min and max.
        let n = 10_000;
        let mut ts = Vec::with_capacity(n);
        let mut vals = Vec::with_capacity(n);
        for i in 0..n {
            ts.push(i as i64 * 1_000_000);
            vals.push((i as f64 * 0.01).sin());
        }
        vals[7777] = 99.0; // spike
        vals[3333] = -99.0;
        let out = decimate_minmax(&ts, &vals, 0, 500);
        assert!(out.len() <= 2 * 500 + 2);
        let ys: Vec<f64> = out.iter().map(|p| p[1]).collect();
        assert!(ys.contains(&99.0), "max spike lost");
        assert!(ys.contains(&-99.0), "min spike lost");
        // x monotonically non-decreasing
        for w in out.windows(2) {
            assert!(w[1][0] >= w[0][0]);
        }
    }

    #[test]
    fn empty_and_zero_buckets() {
        assert!(decimate_minmax(&[], &[], 0, 100).is_empty());
        assert!(decimate_minmax(&[1], &[1.0], 0, 0).is_empty());
    }
}
