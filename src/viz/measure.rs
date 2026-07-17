/// Min/max/mean/RMS over a selection — the cursor-measurement numbers.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Stats {
    pub min: f64,
    pub max: f64,
    pub mean: f64,
    pub rms: f64,
    pub count: usize,
}

pub fn stats(vals: &[f64]) -> Option<Stats> {
    if vals.is_empty() {
        return None;
    }
    let mut min = f64::INFINITY;
    let mut max = f64::NEG_INFINITY;
    let mut sum = 0.0;
    let mut sq = 0.0;
    for &v in vals {
        min = min.min(v);
        max = max.max(v);
        sum += v;
        sq += v * v;
    }
    let n = vals.len() as f64;
    Some(Stats { min, max, mean: sum / n, rms: (sq / n).sqrt(), count: vals.len() })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stats_of_known_values() {
        let s = stats(&[3.0, 4.0]).unwrap();
        assert_eq!(s.min, 3.0);
        assert_eq!(s.max, 4.0);
        assert_eq!(s.mean, 3.5);
        assert!((s.rms - 12.5f64.sqrt()).abs() < 1e-12);
        assert_eq!(s.count, 2);
    }

    #[test]
    fn stats_empty_is_none() {
        assert_eq!(stats(&[]), None);
    }

    #[test]
    fn stats_single_value() {
        let s = stats(&[-2.0]).unwrap();
        assert_eq!((s.min, s.max, s.mean, s.rms), (-2.0, -2.0, -2.0, 2.0));
    }
}
