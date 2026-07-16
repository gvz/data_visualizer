use std::sync::atomic::{AtomicU64, Ordering};

use crate::config::ChannelRegistry;
use crate::store::{ChannelStore, SoaRing, TextBuf};
use crate::types::{
    ChannelId, ChannelMeta, ChannelSnapshot, NumericVal, Sample, SampleType, TimeWindow,
};

enum ChannelData {
    Float(SoaRing<f64>),
    Int(SoaRing<i64>),
    Bool(SoaRing<u8>),
    Text(TextBuf),
}

struct ChannelSlot {
    meta: ChannelMeta,
    data: ChannelData,
}

/// Live store: one typed slot per configured channel, indexed by ChannelId.
pub struct LiveStore {
    channels: Vec<ChannelSlot>,
    type_errors: AtomicU64,
}

impl LiveStore {
    pub fn from_registry(reg: &ChannelRegistry) -> Self {
        let channels = reg
            .iter_ids()
            .map(|id| {
                let meta = reg.meta(id).clone();
                let cfg = reg.config(id);
                // 1.2× headroom so the ring's cap/8 reader guard margin
                // never cuts into the configured history depth.
                let cap = (cfg.max_rate as f64 * cfg.history_s * 1.2).ceil() as usize;
                let data = match meta.sample_type {
                    SampleType::Float => ChannelData::Float(SoaRing::new(cap)),
                    SampleType::Int => ChannelData::Int(SoaRing::new(cap)),
                    SampleType::Bool => ChannelData::Bool(SoaRing::new(cap)),
                    SampleType::Text => ChannelData::Text(TextBuf::new(cfg.max_lines)),
                };
                ChannelSlot { meta, data }
            })
            .collect();
        Self { channels, type_errors: AtomicU64::new(0) }
    }

    /// Count of writes dropped because value type didn't match channel type.
    pub fn type_errors(&self) -> u64 {
        self.type_errors.load(Ordering::Relaxed)
    }

    fn slot(&self, id: ChannelId) -> &ChannelSlot {
        &self.channels[id.0 as usize]
    }

    fn count_type_error(&self) {
        self.type_errors.fetch_add(1, Ordering::Relaxed);
    }
}

impl ChannelStore for LiveStore {
    fn write_numeric(&self, channel: ChannelId, ts: i64, val: NumericVal) {
        match (&self.slot(channel).data, val) {
            (ChannelData::Float(r), NumericVal::Float(v)) => r.push(ts, v),
            (ChannelData::Int(r), NumericVal::Int(v)) => r.push(ts, v),
            (ChannelData::Bool(r), NumericVal::Bool(v)) => r.push(ts, u8::from(v)),
            _ => self.count_type_error(),
        }
    }

    fn write_text(&self, channel: ChannelId, ts: i64, line: String) {
        match &self.slot(channel).data {
            ChannelData::Text(t) => t.push(ts, line),
            _ => self.count_type_error(),
        }
    }

    fn snapshot(&self, channel: ChannelId, window: TimeWindow) -> ChannelSnapshot {
        match &self.slot(channel).data {
            ChannelData::Float(r) => {
                let (mut ts, mut vals) = (Vec::new(), Vec::new());
                r.read_window(window, &mut ts, &mut vals);
                ChannelSnapshot::Float { ts, vals }
            }
            ChannelData::Int(r) => {
                let (mut ts, mut vals) = (Vec::new(), Vec::new());
                r.read_window(window, &mut ts, &mut vals);
                ChannelSnapshot::Int { ts, vals }
            }
            ChannelData::Bool(r) => {
                let (mut ts, mut vals) = (Vec::new(), Vec::new());
                r.read_window(window, &mut ts, &mut vals);
                ChannelSnapshot::Bool { ts, vals }
            }
            ChannelData::Text(t) => ChannelSnapshot::Text { lines: t.window(window) },
        }
    }

    fn latest(&self, channel: ChannelId) -> Option<(i64, Sample)> {
        match &self.slot(channel).data {
            ChannelData::Float(r) => r.latest().map(|(t, v)| (t, Sample::Float(v))),
            ChannelData::Int(r) => r.latest().map(|(t, v)| (t, Sample::Int(v))),
            ChannelData::Bool(r) => r.latest().map(|(t, v)| (t, Sample::Bool(v != 0))),
            ChannelData::Text(t) => t.latest().map(|(ts, l)| (ts, Sample::Text(l))),
        }
    }

    fn channel_meta(&self, channel: ChannelId) -> &ChannelMeta {
        &self.slot(channel).meta
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::ChannelRegistry;
    use crate::types::{NumericVal, Sample, SampleType, TimeWindow};

    const ALL: TimeWindow = TimeWindow { start_ns: i64::MIN, end_ns: i64::MAX };

    fn registry() -> ChannelRegistry {
        ChannelRegistry::from_toml_str(
            r#"
[channels."a.float"]
topic = "t"
proto_path = "p"
ts_path = "q"
type = "float"
max_rate = 100
history_s = 1.0

[channels."b.int"]
topic = "t"
proto_path = "p"
ts_path = "q"
type = "int"
max_rate = 100
history_s = 1.0

[channels."c.bool"]
topic = "t"
proto_path = "p"
ts_path = "q"
type = "bool"
max_rate = 100
history_s = 1.0

[channels."d.log"]
topic = "t"
proto_path = "p"
ts_path = "q"
type = "text"
max_lines = 10
"#,
        )
        .unwrap()
    }

    #[test]
    fn write_and_snapshot_each_type() {
        let reg = registry();
        let store = LiveStore::from_registry(&reg);
        let (fa, ib, bc, td) = (
            reg.id("a.float").unwrap(),
            reg.id("b.int").unwrap(),
            reg.id("c.bool").unwrap(),
            reg.id("d.log").unwrap(),
        );
        store.write_numeric(fa, 1, NumericVal::Float(1.5));
        store.write_numeric(ib, 2, NumericVal::Int(-7));
        store.write_numeric(bc, 3, NumericVal::Bool(true));
        store.write_text(td, 4, "hello".into());

        match store.snapshot(fa, ALL) {
            ChannelSnapshot::Float { ts, vals } => {
                assert_eq!(ts, vec![1]);
                assert_eq!(vals, vec![1.5]);
            }
            other => panic!("wrong snapshot variant: {other:?}"),
        }
        match store.snapshot(bc, ALL) {
            ChannelSnapshot::Bool { vals, .. } => assert_eq!(vals, vec![1u8]),
            other => panic!("wrong snapshot variant: {other:?}"),
        }
        match store.snapshot(td, ALL) {
            ChannelSnapshot::Text { lines } => {
                assert_eq!(lines, vec![(4, "hello".to_string())])
            }
            other => panic!("wrong snapshot variant: {other:?}"),
        }
        assert_eq!(store.latest(ib), Some((2, Sample::Int(-7))));
        assert_eq!(store.latest(bc), Some((3, Sample::Bool(true))));
        assert_eq!(store.channel_meta(fa).sample_type, SampleType::Float);
    }

    #[test]
    fn type_mismatch_is_counted_not_panicking() {
        let reg = registry();
        let store = LiveStore::from_registry(&reg);
        let fa = reg.id("a.float").unwrap();
        let td = reg.id("d.log").unwrap();
        store.write_numeric(fa, 1, NumericVal::Int(3)); // Int into Float channel
        store.write_numeric(td, 2, NumericVal::Float(1.0)); // numeric into text
        store.write_text(fa, 3, "oops".into()); // text into numeric
        assert_eq!(store.type_errors(), 3);
        assert!(store.snapshot(fa, ALL).is_empty());
        assert_eq!(store.latest(fa), None);
    }

    #[test]
    fn ring_sized_from_config_with_headroom() {
        let reg = registry();
        let store = LiveStore::from_registry(&reg);
        let fa = reg.id("a.float").unwrap();
        // max_rate 100 × history 1.0 s × 1.2 = 120 → cap 128, visible 112 ≥ 100
        for i in 0..200i64 {
            store.write_numeric(fa, i, NumericVal::Float(i as f64));
        }
        let snap = store.snapshot(fa, ALL);
        assert!(snap.len() >= 100, "visible history below configured depth");
    }
}
