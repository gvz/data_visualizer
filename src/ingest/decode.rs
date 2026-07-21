use prost_reflect::{DynamicMessage, ReflectMessage, Value};

use crate::ingest::router::ChannelBinding;
use crate::store::ChannelStore;
use crate::types::{now_ns, NumericVal, SampleType};

pub fn decode_batch(data: &[u8], bindings: &[ChannelBinding], store: &dyn ChannelStore) -> usize {
    if bindings.is_empty() {
        return 0;
    }
    let msg_desc = &bindings[0].msg_desc;
    let msg = match DynamicMessage::decode(msg_desc.clone(), data) {
        Ok(m) => m,
        Err(e) => {
            eprintln!("ingest: proto decode error: {e}");
            return 0;
        }
    };
    let mut total = 0;
    for binding in bindings {
        total += decode_channel(&msg, binding, store);
    }
    total
}

fn decode_channel(msg: &DynamicMessage, binding: &ChannelBinding, store: &dyn ChannelStore) -> usize {
    match binding.val_path.len() {
        2 => decode_batch_channel(msg, binding, store),
        1 => decode_single_channel(msg, binding, store),
        n => {
            eprintln!("ingest: unsupported field path depth {n} for channel {:?}", binding.val_path);
            0
        }
    }
}

fn decode_batch_channel(msg: &DynamicMessage, binding: &ChannelBinding, store: &dyn ChannelStore) -> usize {
    let repeated_name = &binding.val_path[0];
    let val_leaf = &binding.val_path[1];
    // ts_path has same structure: [repeated_field, ts_leaf] or just [ts_leaf] for flat paths.
    let ts_leaf = binding.ts_path.get(1).unwrap_or(&binding.ts_path[0]);

    let Some(field_desc) = msg.descriptor().get_field_by_name(repeated_name) else {
        return 0;
    };
    let repeated = msg.get_field(&field_desc).into_owned();
    let Value::List(samples) = repeated else {
        return 0;
    };

    let mut count = 0;
    for sample_val in &samples {
        let Value::Message(sample_msg) = sample_val else {
            continue;
        };
        let ts = resolve_ts(get_named_field(sample_msg, ts_leaf).and_then(|v| extract_ts(&v)));
        let Some(val_v) = get_named_field(sample_msg, val_leaf) else {
            continue;
        };
        if write_value(binding, ts, &val_v, store) {
            count += 1;
        }
    }
    count
}

fn decode_single_channel(msg: &DynamicMessage, binding: &ChannelBinding, store: &dyn ChannelStore) -> usize {
    let val_leaf = &binding.val_path[0];
    let ts_leaf = &binding.ts_path[0];

    let ts = resolve_ts(get_named_field(msg, ts_leaf).and_then(|v| extract_ts(&v)));
    let Some(val_v) = get_named_field(msg, val_leaf) else {
        return 0;
    };
    usize::from(write_value(binding, ts, &val_v, store))
}

fn get_named_field(msg: &DynamicMessage, name: &str) -> Option<Value> {
    let field_desc = msg.descriptor().get_field_by_name(name)?;
    Some(msg.get_field(&field_desc).into_owned())
}

fn write_value(binding: &ChannelBinding, ts: i64, val: &Value, store: &dyn ChannelStore) -> bool {
    match binding.sample_type {
        SampleType::Text => {
            if let Some(s) = extract_text(val) {
                store.write_text(binding.id, ts, s);
                true
            } else {
                false
            }
        }
        st => {
            if let Some(nv) = extract_numeric(val, st, binding.eu_scale, binding.eu_offset) {
                store.write_numeric(binding.id, ts, nv);
                true
            } else {
                false
            }
        }
    }
}

/// A sample's timestamp, falling back to UTC-now when the message carries none.
/// Proto3 reports an unset scalar as `0`, so a non-positive value is treated as
/// "no timestamp" and stamped with the current time rather than 1970.
fn resolve_ts(ts: Option<i64>) -> i64 {
    ts.filter(|&t| t > 0).unwrap_or_else(now_ns)
}

fn extract_ts(val: &Value) -> Option<i64> {
    match val {
        Value::I64(v) => Some(*v),
        Value::U64(v) => Some(*v as i64),
        Value::I32(v) => Some(*v as i64),
        Value::U32(v) => Some(*v as i64),
        _ => None,
    }
}

fn extract_numeric(
    val: &Value,
    sample_type: SampleType,
    eu_scale: f64,
    eu_offset: f64,
) -> Option<NumericVal> {
    let raw: f64 = match val {
        Value::F64(v) => *v,
        Value::F32(v) => *v as f64,
        Value::I64(v) => *v as f64,
        Value::I32(v) => *v as f64,
        Value::U64(v) => *v as f64,
        Value::U32(v) => *v as f64,
        Value::Bool(b) => if *b { 1.0 } else { 0.0 },
        _ => return None,
    };
    match sample_type {
        SampleType::Float => Some(NumericVal::Float(raw * eu_scale + eu_offset)),
        SampleType::Int => Some(NumericVal::Int((raw * eu_scale + eu_offset) as i64)),
        SampleType::Bool => Some(NumericVal::Bool(raw != 0.0)),
        SampleType::Text => None,
    }
}

fn extract_text(val: &Value) -> Option<String> {
    match val {
        Value::String(s) => Some(s.clone()),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::ChannelRegistry;
    use crate::ingest::loader::ProtoSchema;
    use crate::ingest::router::TopicRouter;
    use crate::store::LiveStore;
    use crate::types::{ChannelSnapshot, TimeWindow};
    use prost_reflect::prost::Message as _;
    use prost_reflect::{DynamicMessage, Value};
    use std::io::Write;

    const ALL: TimeWindow = TimeWindow { start_ns: i64::MIN, end_ns: i64::MAX };

    fn make_schema_and_registry() -> (ProtoSchema, tempfile::TempDir, ChannelRegistry) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("test.proto");
        let mut f = std::fs::File::create(&path).unwrap();
        write!(f, r#"
syntax = "proto3";
message AccelBatch {{
  repeated Sample samples = 1;
  message Sample {{
    int64 t_ns = 1;
    float x = 2;
  }}
}}
"#).unwrap();
        let schema = ProtoSchema::from_path(&path).unwrap();
        let registry = ChannelRegistry::from_toml_str(r#"
[channels."accel.x"]
topic = "accel"
proto_path = "AccelBatch.samples.x"
ts_path = "AccelBatch.samples.t_ns"
type = "float"
eu_scale = 2.0
eu_offset = 1.0
"#).unwrap();
        (schema, dir, registry)
    }

    fn encode_accel_batch(schema: &ProtoSchema, samples: &[(i64, f32)]) -> Vec<u8> {
        let pool = schema.pool_for_test();
        let batch_desc = pool.get_message_by_name("AccelBatch").unwrap();
        let sample_desc = pool.get_message_by_name("AccelBatch.Sample").unwrap();
        let t_field = sample_desc.get_field_by_name("t_ns").unwrap();
        let x_field = sample_desc.get_field_by_name("x").unwrap();
        let samples_field = batch_desc.get_field_by_name("samples").unwrap();

        let list: Vec<Value> = samples
            .iter()
            .map(|(t, x)| {
                let mut s = DynamicMessage::new(sample_desc.clone());
                s.set_field(&t_field, Value::I64(*t));
                s.set_field(&x_field, Value::F32(*x));
                Value::Message(s)
            })
            .collect();
        let mut batch = DynamicMessage::new(batch_desc);
        batch.set_field(&samples_field, Value::List(list));
        batch.encode_to_vec()
    }

    #[test]
    fn decode_batch_writes_eu_scaled_samples() {
        let (schema, _dir, registry) = make_schema_and_registry();
        let store = LiveStore::from_registry(&registry);
        let router = TopicRouter::build(&registry, &schema);
        let bindings = router.bindings_for("accel");
        let data = encode_accel_batch(&schema, &[(1_000_000_000, 2.0), (2_000_000_000, 3.0)]);

        let count = decode_batch(&data, bindings, &store);
        assert_eq!(count, 2);

        let ch = registry.id("accel.x").unwrap();
        match store.snapshot(ch, ALL) {
            ChannelSnapshot::Float { ts, vals } => {
                assert_eq!(ts, vec![1_000_000_000i64, 2_000_000_000i64]);
                // EU: raw * 2.0 + 1.0 → 2.0*2+1=5.0, 3.0*2+1=7.0
                assert!((vals[0] - 5.0_f64).abs() < 1e-4, "expected 5.0, got {}", vals[0]);
                assert!((vals[1] - 7.0_f64).abs() < 1e-4, "expected 7.0, got {}", vals[1]);
            }
            other => panic!("wrong snapshot variant: {other:?}"),
        }
    }

    #[test]
    fn decode_batch_empty_bindings_returns_zero() {
        let (_, _dir, registry) = make_schema_and_registry();
        let store = LiveStore::from_registry(&registry);
        assert_eq!(decode_batch(&[1, 2, 3], &[], &store), 0);
    }

    #[test]
    fn decode_batch_bad_bytes_returns_zero_no_panic() {
        let (schema, _dir, registry) = make_schema_and_registry();
        let store = LiveStore::from_registry(&registry);
        let router = TopicRouter::build(&registry, &schema);
        let bindings = router.bindings_for("accel");
        // Malformed proto bytes — must not panic, must return 0.
        assert_eq!(decode_batch(b"not valid protobuf at all!!!", bindings, &store), 0);
    }

    #[test]
    fn missing_timestamp_falls_back_to_now() {
        let (schema, _dir, registry) = make_schema_and_registry();
        let store = LiveStore::from_registry(&registry);
        let router = TopicRouter::build(&registry, &schema);
        let bindings = router.bindings_for("accel");
        let before = now_ns();
        // t_ns = 0 (proto3 default for an unset field) → "no timestamp".
        let data = encode_accel_batch(&schema, &[(0, 2.0)]);

        let count = decode_batch(&data, bindings, &store);
        assert_eq!(count, 1);

        let after = now_ns();
        let ch = registry.id("accel.x").unwrap();
        match store.snapshot(ch, ALL) {
            ChannelSnapshot::Float { ts, .. } => {
                assert_eq!(ts.len(), 1);
                assert!(
                    ts[0] >= before && ts[0] <= after,
                    "sample stamped with now: {} not in [{before}, {after}]",
                    ts[0]
                );
            }
            other => panic!("wrong snapshot variant: {other:?}"),
        }
    }

    #[test]
    fn decode_batch_empty_repeated_returns_zero() {
        let (schema, _dir, registry) = make_schema_and_registry();
        let store = LiveStore::from_registry(&registry);
        let router = TopicRouter::build(&registry, &schema);
        let bindings = router.bindings_for("accel");
        // Empty batch: 0 samples.
        let data = encode_accel_batch(&schema, &[]);
        assert_eq!(decode_batch(&data, bindings, &store), 0);
    }
}
