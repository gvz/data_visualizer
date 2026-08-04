use std::sync::atomic::{AtomicI64, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

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
    /// Discovered text stored the waveform way: a rate×history ring of interned
    /// integer codes plus the code→label table. A state graph reads the codes
    /// (full retention, cheap `Copy` samples) and resolves labels via
    /// [`ChannelStore::state_labels`].
    CodedText { ring: SoaRing<i64>, codec: Mutex<TextCodec> },
}

/// Bidirectional map between state strings and small integer codes, assigned in
/// first-seen order. A state channel has few distinct values, so the table
/// stays tiny even as millions of samples flow through the ring.
#[derive(Default)]
struct TextCodec {
    to_code: std::collections::HashMap<String, i64>,
    labels: Vec<String>,
}

impl TextCodec {
    fn intern(&mut self, s: &str) -> i64 {
        if let Some(&c) = self.to_code.get(s) {
            return c;
        }
        let c = self.labels.len() as i64;
        self.to_code.insert(s.to_string(), c);
        self.labels.push(s.to_string());
        c
    }

    fn label(&self, code: i64) -> Option<&str> {
        usize::try_from(code).ok().and_then(|i| self.labels.get(i)).map(String::as_str)
    }
}

struct ChannelSlot {
    meta: ChannelMeta,
    data: ChannelData,
}

/// Live store: one typed slot per channel, indexed by ChannelId. The slot
/// list is append-only (`boxcar::Vec`) so runtime channels can be added with
/// `&self` while ingest writes to existing slots; existing `&`-borrows stay
/// valid across a push.
pub struct LiveStore {
    channels: boxcar::Vec<ChannelSlot>,
    type_errors: AtomicU64,
    /// Bumped on every write; the GUI polls it for cheap change detection so it
    /// only repaints at full rate while data is arriving.
    writes: AtomicU64,
    /// When non-zero, `now_ns()` returns this value instead of the wall clock.
    /// Set by the app to implement live scrubbing without entering replay mode.
    pub view_override: Arc<AtomicI64>,
}

/// Build a typed ring slot from a channel's meta. 1.2× headroom so the ring's
/// cap/8 reader guard margin never cuts into the configured history depth.
fn slot_from_meta(meta: ChannelMeta) -> ChannelSlot {
    let cap = (meta.max_rate as f64 * meta.history_s * 1.2).ceil() as usize;
    let data = match meta.sample_type {
        SampleType::Float => ChannelData::Float(SoaRing::new(cap)),
        SampleType::Int => ChannelData::Int(SoaRing::new(cap)),
        SampleType::Bool => ChannelData::Bool(SoaRing::new(cap)),
        SampleType::Text if meta.text_coded => {
            ChannelData::CodedText { ring: SoaRing::new(cap), codec: Mutex::new(TextCodec::default()) }
        }
        SampleType::Text => ChannelData::Text(TextBuf::new(meta.max_lines)),
    };
    ChannelSlot { meta, data }
}

impl LiveStore {
    pub fn from_registry(reg: &ChannelRegistry) -> Self {
        let channels = boxcar::Vec::new();
        for id in reg.iter_ids() {
            channels.push(slot_from_meta(reg.meta(id).clone()));
        }
        Self {
            channels,
            type_errors: AtomicU64::new(0),
            writes: AtomicU64::new(0),
            view_override: Arc::new(AtomicI64::new(0)),
        }
    }

    /// Count of writes dropped because value type didn't match channel type.
    pub fn type_errors(&self) -> u64 {
        self.type_errors.load(Ordering::Relaxed)
    }

    fn slot(&self, id: ChannelId) -> &ChannelSlot {
        self.channels.get(id.0 as usize).expect("channel id out of range")
    }

    /// Like [`Self::slot`] but tolerant of an id the registry has already
    /// handed out while this store's matching slot is still being appended.
    /// Dynamic registration grows the registry then the store (see
    /// `resolve_or_register_drop`), so a reader on another thread — the script
    /// engine, the channel tree's value lookup — can resolve an id one step
    /// ahead of its slot. Read paths treat that as "no data yet" instead of
    /// panicking; the slot lands a moment later.
    fn try_slot(&self, id: ChannelId) -> Option<&ChannelSlot> {
        self.channels.get(id.0 as usize)
    }

    fn count_type_error(&self) {
        self.type_errors.fetch_add(1, Ordering::Relaxed);
    }
}

impl ChannelStore for LiveStore {
    fn now_ns(&self) -> i64 {
        let v = self.view_override.load(Ordering::Relaxed);
        if v != 0 { v } else { crate::types::now_ns() }
    }

    fn write_numeric(&self, channel: ChannelId, ts: i64, val: NumericVal) {
        self.writes.fetch_add(1, Ordering::Relaxed);
        match (&self.slot(channel).data, val) {
            (ChannelData::Float(r), NumericVal::Float(v)) => r.push(ts, v),
            (ChannelData::Int(r), NumericVal::Int(v)) => r.push(ts, v),
            (ChannelData::Bool(r), NumericVal::Bool(v)) => r.push(ts, u8::from(v)),
            _ => self.count_type_error(),
        }
    }

    fn write_text(&self, channel: ChannelId, ts: i64, line: String) {
        self.writes.fetch_add(1, Ordering::Relaxed);
        match &self.slot(channel).data {
            ChannelData::Text(t) => t.push(ts, line),
            ChannelData::CodedText { ring, codec } => {
                let code = codec.lock().unwrap().intern(&line);
                ring.push(ts, code);
            }
            _ => self.count_type_error(),
        }
    }

    fn write_seq(&self) -> u64 {
        self.writes.load(Ordering::Relaxed)
    }

    fn snapshot(&self, channel: ChannelId, window: TimeWindow) -> ChannelSnapshot {
        let Some(slot) = self.try_slot(channel) else {
            // Slot not appended yet (see `try_slot`): empty window.
            return ChannelSnapshot::Float { ts: Vec::new(), vals: Vec::new() };
        };
        match &slot.data {
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
            // Codes ride the same ring path as Int; the state graph pairs them
            // with `state_labels` to recover the strings.
            ChannelData::CodedText { ring, .. } => {
                let (mut ts, mut vals) = (Vec::new(), Vec::new());
                ring.read_window(window, &mut ts, &mut vals);
                ChannelSnapshot::Int { ts, vals }
            }
        }
    }

    fn latest(&self, channel: ChannelId) -> Option<(i64, Sample)> {
        match &self.try_slot(channel)?.data {
            ChannelData::Float(r) => r.latest().map(|(t, v)| (t, Sample::Float(v))),
            ChannelData::Int(r) => r.latest().map(|(t, v)| (t, Sample::Int(v))),
            ChannelData::Bool(r) => r.latest().map(|(t, v)| (t, Sample::Bool(v != 0))),
            ChannelData::Text(t) => t.latest().map(|(ts, l)| (ts, Sample::Text(l))),
            ChannelData::CodedText { ring, codec } => ring.latest().map(|(ts, code)| {
                let label = codec.lock().unwrap().label(code).unwrap_or("").to_string();
                (ts, Sample::Text(label))
            }),
        }
    }

    fn channel_meta(&self, channel: ChannelId) -> &ChannelMeta {
        &self.slot(channel).meta
    }

    fn add_channel(&self, meta: ChannelMeta) {
        self.channels.push(slot_from_meta(meta));
    }

    fn state_labels(&self, channel: ChannelId) -> Option<Vec<String>> {
        match &self.try_slot(channel)?.data {
            ChannelData::CodedText { codec, .. } => Some(codec.lock().unwrap().labels.clone()),
            _ => None,
        }
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

    /// A discovered (coded) text channel keeps far more than a `TextBuf`'s line
    /// cap — its ring is sized by rate×history like a waveform — and exposes the
    /// codes as an `Int` snapshot plus the code→label table.
    #[test]
    fn coded_text_channel_uses_ring_retention_and_labels() {
        let reg = registry();
        let store = LiveStore::from_registry(&reg);
        let id = crate::types::ChannelId(reg.len() as u32);
        // max_lines is tiny (5), but text_coded routes storage to a ring sized
        // by max_rate(1000) × history_s(10) — thousands of samples.
        store.add_channel(ChannelMeta {
            name: "vx/state".into(),
            sample_type: SampleType::Text,
            unit: String::new(),
            color: "#cccccc".into(),
            max_rate: 1000,
            history_s: 10.0,
            max_lines: 5,
            text_coded: true,
        });
        // 2000 samples cycling through 3 states — well past max_lines.
        let states = ["idle", "running", "error"];
        for i in 0..2000i64 {
            store.write_text(id, i, states[(i as usize / 100) % 3].to_string());
        }
        // Retention: the ring holds all 2000 (a TextBuf would keep only 5).
        let snap = store.snapshot(id, ALL);
        let ChannelSnapshot::Int { ts, vals } = snap else {
            panic!("coded text must snapshot as Int codes");
        };
        assert_eq!(ts.len(), 2000, "ring retained every sample");
        // Codes resolve to the three interned labels, first-seen order.
        assert_eq!(store.state_labels(id).unwrap(), vec!["idle", "running", "error"]);
        assert_eq!(vals[0], 0); // idle
        assert_eq!(vals[100], 1); // running
        assert_eq!(vals[200], 2); // error
        // latest reconstructs the string.
        assert_eq!(store.latest(id), Some((1999, Sample::Text("running".into()))));
    }

    #[test]
    fn reads_tolerate_id_ahead_of_slot() {
        // Registry hands out an id, but the matching store slot hasn't been
        // appended yet (the dynamic-registration race: registry grows before
        // the store). Reads must return "no data", not panic.
        let reg = registry();
        let store = LiveStore::from_registry(&reg);
        let ahead = ChannelId(reg.len() as u32); // one past the last real slot
        match store.snapshot(ahead, ALL) {
            ChannelSnapshot::Float { ts, vals } => {
                assert!(ts.is_empty() && vals.is_empty());
            }
            other => panic!("expected empty snapshot, got {other:?}"),
        }
        assert_eq!(store.latest(ahead), None);
        assert_eq!(store.latest_at(ahead, i64::MAX), None);
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
    fn add_channel_appends_writable_slot() {
        let reg = registry();
        let store = LiveStore::from_registry(&reg);
        let new_id = crate::types::ChannelId(reg.len() as u32);
        store.add_channel(ChannelMeta {
            name: "home/temp".into(),
            sample_type: SampleType::Float,
            unit: String::new(),
            color: "#cccccc".into(),
            max_rate: 100,
            history_s: 30.0,
            max_lines: 500,
            text_coded: false,
        });
        store.write_numeric(new_id, 5, NumericVal::Float(21.5));
        assert_eq!(store.latest(new_id), Some((5, Sample::Float(21.5))));
        assert_eq!(store.channel_meta(new_id).name, "home/temp");
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
