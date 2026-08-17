use prost_reflect::{Cardinality, DynamicMessage, Kind, MessageDescriptor, ReflectMessage, Value};

use crate::ingest::router::ChannelBinding;
use crate::store::ChannelStore;
use crate::types::{now_ns, ChannelId, NumericVal, SampleType};

/// Read a base-128 varint at `*pos`, advancing `pos`. None on truncation.
pub(crate) fn read_varint(buf: &[u8], pos: &mut usize) -> Option<u64> {
    let mut result: u64 = 0;
    let mut shift = 0;
    loop {
        let byte = *buf.get(*pos)?;
        *pos += 1;
        result |= ((byte & 0x7f) as u64) << shift;
        if byte & 0x80 == 0 {
            return Some(result);
        }
        shift += 7;
        if shift >= 64 {
            return None; // overlong varint
        }
    }
}

#[derive(Clone, Copy)]
pub enum ValKind { F64, F32, I64, I32, U64, U32, Bool, Str }

pub struct FastPlan {
    pub repeated_field: Option<u32>,
    pub ts_field: u32,
    pub val_field: u32,
    pub val_kind: ValKind,
    pub sample_type: SampleType,
    pub eu_scale: f64,
    pub eu_offset: f64,
    pub id: ChannelId,
}

// Only claim kinds `scan_one` decodes bit-for-bit correctly. Sint32/Sint64 are
// zigzag-encoded (plain varint read would be wrong) and Sfixed32/Sfixed64/
// Fixed32/Fixed64 arrive as wire type 1/5, not the varint this scanner reads
// for integer kinds — both must fall back to the (correct) reflection path.
fn kind_to_valkind(kind: &Kind) -> Option<ValKind> {
    Some(match kind {
        Kind::Double => ValKind::F64,
        Kind::Float => ValKind::F32,
        Kind::Int64 => ValKind::I64,
        Kind::Int32 => ValKind::I32,
        Kind::Uint64 => ValKind::U64,
        Kind::Uint32 => ValKind::U32,
        Kind::Bool => ValKind::Bool,
        Kind::String => ValKind::Str,
        _ => return None,
    })
}

/// True for the plain-varint integer kinds `scan_one`/`extract_ts` read
/// correctly as a timestamp. Fixed/zigzag kinds must fall back to reflection.
fn is_ts_kind(kind: &Kind) -> bool {
    matches!(kind, Kind::Int64 | Kind::Int32 | Kind::Uint64 | Kind::Uint32)
}

impl FastPlan {
    pub fn build(
        msg_desc: &MessageDescriptor,
        val_path: &[String],
        ts_path: &[String],
        sample_type: SampleType,
        eu_scale: f64,
        eu_offset: f64,
        id: ChannelId,
    ) -> Option<FastPlan> {
        // Resolve the leaf value/ts field descriptors and, for the batch shape,
        // the repeated container field number.
        match (val_path.len(), ts_path.len()) {
            (1, 1) => {
                let val_f = msg_desc.get_field_by_name(&val_path[0])?;
                let ts_f = msg_desc.get_field_by_name(&ts_path[0])?;
                if !is_ts_kind(&ts_f.kind()) {
                    return None;
                }
                Some(FastPlan {
                    repeated_field: None,
                    ts_field: ts_f.number(),
                    val_field: val_f.number(),
                    val_kind: kind_to_valkind(&val_f.kind())?,
                    sample_type, eu_scale, eu_offset, id,
                })
            }
            (2, _) => {
                let rep = msg_desc.get_field_by_name(&val_path[0])?;
                if rep.cardinality() != Cardinality::Repeated {
                    return None;
                }
                let Kind::Message(sub) = rep.kind() else { return None };
                let val_f = sub.get_field_by_name(&val_path[1])?;
                // ts leaf: batch ts_path is [repeated, ts_leaf] or [ts_leaf].
                let ts_leaf = ts_path.get(1).or_else(|| ts_path.first())?;
                let ts_f = sub.get_field_by_name(ts_leaf)?;
                if !is_ts_kind(&ts_f.kind()) {
                    return None;
                }
                Some(FastPlan {
                    repeated_field: Some(rep.number()),
                    ts_field: ts_f.number(),
                    val_field: val_f.number(),
                    val_kind: kind_to_valkind(&val_f.kind())?,
                    sample_type, eu_scale, eu_offset, id,
                })
            }
            _ => None,
        }
    }
}

/// Wire types.
const WT_VARINT: u8 = 0;
const WT_I64: u8 = 1;
const WT_LEN: u8 = 2;
const WT_I32: u8 = 5;

/// Skip one field's payload given its wire type. Returns false on truncation.
fn skip_field(buf: &[u8], pos: &mut usize, wire: u8) -> bool {
    match wire {
        WT_VARINT => read_varint(buf, pos).is_some(),
        WT_I64 => { *pos += 8; *pos <= buf.len() }
        WT_I32 => { *pos += 4; *pos <= buf.len() }
        WT_LEN => {
            let Some(len) = read_varint(buf, pos) else { return false };
            match pos.checked_add(len as usize) {
                Some(p) => { *pos = p; *pos <= buf.len() }
                None => false,
            }
        }
        _ => false,
    }
}

/// Read a fixed 8-byte little-endian at pos, advancing. None on truncation.
fn read_f64(buf: &[u8], pos: &mut usize) -> Option<f64> {
    let end = pos.checked_add(8)?;
    let slice = buf.get(*pos..end)?;
    *pos = end;
    Some(f64::from_le_bytes(slice.try_into().ok()?))
}
fn read_f32(buf: &[u8], pos: &mut usize) -> Option<f32> {
    let end = pos.checked_add(4)?;
    let slice = buf.get(*pos..end)?;
    *pos = end;
    Some(f32::from_le_bytes(slice.try_into().ok()?))
}

/// Scan one message (flat, or one repeated element) for the plan's ts + value
/// fields, returning `(ts_opt, raw_or_text)`. `text` is Some only for Str kind.
fn scan_one(buf: &[u8], plan: &FastPlan) -> Option<(Option<i64>, Option<f64>, Option<String>)> {
    let mut pos = 0;
    let mut ts: Option<i64> = None;
    let mut raw: Option<f64> = None;
    let mut text: Option<String> = None;
    while pos < buf.len() {
        let key = read_varint(buf, &mut pos)?;
        let field = (key >> 3) as u32;
        let wire = (key & 0x7) as u8;
        if field == plan.ts_field && wire == WT_VARINT {
            ts = Some(read_varint(buf, &mut pos)? as i64);
        } else if field == plan.val_field {
            match plan.val_kind {
                ValKind::F64 if wire == WT_I64 => raw = Some(read_f64(buf, &mut pos)?),
                ValKind::F32 if wire == WT_I32 => raw = Some(read_f32(buf, &mut pos)? as f64),
                // Match reflection's `*v as f64` per kind exactly (sign matters
                // for large u64 and for negative i32/i64).
                ValKind::I64 if wire == WT_VARINT =>
                    raw = Some(read_varint(buf, &mut pos)? as i64 as f64),
                ValKind::U64 if wire == WT_VARINT =>
                    raw = Some(read_varint(buf, &mut pos)? as f64),
                ValKind::I32 if wire == WT_VARINT =>
                    raw = Some(read_varint(buf, &mut pos)? as u32 as i32 as f64),
                ValKind::U32 if wire == WT_VARINT =>
                    raw = Some(read_varint(buf, &mut pos)? as u32 as f64),
                ValKind::Bool if wire == WT_VARINT =>
                    raw = Some(if read_varint(buf, &mut pos)? != 0 { 1.0 } else { 0.0 }),
                ValKind::Str if wire == WT_LEN => {
                    let len = read_varint(buf, &mut pos)? as usize;
                    let end = pos.checked_add(len)?;
                    let s = buf.get(pos..end)?;
                    pos = end;
                    text = Some(std::str::from_utf8(s).ok()?.to_string());
                }
                _ => { if !skip_field(buf, &mut pos, wire) { return None; } }
            }
        } else if !skip_field(buf, &mut pos, wire) {
            return None;
        }
    }
    Some((ts, raw, text))
}

fn emit(plan: &FastPlan, ts_opt: Option<i64>, raw: Option<f64>, text: Option<String>, store: &dyn ChannelStore) -> bool {
    // resolve_ts already maps non-positive/None → now_ns().
    let ts = resolve_ts(ts_opt);
    match plan.sample_type {
        SampleType::Text => match text {
            Some(s) => { store.write_text(plan.id, ts, s); true }
            None => false,
        },
        st => match raw {
            Some(r) => {
                let scaled = r * plan.eu_scale + plan.eu_offset;
                let nv = match st {
                    SampleType::Float => NumericVal::Float(scaled),
                    SampleType::Int => NumericVal::Int(scaled as i64),
                    SampleType::Bool => NumericVal::Bool(r != 0.0),
                    SampleType::Text => return false,
                };
                store.write_numeric(plan.id, ts, nv);
                true
            }
            None => false,
        },
    }
}

fn decode_fast(data: &[u8], plan: &FastPlan, store: &dyn ChannelStore) -> usize {
    match plan.repeated_field {
        None => {
            match scan_one(data, plan) {
                Some((ts, raw, text)) => usize::from(emit(plan, ts, raw, text, store)),
                None => 0,
            }
        }
        Some(rep) => {
            let mut pos = 0;
            let mut count = 0;
            while pos < data.len() {
                let Some(key) = read_varint(data, &mut pos) else { break };
                let field = (key >> 3) as u32;
                let wire = (key & 0x7) as u8;
                if field == rep && wire == WT_LEN {
                    let Some(len) = read_varint(data, &mut pos) else { break };
                    let end = match pos.checked_add(len as usize) { Some(e) => e, None => break };
                    let Some(elem) = data.get(pos..end) else { break };
                    pos = end;
                    if let Some((ts, raw, text)) = scan_one(elem, plan) {
                        if emit(plan, ts, raw, text, store) { count += 1; }
                    }
                } else if !skip_field(data, &mut pos, wire) {
                    break;
                }
            }
            count
        }
    }
}

pub fn decode_batch(data: &[u8], bindings: &[ChannelBinding], store: &dyn ChannelStore) -> usize {
    if bindings.is_empty() {
        return 0;
    }
    let mut total = 0;
    let mut reflected: Option<DynamicMessage> = None;
    for binding in bindings {
        if let Some(plan) = &binding.fast {
            total += decode_fast(data, plan, store);
        } else {
            if reflected.is_none() {
                match DynamicMessage::decode(bindings[0].msg_desc.clone(), data) {
                    Ok(m) => reflected = Some(m),
                    Err(e) => { eprintln!("ingest: proto decode error: {e}"); return total; }
                }
            }
            total += decode_channel(reflected.as_ref().unwrap(), binding, store);
        }
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
pub(crate) fn resolve_ts(ts: Option<i64>) -> i64 {
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

    #[test]
    fn read_varint_reads_multibyte_and_advances() {
        // 300 = 0xAC 0x02 in protobuf varint.
        let buf = [0xACu8, 0x02, 0xFF];
        let mut pos = 0;
        assert_eq!(super::read_varint(&buf, &mut pos), Some(300));
        assert_eq!(pos, 2);
        // Truncated varint (continuation bit set, no next byte) → None.
        let mut pos2 = 0;
        assert_eq!(super::read_varint(&[0x80], &mut pos2), None);
    }

    #[test]
    fn fast_plan_build_flat_and_unsupported() {
        let (schema, _dir, registry) = make_schema_and_registry();
        let router = TopicRouter::build(&registry, &schema);
        let b = &router.bindings_for("accel")[0]; // val_path ["samples","x"], batch shape
        let plan = super::FastPlan::build(
            &b.msg_desc, &b.val_path, &b.ts_path, b.sample_type, b.eu_scale, b.eu_offset, b.id,
        )
        .expect("AccelBatch is a supported batch shape");
        assert!(plan.repeated_field.is_some());
        assert_eq!(plan.ts_field, 1); // t_ns = 1
        assert_eq!(plan.val_field, 2); // x = 2 in the Sample message

        // Unsupported: a 3-deep val_path returns None (fallback path).
        let deep = vec!["a".to_string(), "b".to_string(), "c".to_string()];
        assert!(super::FastPlan::build(
            &b.msg_desc, &deep, &deep, b.sample_type, 1.0, 0.0, b.id,
        )
        .is_none());
    }

    #[test]
    fn fast_path_matches_reflection_batch_float() {
        let (schema, _dir, registry) = make_schema_and_registry(); // eu_scale 2.0, eu_offset 1.0
        let router = TopicRouter::build(&registry, &schema);
        let bindings = router.bindings_for("accel");
        assert!(bindings[0].fast.is_some(), "AccelBatch must take the fast path");
        // t_ns = 0 exercises resolve_ts→now; use positive stamps for determinism.
        let data = encode_accel_batch(&schema, &[(1_000_000_000, 2.0), (2_000_000_000, 3.5)]);

        let store = LiveStore::from_registry(&registry);
        let n = decode_batch(&data, bindings, &store);
        assert_eq!(n, 2);
        let ch = registry.id("accel.x").unwrap();
        match store.snapshot(ch, ALL) {
            ChannelSnapshot::Float { ts, vals } => {
                assert_eq!(ts, vec![1_000_000_000, 2_000_000_000]);
                // EU: raw*2+1 → 5.0, 8.0
                assert!((vals[0] - 5.0).abs() < 1e-6 && (vals[1] - 8.0).abs() < 1e-6);
            }
            other => panic!("wrong variant: {other:?}"),
        }
    }

    #[test]
    fn fast_path_truncated_bytes_no_panic() {
        let (schema, _dir, registry) = make_schema_and_registry();
        let router = TopicRouter::build(&registry, &schema);
        let bindings = router.bindings_for("accel");
        let mut data = encode_accel_batch(&schema, &[(1_000_000_000, 2.0)]);
        data.truncate(data.len() - 1); // chop the last byte
        let store = LiveStore::from_registry(&registry);
        // Must not panic; may write 0 or a partial-but-bounded result.
        let _ = decode_batch(&data, bindings, &store);
    }

    /// Append a base-128 varint encoding of `v` to `buf`.
    fn push_varint(buf: &mut Vec<u8>, mut v: u64) {
        loop {
            let byte = (v & 0x7f) as u8;
            v >>= 7;
            if v == 0 {
                buf.push(byte);
                break;
            }
            buf.push(byte | 0x80);
        }
    }

    #[test]
    fn fast_path_huge_unknown_field_length_no_panic() {
        // Finding 1: an unknown trailing field (wire type LEN) whose length
        // varint encodes u64::MAX must not overflow `usize` in `skip_field`.
        let (schema, _dir, registry) = make_schema_and_registry();
        let router = TopicRouter::build(&registry, &schema);
        let bindings = router.bindings_for("accel");
        assert!(bindings[0].fast.is_some(), "AccelBatch must take the fast path");

        // One Sample: valid t_ns (field 1, varint) + x (field 2, fixed32),
        // followed by an unknown field 99 (wire type LEN) with a huge length.
        let mut sample = Vec::new();
        sample.push(0x08); // (1 << 3) | WT_VARINT
        push_varint(&mut sample, 1_000_000_000u64);
        sample.push(0x15); // (2 << 3) | WT_I32
        sample.extend_from_slice(&2.0f32.to_le_bytes());
        let unknown_tag: u64 = (99 << 3) | 2; // field 99, WT_LEN
        push_varint(&mut sample, unknown_tag);
        push_varint(&mut sample, u64::MAX); // huge length, no payload follows

        // Wrap as the `samples` repeated field (field 1, WT_LEN) of AccelBatch.
        let mut data = Vec::new();
        data.push(0x0A); // (1 << 3) | WT_LEN
        push_varint(&mut data, sample.len() as u64);
        data.extend_from_slice(&sample);

        let store = LiveStore::from_registry(&registry);
        // Must not panic. The corrupt trailing field aborts this sample, so
        // nothing gets written — but crucially, no overflow panic.
        let n = decode_batch(&data, bindings, &store);
        assert_eq!(n, 0);
    }

    fn make_sfixed_schema_and_registry() -> (ProtoSchema, tempfile::TempDir, ChannelRegistry) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("test_sfixed.proto");
        let mut f = std::fs::File::create(&path).unwrap();
        write!(f, r#"
syntax = "proto3";
message SFixedBatch {{
  repeated Sample samples = 1;
  message Sample {{
    int64 t_ns = 1;
    sfixed64 z = 2;
  }}
}}
"#).unwrap();
        let schema = ProtoSchema::from_path(&path).unwrap();
        let registry = ChannelRegistry::from_toml_str(r#"
[channels."accel.z"]
topic = "accelz"
proto_path = "SFixedBatch.samples.z"
ts_path = "SFixedBatch.samples.t_ns"
type = "int"
"#).unwrap();
        (schema, dir, registry)
    }

    fn encode_sfixed_batch(schema: &ProtoSchema, samples: &[(i64, i64)]) -> Vec<u8> {
        let pool = schema.pool_for_test();
        let batch_desc = pool.get_message_by_name("SFixedBatch").unwrap();
        let sample_desc = pool.get_message_by_name("SFixedBatch.Sample").unwrap();
        let t_field = sample_desc.get_field_by_name("t_ns").unwrap();
        let z_field = sample_desc.get_field_by_name("z").unwrap();
        let samples_field = batch_desc.get_field_by_name("samples").unwrap();

        let list: Vec<Value> = samples
            .iter()
            .map(|(t, z)| {
                let mut s = DynamicMessage::new(sample_desc.clone());
                s.set_field(&t_field, Value::I64(*t));
                s.set_field(&z_field, Value::I64(*z));
                Value::Message(s)
            })
            .collect();
        let mut batch = DynamicMessage::new(batch_desc);
        batch.set_field(&samples_field, Value::List(list));
        batch.encode_to_vec()
    }

    #[test]
    fn unsupported_val_kind_falls_back_to_reflection() {
        // Finding 2: sfixed64 must not be claimed by the fast scanner (it
        // arrives as wire type I64, not the varint the scanner reads for
        // integer kinds) — it must route to reflection and decode correctly.
        let (schema, _dir, registry) = make_sfixed_schema_and_registry();
        let router = TopicRouter::build(&registry, &schema);
        let bindings = router.bindings_for("accelz");
        assert!(bindings[0].fast.is_none(), "sfixed64 value kind must not take the fast path");

        let data = encode_sfixed_batch(&schema, &[(1_000_000_000, 42)]);
        let store = LiveStore::from_registry(&registry);
        let n = decode_batch(&data, bindings, &store);
        assert_eq!(n, 1);

        let ch = registry.id("accel.z").unwrap();
        match store.snapshot(ch, ALL) {
            ChannelSnapshot::Int { ts, vals } => {
                assert_eq!(ts, vec![1_000_000_000]);
                assert_eq!(vals, vec![42]);
            }
            other => panic!("wrong variant: {other:?}"),
        }
    }

    fn make_badts_schema_and_registry() -> (ProtoSchema, tempfile::TempDir, ChannelRegistry) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("test_badts.proto");
        let mut f = std::fs::File::create(&path).unwrap();
        write!(f, r#"
syntax = "proto3";
message BadTsBatch {{
  repeated Sample samples = 1;
  message Sample {{
    sfixed64 t_ns = 1;
    float x = 2;
  }}
}}
"#).unwrap();
        let schema = ProtoSchema::from_path(&path).unwrap();
        let registry = ChannelRegistry::from_toml_str(r#"
[channels."badts.x"]
topic = "badts"
proto_path = "BadTsBatch.samples.x"
ts_path = "BadTsBatch.samples.t_ns"
type = "float"
"#).unwrap();
        (schema, dir, registry)
    }

    fn encode_badts_batch(schema: &ProtoSchema, samples: &[(i64, f32)]) -> Vec<u8> {
        let pool = schema.pool_for_test();
        let batch_desc = pool.get_message_by_name("BadTsBatch").unwrap();
        let sample_desc = pool.get_message_by_name("BadTsBatch.Sample").unwrap();
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
    fn unsupported_ts_kind_falls_back_to_reflection() {
        // Finding 3: a ts field of a kind `scan_one`/`extract_ts` can't read
        // as a plain varint int (sfixed64 here) must not take the fast path,
        // even though the value field (float) is otherwise fully supported.
        let (schema, _dir, registry) = make_badts_schema_and_registry();
        let router = TopicRouter::build(&registry, &schema);
        let bindings = router.bindings_for("badts");
        assert!(bindings[0].fast.is_none(), "sfixed64 ts kind must not take the fast path");

        let data = encode_badts_batch(&schema, &[(1_000_000_000, 7.5)]);
        let store = LiveStore::from_registry(&registry);
        let n = decode_batch(&data, bindings, &store);
        assert_eq!(n, 1);

        let ch = registry.id("badts.x").unwrap();
        match store.snapshot(ch, ALL) {
            ChannelSnapshot::Float { ts, vals } => {
                assert_eq!(ts, vec![1_000_000_000]);
                assert!((vals[0] - 7.5).abs() < 1e-6);
            }
            other => panic!("wrong variant: {other:?}"),
        }
    }
}
