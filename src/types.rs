use serde::{Deserialize, Serialize};
use std::time::{SystemTime, UNIX_EPOCH};

/// Index into the channel table built from channels.toml. Stable for one run.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct ChannelId(pub u32);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SampleType {
    Float,
    Int,
    Bool,
    Text,
}

/// Logical value at API boundaries. Never stored per-slot in the ring.
#[derive(Debug, Clone, PartialEq)]
pub enum Sample {
    Float(f64),
    Int(i64),
    Bool(bool),
    Text(String),
}

impl Sample {
    pub fn sample_type(&self) -> SampleType {
        match self {
            Sample::Float(_) => SampleType::Float,
            Sample::Int(_) => SampleType::Int,
            Sample::Bool(_) => SampleType::Bool,
            Sample::Text(_) => SampleType::Text,
        }
    }
}

/// Copy-only numeric value for the ingest hot path.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum NumericVal {
    Float(f64),
    Int(i64),
    Bool(bool),
}

impl NumericVal {
    pub fn as_f64(&self) -> f64 {
        match self {
            NumericVal::Float(v) => *v,
            NumericVal::Int(v) => *v as f64,
            NumericVal::Bool(b) => u8::from(*b) as f64,
        }
    }

    pub fn sample_type(&self) -> SampleType {
        match self {
            NumericVal::Float(_) => SampleType::Float,
            NumericVal::Int(_) => SampleType::Int,
            NumericVal::Bool(_) => SampleType::Bool,
        }
    }
}

/// Half-open time range [start_ns, end_ns) in ns since Unix epoch.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TimeWindow {
    pub start_ns: i64,
    pub end_ns: i64,
}

impl TimeWindow {
    pub fn contains(&self, ts: i64) -> bool {
        ts >= self.start_ns && ts < self.end_ns
    }

    /// Window covering the last `duration_ns` ending at `now_ns`.
    pub fn last(duration_ns: i64, now_ns: i64) -> Self {
        Self { start_ns: now_ns - duration_ns, end_ns: now_ns }
    }
}

/// Display-side channel metadata (EU scale/offset stay in ChannelConfig —
/// they are consumed on ingest, panels only ever see scaled values).
#[derive(Debug, Clone)]
pub struct ChannelMeta {
    pub name: String,
    pub sample_type: SampleType,
    pub unit: String,
    pub color: String,
    pub max_rate: u32,
    pub history_s: f64,
    pub max_lines: usize,
    /// For `Text` channels only: store values as interned integer codes in a
    /// rate×history ring (the waveform buffer) rather than a fixed-line
    /// `TextBuf`. Set for discovered channels (state/status fields), so a
    /// state graph keeps full transition history at high sample rates. Left
    /// false for config-declared text channels, which are logs and must keep
    /// every line verbatim.
    pub text_coded: bool,
}

/// Owned copy of a channel's samples within a window, SoA layout.
#[derive(Debug, Clone)]
pub enum ChannelSnapshot {
    Float { ts: Vec<i64>, vals: Vec<f64> },
    Int { ts: Vec<i64>, vals: Vec<i64> },
    Bool { ts: Vec<i64>, vals: Vec<u8> },
    Text { lines: Vec<(i64, String)> },
}

impl ChannelSnapshot {
    pub fn len(&self) -> usize {
        match self {
            ChannelSnapshot::Float { ts, .. } => ts.len(),
            ChannelSnapshot::Int { ts, .. } => ts.len(),
            ChannelSnapshot::Bool { ts, .. } => ts.len(),
            ChannelSnapshot::Text { lines } => lines.len(),
        }
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

/// Wall clock as i64 ns since Unix epoch.
pub fn now_ns() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("system clock before Unix epoch")
        .as_nanos() as i64
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sample_reports_its_type() {
        assert_eq!(Sample::Float(1.0).sample_type(), SampleType::Float);
        assert_eq!(Sample::Int(3).sample_type(), SampleType::Int);
        assert_eq!(Sample::Bool(true).sample_type(), SampleType::Bool);
        assert_eq!(Sample::Text("x".into()).sample_type(), SampleType::Text);
    }

    #[test]
    fn numeric_val_as_f64() {
        assert_eq!(NumericVal::Float(1.5).as_f64(), 1.5);
        assert_eq!(NumericVal::Int(3).as_f64(), 3.0);
        assert_eq!(NumericVal::Bool(true).as_f64(), 1.0);
        assert_eq!(NumericVal::Bool(false).as_f64(), 0.0);
    }

    #[test]
    fn time_window_start_inclusive_end_exclusive() {
        let w = TimeWindow { start_ns: 10, end_ns: 20 };
        assert!(w.contains(10));
        assert!(w.contains(19));
        assert!(!w.contains(20));
        assert!(!w.contains(9));
        assert_eq!(TimeWindow::last(5, 20), TimeWindow { start_ns: 15, end_ns: 20 });
    }

    #[test]
    fn sample_type_deserializes_lowercase_from_toml() {
        #[derive(serde::Deserialize)]
        struct W { t: SampleType }
        let w: W = toml::from_str(r#"t = "float""#).unwrap();
        assert_eq!(w.t, SampleType::Float);
        let w: W = toml::from_str(r#"t = "text""#).unwrap();
        assert_eq!(w.t, SampleType::Text);
    }

    #[test]
    fn snapshot_len() {
        let s = ChannelSnapshot::Float { ts: vec![1, 2], vals: vec![0.1, 0.2] };
        assert_eq!(s.len(), 2);
        assert!(!s.is_empty());
        let s = ChannelSnapshot::Text { lines: vec![] };
        assert!(s.is_empty());
    }
}
