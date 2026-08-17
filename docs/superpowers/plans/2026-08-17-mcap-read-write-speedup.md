# MCAP Read/Write Speedup Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Make MCAP replay fast by (A) decoding samples with a hand-rolled protobuf field-number scanner instead of `prost-reflect`, and (B) batching per-sample MQTT/script frames into one repeated-field MCAP message per topic.

**Architecture:** Two independent, backward-compatible changes. Part A adds a `FastPlan` (field numbers + wire types resolved once per `ChannelBinding`) and a wire scanner in `src/ingest/decode.rs`, with the existing `prost-reflect` path kept as a fallback for shapes the scanner can't represent. Part B generates a batch schema per MQTT topic (`Batch { repeated Sample samples=1; Sample{t_ns=1,value=2} }`), has the recorder byte-concatenate producer-encoded `Sample` messages into one MCAP message flushed on size/time, and teaches the reader to detect and decode the batched shape.

**Tech Stack:** Rust, `prost-reflect` 0.22 (DynamicMessage), `mcap` 0.8, `crossbeam-channel`, `memmap2`. Design doc: `docs/superpowers/specs/2026-08-17-mcap-read-write-speedup-design.md`.

## Global Constraints

- Never add self as commit co-author; no `Co-Authored-By`/`Claude-*` trailers on commits.
- Never stage or commit `config.toml` (it stays dirty/untracked in this repo).
- Sample protobuf field numbers are fixed: `t_ns = 1`, `value = 2`. Do not change them.
- Full backward compatibility: old (flat, one-message-per-sample) files must load and return exact samples unchanged. No migration step.
- Flush policy (Part B): flush a topic's buffer at **1024 samples OR 500 ms**, whichever first; flush **all** buffers on `finish`/`rotate`.
- `SPAN_PAD_NS = 500_000_000` (= max batch time span, `DYN_BATCH_MS`) — pads the chunk-selection **start** check in `overlapping`. Must not exceed the existing `overlapping_selects_intersecting_spans` test's 5e8 gap, so keep it at exactly 500 ms.
- Exact per-sample timestamps and values must be preserved through both changes; a read returns exactly the samples inside the requested window.
- Preserve `resolve_ts` semantics: a non-positive `t_ns` (proto3 unset) is stamped with `now_ns()`.
- Tests: run with `--no-default-features` where the crate is built that way in this repo's flows, but these unit/integration tests live under `cargo test` normally; run `cargo test <name>` per step.

---

### Task 1: Wire primitives + `FastPlan`

**Files:**
- Modify: `src/ingest/decode.rs` (add near top, below imports)

**Interfaces:**
- Produces:
  - `pub(crate) fn read_varint(buf: &[u8], pos: &mut usize) -> Option<u64>`
  - `#[derive(Clone, Copy)] pub(crate) enum ValKind { F64, F32, I64, I32, U64, U32, Bool, Str }`
  - ```rust
    pub(crate) struct FastPlan {
        pub repeated_field: Option<u32>, // Some(n) = batch shape, None = flat
        pub ts_field: u32,
        pub val_field: u32,
        pub val_kind: ValKind,
        pub sample_type: crate::types::SampleType,
        pub eu_scale: f64,
        pub eu_offset: f64,
        pub id: crate::types::ChannelId,
    }
    ```
  - `impl FastPlan { pub(crate) fn build(msg_desc: &MessageDescriptor, val_path: &[String], ts_path: &[String], sample_type: SampleType, eu_scale: f64, eu_offset: f64, id: ChannelId) -> Option<FastPlan> }`

- [ ] **Step 1: Write the failing test**

Add to the `tests` module in `src/ingest/decode.rs`:

```rust
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
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test -p datavis read_varint_reads_multibyte_and_advances fast_plan_build_flat_and_unsupported 2>&1 | tail -20`
Expected: FAIL — `read_varint`/`FastPlan` not found.

- [ ] **Step 3: Write minimal implementation**

Add to `src/ingest/decode.rs` (below the existing `use` lines). Note the extra import of `MessageDescriptor`, `Kind`, `Cardinality` from `prost_reflect`, and `ChannelId`:

```rust
use prost_reflect::{Cardinality, Kind, MessageDescriptor};
use crate::types::ChannelId;

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
pub(crate) enum ValKind { F64, F32, I64, I32, U64, U32, Bool, Str }

pub(crate) struct FastPlan {
    pub repeated_field: Option<u32>,
    pub ts_field: u32,
    pub val_field: u32,
    pub val_kind: ValKind,
    pub sample_type: SampleType,
    pub eu_scale: f64,
    pub eu_offset: f64,
    pub id: ChannelId,
}

fn kind_to_valkind(kind: &Kind) -> Option<ValKind> {
    Some(match kind {
        Kind::Double => ValKind::F64,
        Kind::Float => ValKind::F32,
        Kind::Int64 | Kind::Sint64 | Kind::Sfixed64 => ValKind::I64,
        Kind::Int32 | Kind::Sint32 | Kind::Sfixed32 => ValKind::I32,
        Kind::Uint64 | Kind::Fixed64 => ValKind::U64,
        Kind::Uint32 | Kind::Fixed32 => ValKind::U32,
        Kind::Bool => ValKind::Bool,
        Kind::String => ValKind::Str,
        _ => return None,
    })
}

impl FastPlan {
    pub(crate) fn build(
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
```

- [ ] **Step 4: Run test to verify it passes**

Run: `cargo test -p datavis read_varint_reads_multibyte_and_advances fast_plan_build_flat_and_unsupported 2>&1 | tail -20`
Expected: PASS (2 tests).

- [ ] **Step 5: Commit**

```bash
git add src/ingest/decode.rs
git commit -m "feat(decode): add varint reader and FastPlan field-number decode plan"
```

---

### Task 2: Wire scanner + `decode_batch` fast path with reflection fallback

**Files:**
- Modify: `src/ingest/decode.rs` (add scanner; branch `decode_batch`)
- Modify: `src/ingest/router.rs:9-17` (add `fast` field), `src/ingest/router.rs:42-50` (build it)
- Modify: `src/record/lazy/source.rs:131-142` (build `fast` on the reconstructed binding)

**Interfaces:**
- Consumes: `FastPlan`, `read_varint`, `ValKind` from Task 1.
- Produces:
  - `ChannelBinding` gains `pub fast: Option<FastPlan>`.
  - `fn decode_fast(data: &[u8], plan: &FastPlan, store: &dyn ChannelStore) -> usize` (private).
  - `decode_batch` uses `binding.fast` when present, else the existing reflection path.

- [ ] **Step 1: Write the failing test**

Add to the `tests` module in `src/ingest/decode.rs`. This asserts the fast path writes byte-identical samples to reflection, across the batch shape, with EU scale/offset:

```rust
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
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test -p datavis fast_path_matches_reflection_batch_float fast_path_truncated_bytes_no_panic 2>&1 | tail -25`
Expected: FAIL — `bindings[0].fast` field does not exist (compile error).

- [ ] **Step 3: Write minimal implementation**

(a) In `src/ingest/router.rs`, add the field and build it:

```rust
// struct ChannelBinding { ... existing fields ... }
    pub fast: Option<crate::ingest::decode::FastPlan>,
```

At the binding construction in `build` (after `sample_type: meta.sample_type,`), replace the struct literal so it computes `fast`:

```rust
let sample_type = meta.sample_type;
let binding = ChannelBinding {
    id,
    fast: crate::ingest::decode::FastPlan::build(
        &desc.msg_desc, &desc.val_path, &desc.ts_path,
        sample_type, cfg.eu_scale, cfg.eu_offset, id,
    ),
    msg_desc: desc.msg_desc,
    val_path: desc.val_path,
    ts_path: desc.ts_path,
    eu_scale: cfg.eu_scale,
    eu_offset: cfg.eu_offset,
    sample_type,
};
```

Make `FastPlan` and its fields `pub` (they are `pub(crate)` from Task 1 — change the `struct FastPlan` and `impl` visibility to `pub` so `router.rs` can name the type in a `pub` field; keep fields `pub`).

(b) In `src/record/lazy/source.rs`, where the reconstructed binding is built (`vec![ChannelBinding { id, msg_desc: desc, val_path: vec!["value".to_string()], ts_path: vec!["t_ns".to_string()], eu_scale: 1.0, eu_offset: 0.0, sample_type }]`), add `fast`:

```rust
vec![ChannelBinding {
    id,
    fast: crate::ingest::decode::FastPlan::build(
        &desc, &["value".to_string()], &["t_ns".to_string()],
        sample_type, 1.0, 0.0, id,
    ),
    msg_desc: desc,
    val_path: vec!["value".to_string()],
    ts_path: vec!["t_ns".to_string()],
    eu_scale: 1.0,
    eu_offset: 0.0,
    sample_type,
}]
```

(c) In `src/ingest/decode.rs`, add the scanner and branch `decode_batch`:

```rust
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
            *pos += len as usize;
            *pos <= buf.len()
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
```

Then branch `decode_batch` — replace its body so each binding takes its fast path when present, decoding the reflection message lazily only if some binding lacks a plan:

```rust
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
```

- [ ] **Step 4: Run test to verify it passes**

Run: `cargo test -p datavis --lib ingest::decode 2>&1 | tail -25`
Expected: PASS — all `decode` tests including the two new ones and the pre-existing `decode_batch_writes_eu_scaled_samples` (still green via the fast path).

- [ ] **Step 5: Commit**

```bash
git add src/ingest/decode.rs src/ingest/router.rs src/record/lazy/source.rs
git commit -m "feat(decode): fast field-number scanner with reflection fallback"
```

---

### Task 3: Batch schema generation in `DynamicProtoRegistry`

**Files:**
- Modify: `src/record/mqtt_schema.rs:107-124` (`build_entry`), `:139-145` (`encode`), `:15-21` (`TopicEntry`), `:188-215` (`build_file_descriptor`)

**Interfaces:**
- Consumes: nothing from earlier tasks.
- Produces: `record_frame`/`record_numeric` keep their signature `Option<(Arc<[u8]>, Vec<u8>)>` but now return `(batch_schema_bytes, sample_bytes)` where `sample_bytes` is one encoded `Sample` message. The batch schema's top message is `mqtt.<Name>` with `repeated Sample samples = 1;` and nested `Sample { int64 t_ns = 1; <type> value = 2; }`.

- [ ] **Step 1: Write the failing test**

Add to the `tests` module of `src/record/mqtt_schema.rs`:

```rust
#[test]
fn schema_is_batch_and_sample_bytes_roundtrip() {
    use prost_reflect::{DescriptorPool, DynamicMessage, Value};
    let mut reg = DynamicProtoRegistry::new();
    let (schema, sample_bytes) = reg.record_frame("sensors/x", 111, "2.5").unwrap();

    // Top message has a repeated `samples` field of a Sample submessage.
    let pool = DescriptorPool::decode(schema.as_ref()).unwrap();
    let batch = pool.get_message_by_name("mqtt.SensorsX").unwrap();
    let samples_f = batch.get_field_by_name("samples").unwrap();
    assert_eq!(samples_f.number(), 1);
    assert!(samples_f.is_list());
    let sample_desc = match samples_f.kind() {
        prost_reflect::Kind::Message(m) => m,
        k => panic!("samples not a message: {k:?}"),
    };
    assert_eq!(sample_desc.get_field_by_name("t_ns").unwrap().number(), 1);
    assert_eq!(sample_desc.get_field_by_name("value").unwrap().number(), 2);

    // sample_bytes decode as a Sample with t_ns=111, value=2.5.
    let s = DynamicMessage::decode(sample_desc, sample_bytes.as_slice()).unwrap();
    assert_eq!(s.get_field_by_name("t_ns").unwrap().as_i64(), Some(111));
    assert_eq!(s.get_field_by_name("value").unwrap().as_f64(), Some(2.5));
    let _ = Value::Bool(false);
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test -p datavis schema_is_batch_and_sample_bytes_roundtrip 2>&1 | tail -20`
Expected: FAIL — `mqtt.SensorsX` has no `samples` field (current schema is flat).

- [ ] **Step 3: Write minimal implementation**

In `src/record/mqtt_schema.rs`:

(a) `TopicEntry` — store the inner `Sample` descriptor for encoding:

```rust
struct TopicEntry {
    sample_type: SampleType,
    sample_descriptor: MessageDescriptor, // the nested Sample message
    schema_bytes: Arc<[u8]>,              // batch FileDescriptorSet
}
```

(b) `build_file_descriptor` — emit the batch container with a nested `Sample`:

```rust
fn build_file_descriptor(msg_name: &str, sample_type: SampleType) -> FileDescriptorProto {
    let value_type = match sample_type {
        SampleType::Float => Type::Double,
        SampleType::Int => Type::Int64,
        SampleType::Bool => Type::Bool,
        SampleType::Text => Type::String,
    };
    let field = |name: &str, number: i32, ty: Type, type_name: Option<&str>| FieldDescriptorProto {
        name: Some(name.to_string()),
        number: Some(number),
        label: Some(Label::Optional as i32),
        r#type: Some(ty as i32),
        type_name: type_name.map(|s| s.to_string()),
        ..Default::default()
    };
    let sample = DescriptorProto {
        name: Some("Sample".to_string()),
        field: vec![
            field("t_ns", 1, Type::Int64, None),
            field("value", 2, value_type, None),
        ],
        ..Default::default()
    };
    let samples = FieldDescriptorProto {
        name: Some("samples".to_string()),
        number: Some(1),
        label: Some(Label::Repeated as i32),
        r#type: Some(Type::Message as i32),
        type_name: Some(format!(".mqtt.{msg_name}.Sample")),
        ..Default::default()
    };
    FileDescriptorProto {
        name: Some(format!("mqtt/{msg_name}.proto")),
        package: Some("mqtt".to_string()),
        syntax: Some("proto3".to_string()),
        message_type: vec![DescriptorProto {
            name: Some(msg_name.to_string()),
            field: vec![samples],
            nested_type: vec![sample],
            ..Default::default()
        }],
        ..Default::default()
    }
}
```

(c) `build_entry` — resolve and cache the `Sample` descriptor:

```rust
fn build_entry(&mut self, topic: &str, sample_type: SampleType) -> Option<()> {
    let msg_name = self.unique_name(topic);
    let file = build_file_descriptor(&msg_name, sample_type);
    if self.pool.add_file_descriptor_proto(file.clone()).is_err() {
        return None;
    }
    let fq = format!("mqtt.{msg_name}.Sample");
    let sample_descriptor = self.pool.get_message_by_name(&fq)?;
    let schema_bytes: Arc<[u8]> = FileDescriptorSet { file: vec![file] }
        .encode_to_vec()
        .into();
    self.entries.insert(
        topic.to_string(),
        TopicEntry { sample_type, sample_descriptor, schema_bytes },
    );
    self.used_names.insert(msg_name);
    Some(())
}
```

(d) `encode` — build a `Sample` message (identical field numbers):

```rust
fn encode(entry: &TopicEntry, ts_ns: i64, value: Value) -> (Arc<[u8]>, Vec<u8>) {
    let mut msg = DynamicMessage::new(entry.sample_descriptor.clone());
    msg.set_field_by_name("t_ns", Value::I64(ts_ns));
    msg.set_field_by_name("value", value);
    (entry.schema_bytes.clone(), msg.encode_to_vec())
}
```

Update the two existing `decode_back(..., "mqtt.SensorsX")` style tests in this module: they decode the returned bytes as the top-level message, but the bytes are now a `Sample`. Change their expected message name to the nested `"mqtt.<Name>.Sample"` (e.g. `"mqtt.SensorsX.Sample"`, `"mqtt.ACount.Sample"`, `"mqtt.AFlag.Sample"`, `"mqtt.AMsg.Sample"`, `"mqtt.CalcOut.Sample"`, `"mqtt.TB.Sample"`) so they decode the `Sample` shape. The `name_collision_disambiguated` test checks message names in the schema pool — update its assertions to the batch top-message names (`mqtt.AB`, `mqtt.AB_2` still hold, since the top message keeps `<Name>`).

- [ ] **Step 4: Run test to verify it passes**

Run: `cargo test -p datavis --lib record::mqtt_schema 2>&1 | tail -25`
Expected: PASS — the new test plus the updated existing tests.

- [ ] **Step 5: Commit**

```bash
git add src/record/mqtt_schema.rs
git commit -m "feat(record): generate batch schema; encode one Sample per frame"
```

---

### Task 4: Reader detects and decodes the batched shape

**Files:**
- Modify: `src/record/lazy/source.rs:110-143` (`plan_file` reconstruction loop)

**Interfaces:**
- Consumes: batch schema shape from Task 3; `FastPlan::build` (already wired in Task 2 for the reconstructed binding — extend it to the batch `val_path`).
- Produces: a reconstructed `ChannelBinding` with `val_path=["samples","value"]`, `ts_path=["samples","t_ns"]` for batch-shaped topics; flat topics unchanged.

- [ ] **Step 1: Write the failing test**

Add to the `tests` module of `src/record/lazy/source.rs` a helper that writes a batched MCAP file using the Task 3 registry, then a test that loads it and checks exact samples. Add near the other test helpers:

```rust
/// Write a batched MCAP: many Samples per topic in one repeated-field message.
fn write_batched_mqtt_mcap(path: &std::path::Path, topic: &str, samples: &[(i64, &str)]) {
    use crate::record::mqtt_schema::DynamicProtoRegistry;
    use std::borrow::Cow;
    use std::collections::BTreeMap;
    use std::sync::Arc;

    let mut reg = DynamicProtoRegistry::new();
    let mut schema_bytes: Option<Arc<[u8]>> = None;
    let mut batch: Vec<u8> = Vec::new();
    let mut last_ts = 0i64;
    for (ts, payload) in samples {
        let (schema, sample_bytes) = reg.record_frame(topic, *ts, payload).unwrap();
        schema_bytes.get_or_insert(schema);
        // repeated Sample samples = 1  →  tag 0x0A, len, bytes
        batch.push(0x0A);
        let mut len = sample_bytes.len();
        loop {
            let mut b = (len & 0x7f) as u8;
            len >>= 7;
            if len != 0 { b |= 0x80; }
            batch.push(b);
            if len == 0 { break; }
        }
        batch.extend_from_slice(&sample_bytes);
        last_ts = *ts;
    }
    let channel = Arc::new(mcap::Channel {
        topic: topic.to_string(),
        schema: Some(Arc::new(mcap::Schema {
            name: topic.to_string(),
            encoding: "protobuf".to_string(),
            data: Cow::Owned(schema_bytes.unwrap().to_vec()),
        })),
        message_encoding: "protobuf".to_string(),
        metadata: BTreeMap::new(),
    });
    let file = std::io::BufWriter::new(std::fs::File::create(path).unwrap());
    let mut writer = mcap::Writer::new(file).unwrap();
    writer.write(&mcap::Message {
        channel, sequence: 0, log_time: last_ts as u64, publish_time: last_ts as u64,
        data: Cow::Owned(batch),
    }).unwrap();
    writer.finish().unwrap();
}

#[test]
fn batched_topic_reconstructs_and_decodes_all_samples() {
    let registry = ChannelRegistry::from_toml_str(r#"
[channels."accel.x"]
topic = "accel"
proto_path = "AccelBatch.samples.x"
ts_path = "AccelBatch.samples.t_ns"
type = "float"
"#).unwrap();
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("batched.mcap");
    write_batched_mqtt_mcap(&path, "home/temp", &[
        (1_000_000_000, "21.5"),
        (2_000_000_000, "22.0"),
        (3_000_000_000, "23.5"),
    ]);
    let src = RecordingSource::open(&path, &registry).unwrap();
    let id = registry.id("home/temp").expect("batched topic reconstructed");
    let chunk = src.decode_chunk(0, &metas_of(&registry));
    match chunk.window(id.0 as usize, TimeWindow { start_ns: i64::MIN, end_ns: i64::MAX }) {
        ChannelSnapshot::Float { ts, vals } => {
            assert_eq!(ts, vec![1_000_000_000, 2_000_000_000, 3_000_000_000]);
            assert!((vals[0] - 21.5).abs() < 1e-4 && (vals[2] - 23.5).abs() < 1e-4);
        }
        other => panic!("wrong variant: {other:?}"),
    }
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test -p datavis batched_topic_reconstructs_and_decodes_all_samples 2>&1 | tail -25`
Expected: FAIL — `home/temp` not reconstructed (current `plan_file` requires a top-level `t_ns`).

- [ ] **Step 3: Write minimal implementation**

In `src/record/lazy/source.rs`, replace the body of the reconstruction loop (the `for (topic, schema_bytes) in &topic_schemas { ... }` block) so it handles both shapes. Extract the shape decision into a helper and build the right paths:

```rust
for (topic, schema_bytes) in &topic_schemas {
    if !router.bindings_for(topic).is_empty() {
        continue;
    }
    let Some(full) = first_message_name(schema_bytes) else { continue };
    let Some(desc) = merged.message_by_name(&full) else { continue };

    // Determine shape: flat (top-level t_ns+value) or batch (repeated Sample).
    let (leaf_desc, val_path, ts_path) = if desc.get_field_by_name("t_ns").is_some()
        && desc.get_field_by_name("value").is_some()
    {
        (desc.clone(), vec!["value".to_string()], vec!["t_ns".to_string()])
    } else if let Some((rep_name, sub)) = desc.fields().find_map(|f| {
        if f.cardinality() == prost_reflect::Cardinality::Repeated {
            if let prost_reflect::Kind::Message(m) = f.kind() {
                if m.get_field_by_name("t_ns").is_some() && m.get_field_by_name("value").is_some() {
                    return Some((f.name().to_string(), m));
                }
            }
        }
        None
    }) {
        (sub, vec![rep_name.clone(), "value".to_string()], vec![rep_name, "t_ns".to_string()])
    } else {
        continue;
    };

    let Some(sample_type) = value_sample_type(&leaf_desc) else { continue };
    let id = registry.add_dynamic(topic, topic, sample_type);
    if registry.meta(id).sample_type != sample_type {
        eprintln!(
            "replay: topic {:?} already registered as {:?}, but this file records it as {:?}; skipping",
            topic, registry.meta(id).sample_type, sample_type
        );
        continue;
    }
    reconstructed.insert(
        topic.clone(),
        vec![ChannelBinding {
            id,
            fast: crate::ingest::decode::FastPlan::build(
                &desc, &val_path, &ts_path, sample_type, 1.0, 0.0, id,
            ),
            msg_desc: desc,
            val_path,
            ts_path,
            eu_scale: 1.0,
            eu_offset: 0.0,
            sample_type,
        }],
    );
}
```

Note: `value_sample_type` reads the `value` field of whatever descriptor it's given, so passing `leaf_desc` (the `Sample` submessage for batch, or the top message for flat) is correct. `FastPlan::build` is given the top-level `desc` as `msg_desc` and the 2-deep `val_path`, matching Task 2's batch branch. Add `use prost_reflect::Cardinality;` etc. as needed, or fully-qualify as shown.

Requires `Cardinality`, `Kind` in scope — they are already imported (`use prost_reflect::{Kind, MessageDescriptor};`); add `Cardinality`.

- [ ] **Step 4: Run test to verify it passes**

Run: `cargo test -p datavis --lib record::lazy::source 2>&1 | tail -25`
Expected: PASS — the new batched test plus existing flat-shape tests (`mqtt_topic_reconstructed_from_embedded_schema` still passes, since flat detection is preserved).

- [ ] **Step 5: Commit**

```bash
git add src/record/lazy/source.rs
git commit -m "feat(replay): detect and decode batched repeated-Sample topics"
```

---

### Task 5: Recorder buffers and flushes per-topic batches

**Files:**
- Modify: `src/record/writer.rs:38-50` (`McapRecorder` fields), `:141-165` (`write_dynamic`), `:167-171` (`finish`), `:193-224` (`recorder_loop`)

**Interfaces:**
- Consumes: producers still call `write_dynamic(topic, schema_bytes, sample_bytes, ts)` via `RecordMsg::DynamicProto` (unchanged shape); `sample_bytes` is now one `Sample` (Task 3).
- Produces: on disk, one MCAP message per topic per flush carrying `repeated Sample samples=1` bytes; flush at 1024 samples OR 500 ms; all buffers flushed on `finish`/`rotate`.

- [ ] **Step 1: Write the failing test**

Replace/extend the `writer.rs` test module. Add an end-to-end test that records many single samples on one MQTT topic and asserts (a) the file has far fewer MCAP messages than samples, and (b) replay returns every sample. Use the public `start_recording` + `record_channel` path if available, else drive `McapRecorder` directly. Concretely, drive the recorder loop via the queue:

```rust
#[test]
fn dynamic_samples_are_batched_and_replay_exactly() {
    use crate::record::mqtt_schema::DynamicProtoRegistry;
    use crate::record::queue::RecordMsg;
    use crate::config::ChannelRegistry;
    use std::sync::Arc;

    let dir = tempfile::tempdir().unwrap();
    // Minimal registry (no MQTT topic pre-registered).
    let registry = ChannelRegistry::from_toml_str(r#"
[channels."accel.x"]
topic = "accel"
proto_path = "M.samples.x"
ts_path = "M.samples.t_ns"
type = "float"
"#).unwrap();

    // Open a recorder directly on a known path (no rotation).
    let cfg = RecorderCfg {
        output_dir: dir.path().to_path_buf(),
        session_secs: 12345,
        schema_bytes: b"".to_vec(),
        zmq_topics: vec![],
        max_bytes: None,
    };
    let mut rec = McapRecorder::open(&cfg, 0).unwrap();

    // 3000 single samples on one MQTT topic → should batch to 3 messages.
    let mut proto = DynamicProtoRegistry::new();
    let n = 3000i64;
    for i in 1..=n {
        let (schema, data) = proto.record_numeric(
            "home/temp", i, crate::types::SampleType::Float,
            crate::types::NumericVal::Float(i as f64),
        ).unwrap();
        rec.write_dynamic("home/temp", &schema, &data, i).unwrap();
    }
    let path = cfg.part_path(0);
    rec.finish().unwrap(); // must flush the partial buffer

    // Count MCAP messages: ceil(3000/1024) = 3.
    let bytes = std::fs::read(&path).unwrap();
    let msg_count = mcap::MessageStream::new(&bytes).unwrap().count();
    assert_eq!(msg_count, 3, "3000 samples must batch to 3 messages, got {msg_count}");

    // Replay returns all 3000 samples with exact ts/value.
    let store = crate::record::playback::PlaybackStore::load(&path, &registry).unwrap();
    let id = registry.id("home/temp").unwrap();
    let all = TimeWindow { start_ns: i64::MIN, end_ns: i64::MAX };
    match crate::store::ChannelStore::snapshot(store.as_ref(), id, all) {
        ChannelSnapshot::Float { ts, vals } => {
            assert_eq!(ts.len(), 3000);
            assert_eq!(ts[0], 1);
            assert_eq!(*ts.last().unwrap(), 3000);
            assert!((vals[2999] - 3000.0).abs() < 1e-6);
        }
        other => panic!("wrong variant: {other:?}"),
    }
}
```

Add the needed `use` lines (`crate::types::TimeWindow`, `ChannelSnapshot`) to the test module.

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test -p datavis dynamic_samples_are_batched_and_replay_exactly 2>&1 | tail -25`
Expected: FAIL — currently each `write_dynamic` writes one MCAP message (3000 messages, not 3).

- [ ] **Step 3: Write minimal implementation**

In `src/record/writer.rs`:

(a) Add a per-topic buffer type and swap the `dynamic_channel_ids` field:

```rust
const DYN_BATCH_SAMPLES: usize = 1024;
const DYN_BATCH_MS: u64 = 500;

struct DynBuf {
    channel_id: u16,
    samples: Vec<u8>,   // concatenated repeated-Sample elements (tag+len+bytes)
    count: usize,
    deadline: Instant,  // flush at-or-after this instant
    last_log_time: i64,
}
```

Replace `dynamic_channel_ids: HashMap<String, u16>` with `dynamic: HashMap<String, DynBuf>` in `McapRecorder` and its initializer (`dynamic: HashMap::new()`).

(b) Rewrite `write_dynamic` to register-then-buffer (append the repeated element), flushing when full:

```rust
fn write_dynamic(&mut self, topic: &str, schema_bytes: &[u8], sample_bytes: &[u8], log_time_ns: i64) -> anyhow::Result<()> {
    if !self.dynamic.contains_key(topic) {
        let schema = Arc::new(mcap::Schema {
            name: topic.to_string(),
            encoding: "protobuf".to_string(),
            data: Cow::Owned(schema_bytes.to_vec()),
        });
        let channel = mcap::Channel {
            topic: topic.to_string(),
            schema: Some(schema),
            message_encoding: "protobuf".to_string(),
            metadata: BTreeMap::new(),
        };
        let channel_id = self.writer.add_channel(&channel)?;
        self.dynamic.insert(topic.to_string(), DynBuf {
            channel_id, samples: Vec::new(), count: 0,
            deadline: Instant::now() + Duration::from_millis(DYN_BATCH_MS),
            last_log_time: log_time_ns,
        });
    }
    let buf = self.dynamic.get_mut(topic).unwrap();
    if buf.count == 0 {
        buf.deadline = Instant::now() + Duration::from_millis(DYN_BATCH_MS);
    }
    // repeated Sample samples = 1  →  tag 0x0A, varint len, bytes.
    buf.samples.push(0x0A);
    write_varint(&mut buf.samples, sample_bytes.len() as u64);
    buf.samples.extend_from_slice(sample_bytes);
    buf.count += 1;
    buf.last_log_time = log_time_ns;
    if buf.count >= DYN_BATCH_SAMPLES {
        self.flush_dynamic(topic)?;
    }
    Ok(())
}

/// Flush one topic's buffered batch as a single MCAP message.
fn flush_dynamic(&mut self, topic: &str) -> anyhow::Result<()> {
    let Some(buf) = self.dynamic.get_mut(topic) else { return Ok(()) };
    if buf.count == 0 {
        return Ok(());
    }
    let channel_id = buf.channel_id;
    let log_time = buf.last_log_time;
    let data = std::mem::take(&mut buf.samples);
    buf.count = 0;
    self.write_to_channel(channel_id, &data, log_time)
}

/// Flush every dynamic buffer whose deadline has passed.
fn flush_due_dynamic(&mut self) -> anyhow::Result<()> {
    let now = Instant::now();
    let due: Vec<String> = self.dynamic.iter()
        .filter(|(_, b)| b.count > 0 && now >= b.deadline)
        .map(|(t, _)| t.clone())
        .collect();
    for topic in due {
        self.flush_dynamic(&topic)?;
    }
    Ok(())
}

/// Flush all dynamic buffers (used on finish/rotate).
fn flush_all_dynamic(&mut self) -> anyhow::Result<()> {
    let topics: Vec<String> = self.dynamic.keys().cloned().collect();
    for topic in topics {
        self.flush_dynamic(&topic)?;
    }
    Ok(())
}
```

Add a `write_varint` helper near the top of the file:

```rust
fn write_varint(buf: &mut Vec<u8>, mut v: u64) {
    loop {
        let mut b = (v & 0x7f) as u8;
        v >>= 7;
        if v != 0 { b |= 0x80; }
        buf.push(b);
        if v == 0 { break; }
    }
}
```

(c) `finish` must flush all buffers first:

```rust
fn finish(mut self) -> anyhow::Result<()> {
    self.flush_all_dynamic()?;
    self.writer.flush()?;
    self.writer.finish()?;
    Ok(())
}
```

(d) `recorder_loop` — add a tick arm and flush-all before rotate. Add at the top of the loop a ticker and check `over_limit` after tick flushes:

```rust
let ticker = crossbeam_channel::tick(Duration::from_millis(100));
loop {
    crossbeam_channel::select! {
        recv(record_rx) -> result => match result {
            Ok(msg) => {
                write_record(&mut recorder, msg)?;
                if recorder.over_limit {
                    recorder.flush_all_dynamic()?; // don't lose buffered samples across rotate
                    part += 1;
                    recorder = rotate(recorder, &cfg, part)?;
                }
            }
            Err(_) => break,
        },
        recv(ticker) -> _ => {
            recorder.flush_due_dynamic()?;
            if recorder.over_limit {
                recorder.flush_all_dynamic()?;
                part += 1;
                recorder = rotate(recorder, &cfg, part)?;
            }
        }
        recv(stop_rx) -> _ => break,
    }
}
```

The drain-after-stop loop stays; `recorder.finish()` at the end flushes remaining buffers via (c). Note `rotate` calls `recorder.finish()` which now flushes-all, so the explicit `flush_all_dynamic()` before `rotate` is belt-and-suspenders — keep it only if it reads clearly; otherwise rely on `finish`. (Either is correct; do not double-write — `flush_dynamic` sets `count=0`, so a second flush is a no-op.)

- [ ] **Step 4: Run test to verify it passes**

Run: `cargo test -p datavis dynamic_samples_are_batched_and_replay_exactly 2>&1 | tail -25`
Then the whole writer module: `cargo test -p datavis --lib record::writer 2>&1 | tail -25`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add src/record/writer.rs
git commit -m "feat(record): batch dynamic samples per topic, flush on size/time"
```

---

### Task 6: Span-pad chunk selection for coarsened batch bounds

**Files:**
- Modify: `src/record/lazy/source.rs:233-241` (`overlapping`), add `SPAN_PAD_NS` const

**Interfaces:**
- Consumes: batched files from Task 5.
- Produces: `overlapping(window)` selects any chunk whose true samples fall in `window`, even when the chunk index's **start** bound is coarsened *late* by up to one batch span (batch `log_time` = last sample, so the first batch's index-start sits after the chunk's true earliest sample). The `end` bound is accurate (last batch's log_time is the true max), so only the start check is padded.

- [ ] **Step 1: Write the failing test**

Add to the `tests` module of `src/record/lazy/source.rs`. Build a batched file whose single chunk's index-`start_ns` is later than its earliest sample (because the batch `log_time` is the last sample), then read a narrow window at the leading edge:

```rust
#[test]
fn narrow_window_at_batch_leading_edge_returns_samples() {
    let registry = ChannelRegistry::from_toml_str(r#"
[channels."accel.x"]
topic = "accel"
proto_path = "AccelBatch.samples.x"
ts_path = "AccelBatch.samples.t_ns"
type = "float"
"#).unwrap();
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("edge.mcap");
    // One batch; log_time = last sample (300). Chunk index start_ns becomes 300,
    // but samples exist at 100/200/300.
    write_batched_mqtt_mcap(&path, "home/temp", &[
        (100, "1.0"), (200, "2.0"), (300, "3.0"),
    ]);
    let store = crate::record::playback::PlaybackStore::load(&path, &registry).unwrap();
    let id = registry.id("home/temp").unwrap();
    // Window [100,150) covers only the first sample, which sits before the
    // coarsened chunk start — without the pad this returns empty.
    let w = TimeWindow { start_ns: 100, end_ns: 150 };
    match crate::store::ChannelStore::snapshot(store.as_ref(), id, w) {
        ChannelSnapshot::Float { ts, .. } => assert_eq!(ts, vec![100]),
        other => panic!("wrong variant: {other:?}"),
    }
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test -p datavis narrow_window_at_batch_leading_edge_returns_samples 2>&1 | tail -25`
Expected: FAIL — `overlapping` excludes the chunk (its `start_ns` = 300 > `window.end_ns` = 150), snapshot returns empty.

- [ ] **Step 3: Write minimal implementation**

In `src/record/lazy/source.rs`, add the constant near the top:

```rust
/// Batch MCAP messages carry one `log_time` (the batch's last sample), so the
/// first batch's chunk-index start bound can sit up to one batch span AFTER the
/// chunk's true earliest sample. Pad the start check by that span so no
/// in-window chunk is skipped; per-sample clipping inside the chunk still
/// returns exact results. Kept at the max batch span (500 ms) so it does not
/// over-select on files with tightly spaced chunks.
const SPAN_PAD_NS: i64 = 500_000_000;
```

Update `overlapping` — pad the **start** comparison (the coarsened side); leave the accurate `end` comparison unchanged:

```rust
pub fn overlapping(&self, window: TimeWindow) -> Vec<usize> {
    // The index start can trail the true earliest sample by ≤ SPAN_PAD_NS, so a
    // span may contain in-window samples iff (start - pad) < window.end && end >= window.start.
    self.spans
        .iter()
        .enumerate()
        .filter(|(_, s)| {
            s.start_ns.saturating_sub(SPAN_PAD_NS) < window.end_ns && s.end_ns >= window.start_ns
        })
        .map(|(i, _)| i)
        .collect()
}
```

- [ ] **Step 4: Run test to verify it passes**

Run: `cargo test -p datavis narrow_window_at_batch_leading_edge_returns_samples 2>&1 | tail -25`
Then the module: `cargo test -p datavis --lib record::lazy::source 2>&1 | tail -25`
Expected: PASS — including the existing `overlapping_selects_intersecting_spans`. Verify its "before all data" case stays empty with the pad: window `{start:0,end:500_000_000}`, chunk span start `1e9` → `1e9 - 5e8 = 5e8`, and `5e8 < 5e8` is **false** (strict `<`), so the chunk is not selected. This is exactly why `SPAN_PAD_NS` is 500 ms (5e8) and not larger — a bigger pad would flip this to true and break the test.

- [ ] **Step 5: Commit**

```bash
git add src/record/lazy/source.rs
git commit -m "feat(replay): pad chunk selection for coarsened batch time bounds"
```

---

### Task 7: Full suite + before/after measurement

**Files:** none (verification only)

- [ ] **Step 1: Run the whole test suite**

Run: `cargo test 2>&1 | tail -30`
Expected: all green (default features). Also run `cargo test --no-default-features 2>&1 | tail -15` for the non-scripting build.

- [ ] **Step 2: Measure existing-file read speedup (Part A)**

Run against a real recording (Part A alone helps old files):

```bash
cargo build --release --example mcap_profile --no-default-features
./target/release/examples/mcap_profile recording_1785913919.mcap config.toml
```

Expected: scrub ms/read lower than the baseline 10.5 ms (Part A removes the ~37% decode+extract). Record the number.

- [ ] **Step 3: Measure new-file read speedup (Part B)**

Produce a batched recording, then profile it: run the app (or a small harness) to record the same synthetic MQTT load with this build, or reuse an existing capture flow. Compare messages-per-sample and scrub ms/read against a pre-batching file of similar size. Record both numbers in the design doc's efficiency section.

- [ ] **Step 4: Commit the measured numbers**

Append the before/after figures to `docs/superpowers/specs/2026-08-17-mcap-read-write-speedup-design.md` under a new "## Measured results" section, then:

```bash
git add docs/superpowers/specs/2026-08-17-mcap-read-write-speedup-design.md
git commit -m "docs: record measured MCAP read/write speedup"
```

---

## Notes for the implementer

- `cargo test -p datavis` and `cargo test` are equivalent here (single crate); either works.
- The reflection fallback in `decode_batch` MUST remain reachable — do not delete `decode_channel`, `decode_batch_channel`, `decode_single_channel`, `get_named_field`, or the `extract_*` helpers; the fast path does not cover every schema (e.g. a value field of a message/group kind returns `None` from `FastPlan::build`).
- Do not change `t_ns=1`, `value=2`, or the ZMQ `RecordMsg::Proto` path.
- `config.toml` is never staged.
