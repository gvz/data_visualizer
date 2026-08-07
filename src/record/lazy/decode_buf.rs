use std::sync::Mutex;

use crate::config::ChannelRegistry;
use crate::store::ChannelStore;
use crate::types::{
    ChannelId, ChannelMeta, ChannelSnapshot, NumericVal, Sample, SampleType, TimeWindow,
};

pub enum ChanSamples {
    Float { ts: Vec<i64>, vals: Vec<f64> },
    Int { ts: Vec<i64>, vals: Vec<i64> },
    Bool { ts: Vec<i64>, vals: Vec<u8> },
    Text { lines: Vec<(i64, String)> },
}

impl ChanSamples {
    fn for_type(t: SampleType) -> Self {
        match t {
            SampleType::Float => ChanSamples::Float { ts: vec![], vals: vec![] },
            SampleType::Int => ChanSamples::Int { ts: vec![], vals: vec![] },
            SampleType::Bool => ChanSamples::Bool { ts: vec![], vals: vec![] },
            SampleType::Text => ChanSamples::Text { lines: vec![] },
        }
    }
}

/// A `ChannelStore` scratch that simply collects every write into per-channel
/// typed vectors, so the existing `decode_batch`/`decode_message` path can
/// decode one chunk without a full store. Interior mutability (Mutex per
/// channel) because `ChannelStore` writes take `&self`.
pub struct ChunkDecodeBuf {
    channels: Vec<Mutex<ChanSamples>>,
    metas: Vec<ChannelMeta>,
}

impl ChunkDecodeBuf {
    pub fn new(registry: &ChannelRegistry) -> Self {
        Self {
            channels: registry
                .iter_ids()
                .map(|id| Mutex::new(ChanSamples::for_type(registry.meta(id).sample_type)))
                .collect(),
            metas: registry.iter_ids().map(|id| registry.meta(id).clone()).collect(),
        }
    }

    pub fn from_metas(metas: &[ChannelMeta]) -> Self {
        Self {
            channels: metas
                .iter()
                .map(|m| Mutex::new(ChanSamples::for_type(m.sample_type)))
                .collect(),
            metas: metas.to_vec(),
        }
    }

    pub fn freeze(self) -> DecodedChunk {
        let channels: Vec<ChanSamples> =
            self.channels.into_iter().map(|m| m.into_inner().unwrap()).collect();
        let mut bytes = 0usize;
        for c in &channels {
            bytes += match c {
                ChanSamples::Float { ts, .. } => ts.len() * (8 + 8),
                ChanSamples::Int { ts, .. } => ts.len() * (8 + 8),
                ChanSamples::Bool { ts, .. } => ts.len() * (8 + 1),
                ChanSamples::Text { lines } => {
                    lines.iter().map(|(_, s)| 8 + s.len() + 24).sum::<usize>()
                }
            };
        }
        DecodedChunk { channels, bytes }
    }
}

pub struct DecodedChunk {
    pub channels: Vec<ChanSamples>,
    bytes: usize,
}

impl DecodedChunk {
    pub fn bytes(&self) -> usize {
        self.bytes
    }

    pub fn window(&self, ch: usize, window: TimeWindow) -> ChannelSnapshot {
        let Some(c) = self.channels.get(ch) else {
            return ChannelSnapshot::Float { ts: Vec::new(), vals: Vec::new() };
        };
        match c {
            ChanSamples::Float { ts, vals } => {
                let (s, e) = clip(ts, window);
                ChannelSnapshot::Float { ts: ts[s..e].to_vec(), vals: vals[s..e].to_vec() }
            }
            ChanSamples::Int { ts, vals } => {
                let (s, e) = clip(ts, window);
                ChannelSnapshot::Int { ts: ts[s..e].to_vec(), vals: vals[s..e].to_vec() }
            }
            ChanSamples::Bool { ts, vals } => {
                let (s, e) = clip(ts, window);
                ChannelSnapshot::Bool { ts: ts[s..e].to_vec(), vals: vals[s..e].to_vec() }
            }
            ChanSamples::Text { lines } => {
                let s = lines.partition_point(|(t, _)| *t < window.start_ns);
                let e = lines.partition_point(|(t, _)| *t < window.end_ns);
                ChannelSnapshot::Text { lines: lines[s..e].to_vec() }
            }
        }
    }

    /// Newest sample at or before `end_ns` for channel `ch`, or `None` if this
    /// chunk holds no such sample.
    pub fn last_le(&self, ch: usize, end_ns: i64) -> Option<(i64, Sample)> {
        let c = self.channels.get(ch)?;
        match c {
            ChanSamples::Float { ts, vals } => {
                let i = ts.partition_point(|&t| t <= end_ns);
                (i > 0).then(|| (ts[i - 1], Sample::Float(vals[i - 1])))
            }
            ChanSamples::Int { ts, vals } => {
                let i = ts.partition_point(|&t| t <= end_ns);
                (i > 0).then(|| (ts[i - 1], Sample::Int(vals[i - 1])))
            }
            ChanSamples::Bool { ts, vals } => {
                let i = ts.partition_point(|&t| t <= end_ns);
                (i > 0).then(|| (ts[i - 1], Sample::Bool(vals[i - 1] != 0)))
            }
            ChanSamples::Text { lines } => {
                let i = lines.partition_point(|(t, _)| *t <= end_ns);
                (i > 0).then(|| (lines[i - 1].0, Sample::Text(lines[i - 1].1.clone())))
            }
        }
    }
}

fn clip(ts: &[i64], window: TimeWindow) -> (usize, usize) {
    let s = ts.partition_point(|&t| t < window.start_ns);
    let e = ts.partition_point(|&t| t < window.end_ns);
    (s, e)
}

impl ChannelStore for ChunkDecodeBuf {
    fn write_numeric(&self, channel: ChannelId, ts: i64, val: NumericVal) {
        let mut c = self.channels[channel.0 as usize].lock().unwrap();
        match (&mut *c, val) {
            (ChanSamples::Float { ts: tv, vals }, NumericVal::Float(v)) => {
                tv.push(ts);
                vals.push(v);
            }
            (ChanSamples::Int { ts: tv, vals }, NumericVal::Int(v)) => {
                tv.push(ts);
                vals.push(v);
            }
            (ChanSamples::Bool { ts: tv, vals }, NumericVal::Bool(v)) => {
                tv.push(ts);
                vals.push(v as u8);
            }
            _ => {}
        }
    }

    fn write_text(&self, channel: ChannelId, ts: i64, line: String) {
        let mut c = self.channels[channel.0 as usize].lock().unwrap();
        if let ChanSamples::Text { lines } = &mut *c {
            lines.push((ts, line));
        }
    }

    fn snapshot(&self, _channel: ChannelId, _window: TimeWindow) -> ChannelSnapshot {
        ChannelSnapshot::Float { ts: Vec::new(), vals: Vec::new() }
    }

    fn latest(&self, _channel: ChannelId) -> Option<(i64, Sample)> {
        None
    }

    fn channel_meta(&self, channel: ChannelId) -> &ChannelMeta {
        &self.metas[channel.0 as usize]
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::ChannelRegistry;
    use crate::store::ChannelStore;
    use crate::types::{ChannelId, NumericVal, SampleType, TimeWindow};

    fn reg() -> ChannelRegistry {
        ChannelRegistry::from_toml_str(
            r#"
[channels."a.f"]
topic = "t"
proto_path = "M.v"
ts_path = "M.t"
type = "float"
"#,
        )
        .unwrap()
    }

    #[test]
    fn buf_collects_then_freezes_and_windows() {
        let r = reg();
        let buf = ChunkDecodeBuf::new(&r);
        let id = r.id("a.f").unwrap();
        buf.write_numeric(id, 10, NumericVal::Float(1.0));
        buf.write_numeric(id, 20, NumericVal::Float(2.0));
        buf.write_numeric(id, 30, NumericVal::Float(3.0));
        let chunk = buf.freeze();
        assert!(chunk.bytes() > 0);
        match chunk.window(id.0 as usize, TimeWindow { start_ns: 15, end_ns: 30 }) {
            ChannelSnapshot::Float { ts, vals } => {
                assert_eq!(ts, vec![20]);
                assert_eq!(vals, vec![2.0]);
            }
            other => panic!("wrong variant: {other:?}"),
        }
    }

    #[test]
    fn from_metas_matches_registry_shape() {
        let metas = vec![ChannelMeta {
            name: "x".into(),
            sample_type: SampleType::Float,
            unit: String::new(),
            color: "#fff".into(),
            max_rate: 1,
            history_s: 1.0,
            max_lines: 1,
            text_coded: false,
        }];
        let buf = ChunkDecodeBuf::from_metas(&metas);
        buf.write_numeric(ChannelId(0), 1, NumericVal::Float(2.0));
        let c = buf.freeze();
        assert!(c.bytes() > 0);
        assert_eq!(c.last_le(0, 5).map(|(t, _)| t), Some(1));
    }
}
