use std::collections::{HashMap, HashSet};
use std::sync::Mutex;

use crate::config::ChannelRegistry;
use crate::ingest::bridge::schema::pb;
use crate::ingest::decode::resolve_ts;
use crate::store::ChannelStore;
use crate::types::{ChannelId, NumericVal, SampleType};

struct BridgeBinding {
    id: ChannelId,
    sample_type: SampleType,
    eu_scale: f64,
    eu_offset: f64,
    name: String,
}

/// Maps a bridge column `topic` to its channel and applies decoded `Batch`
/// messages to the store. A bridge channel is any registry channel with a
/// `topic` set; `topic` uniquely identifies one channel (unlike ZMQ, where
/// several channels share a topic via distinct `proto_path`s).
pub struct BridgeRouter {
    map: HashMap<String, BridgeBinding>,
    /// Unknown topics already logged, so the warning fires once each.
    warned: Mutex<HashSet<String>>,
}

impl BridgeRouter {
    pub fn build(registry: &ChannelRegistry) -> Self {
        let mut map: HashMap<String, BridgeBinding> = HashMap::new();
        for id in registry.iter_ids() {
            let cfg = registry.config(id);
            let Some(topic) = &cfg.topic else { continue };
            let meta = registry.meta(id);
            let binding = BridgeBinding {
                id,
                sample_type: meta.sample_type,
                eu_scale: cfg.eu_scale,
                eu_offset: cfg.eu_offset,
                name: meta.name.clone(),
            };
            if map.insert(topic.clone(), binding).is_some() {
                eprintln!("bridge: duplicate topic {topic:?}; keeping the last-declared channel");
            }
        }
        Self { map, warned: Mutex::new(HashSet::new()) }
    }

    /// Apply one decoded batch; returns the number of samples written.
    pub fn apply(&self, batch: &pb::Batch, store: &dyn ChannelStore) -> usize {
        let mut written = 0;
        for col in &batch.cols {
            let Some(b) = self.map.get(&col.topic) else {
                self.warn_unknown(&col.topic);
                continue;
            };
            written += self.apply_column(b, col, store);
        }
        written
    }

    fn apply_column(&self, b: &BridgeBinding, col: &pb::Column, store: &dyn ChannelStore) -> usize {
        let ts = &col.t_ns;
        match (&col.values, b.sample_type) {
            (Some(pb::column::Values::Strings(s)), SampleType::Text) => {
                if s.v.len() != ts.len() {
                    self.warn_len(b, ts.len(), s.v.len());
                    return 0;
                }
                for (t, line) in ts.iter().zip(&s.v) {
                    store.write_text(b.id, resolve_ts(Some(*t)), line.clone());
                }
                s.v.len()
            }
            (Some(pb::column::Values::Doubles(d)), st) if st != SampleType::Text => {
                if d.v.len() != ts.len() {
                    self.warn_len(b, ts.len(), d.v.len());
                    return 0;
                }
                for (t, raw) in ts.iter().zip(&d.v) {
                    store.write_numeric(b.id, resolve_ts(Some(*t)), scale(b, *raw));
                }
                d.v.len()
            }
            (Some(pb::column::Values::Ints(i)), st) if st != SampleType::Text => {
                if i.v.len() != ts.len() {
                    self.warn_len(b, ts.len(), i.v.len());
                    return 0;
                }
                for (t, raw) in ts.iter().zip(&i.v) {
                    store.write_numeric(b.id, resolve_ts(Some(*t)), scale(b, *raw as f64));
                }
                i.v.len()
            }
            (Some(_), _) => {
                eprintln!(
                    "bridge: column type incompatible with channel {:?} ({:?}); dropping",
                    b.name, b.sample_type
                );
                0
            }
            (None, _) => 0, // empty column: no value set
        }
    }

    fn warn_unknown(&self, topic: &str) {
        if let Ok(mut w) = self.warned.lock() {
            if w.insert(topic.to_string()) {
                eprintln!("bridge: unknown topic {topic:?} (not in config); dropping column");
            }
        }
    }

    fn warn_len(&self, b: &BridgeBinding, ts: usize, vals: usize) {
        eprintln!(
            "bridge: channel {:?} length mismatch (t_ns={ts}, values={vals}); dropping column",
            b.name
        );
    }
}

/// Apply the channel's engineering-unit transform and coerce to its type.
fn scale(b: &BridgeBinding, raw: f64) -> NumericVal {
    let v = raw * b.eu_scale + b.eu_offset;
    match b.sample_type {
        SampleType::Float => NumericVal::Float(v),
        SampleType::Int => NumericVal::Int(v as i64),
        SampleType::Bool => NumericVal::Bool(v != 0.0),
        SampleType::Text => NumericVal::Float(v), // unreachable: guarded by caller
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::ChannelRegistry;
    use crate::ingest::bridge::schema::pb;
    use crate::store::LiveStore;
    use crate::types::{ChannelSnapshot, TimeWindow};

    const ALL: TimeWindow = TimeWindow { start_ns: i64::MIN, end_ns: i64::MAX };

    fn registry() -> ChannelRegistry {
        ChannelRegistry::from_toml_str(
            r#"
[channels."accel"]
topic = "accel"
type = "float"
eu_scale = 2.0
eu_offset = 1.0

[channels."state"]
topic = "state"
type = "int"

[channels."log"]
topic = "log"
type = "text"
"#,
        )
        .unwrap()
    }

    fn col_doubles(topic: &str, t: Vec<i64>, v: Vec<f64>) -> pb::Column {
        pb::Column { topic: topic.into(), t_ns: t, values: Some(pb::column::Values::Doubles(pb::DoubleCol { v })) }
    }

    #[test]
    fn applies_scaled_doubles_and_routes_multiple_rates() {
        let reg = registry();
        let store = LiveStore::from_registry(&reg);
        let router = BridgeRouter::build(&reg);
        // Two channels, different lengths (different rates) in one batch.
        let batch = pb::Batch {
            cols: vec![
                col_doubles("accel", vec![10, 20, 30], vec![1.0, 2.0, 3.0]),
                pb::Column {
                    topic: "state".into(),
                    t_ns: vec![10],
                    values: Some(pb::column::Values::Ints(pb::Sint64Col { v: vec![7] })),
                },
            ],
        };
        let n = router.apply(&batch, &store);
        assert_eq!(n, 4);

        let accel = reg.id("accel").unwrap();
        match store.snapshot(accel, ALL) {
            ChannelSnapshot::Float { ts, vals } => {
                assert_eq!(ts, vec![10, 20, 30]);
                // EU: raw*2+1 → 3,5,7
                assert_eq!(vals, vec![3.0, 5.0, 7.0]);
            }
            other => panic!("wrong variant: {other:?}"),
        }
    }

    #[test]
    fn routes_strings_to_text_channel() {
        let reg = registry();
        let store = LiveStore::from_registry(&reg);
        let router = BridgeRouter::build(&reg);
        let batch = pb::Batch {
            cols: vec![pb::Column {
                topic: "log".into(),
                t_ns: vec![5, 6],
                values: Some(pb::column::Values::Strings(pb::StringCol {
                    v: vec!["a".into(), "b".into()],
                })),
            }],
        };
        assert_eq!(router.apply(&batch, &store), 2);
        let log = reg.id("log").unwrap();
        match store.snapshot(log, ALL) {
            ChannelSnapshot::Text { lines } => {
                assert_eq!(lines.iter().map(|(_, l)| l.as_str()).collect::<Vec<_>>(), vec!["a", "b"]);
            }
            other => panic!("wrong variant: {other:?}"),
        }
    }

    #[test]
    fn length_mismatch_drops_column_keeps_siblings() {
        let reg = registry();
        let store = LiveStore::from_registry(&reg);
        let router = BridgeRouter::build(&reg);
        let batch = pb::Batch {
            cols: vec![
                col_doubles("accel", vec![1, 2], vec![9.0]), // len mismatch → dropped
                col_doubles("accel", vec![3], vec![9.0]),    // ok → 1 sample
            ],
        };
        assert_eq!(router.apply(&batch, &store), 1);
    }

    #[test]
    fn type_incompatible_column_is_dropped() {
        let reg = registry();
        let store = LiveStore::from_registry(&reg);
        let router = BridgeRouter::build(&reg);
        // strings into the numeric "accel" channel → dropped.
        let batch = pb::Batch {
            cols: vec![pb::Column {
                topic: "accel".into(),
                t_ns: vec![1],
                values: Some(pb::column::Values::Strings(pb::StringCol { v: vec!["x".into()] })),
            }],
        };
        assert_eq!(router.apply(&batch, &store), 0);
    }

    #[test]
    fn unknown_topic_is_dropped() {
        let reg = registry();
        let store = LiveStore::from_registry(&reg);
        let router = BridgeRouter::build(&reg);
        let batch = pb::Batch { cols: vec![col_doubles("nope", vec![1], vec![1.0])] };
        assert_eq!(router.apply(&batch, &store), 0);
    }
}
