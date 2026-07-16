use std::collections::VecDeque;
use std::sync::Mutex;

use crate::types::TimeWindow;

/// Bounded text-channel buffer. Low rate by design — a mutex is fine here
/// and keeps String allocation off the numeric hot path.
pub struct TextBuf {
    max_lines: usize,
    lines: Mutex<VecDeque<(i64, String)>>,
}

impl TextBuf {
    pub fn new(max_lines: usize) -> Self {
        Self { max_lines: max_lines.max(1), lines: Mutex::new(VecDeque::new()) }
    }

    pub fn push(&self, ts: i64, line: String) {
        let mut lines = self.lines.lock().unwrap();
        if lines.len() == self.max_lines {
            lines.pop_front();
        }
        lines.push_back((ts, line));
    }

    pub fn window(&self, w: TimeWindow) -> Vec<(i64, String)> {
        self.lines
            .lock()
            .unwrap()
            .iter()
            .filter(|(ts, _)| w.contains(*ts))
            .cloned()
            .collect()
    }

    pub fn latest(&self) -> Option<(i64, String)> {
        self.lines.lock().unwrap().back().cloned()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::TimeWindow;

    #[test]
    fn push_window_latest() {
        let t = TextBuf::new(10);
        t.push(1, "a".into());
        t.push(5, "b".into());
        t.push(9, "c".into());
        assert_eq!(t.latest(), Some((9, "c".to_string())));
        let w = t.window(TimeWindow { start_ns: 2, end_ns: 9 });
        assert_eq!(w, vec![(5, "b".to_string())]);
    }

    #[test]
    fn bounded_drops_oldest() {
        let t = TextBuf::new(3);
        for i in 0..5i64 {
            t.push(i, format!("line{i}"));
        }
        let w = t.window(TimeWindow { start_ns: i64::MIN, end_ns: i64::MAX });
        assert_eq!(w.len(), 3);
        assert_eq!(w[0].0, 2);
        assert_eq!(w[2].0, 4);
    }
}
