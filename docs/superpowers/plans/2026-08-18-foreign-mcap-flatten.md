# Foreign MCAP Channel Flattening Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** On load, auto-create one channel per scalar leaf field of any foreign (non-`{t_ns,value}`) MCAP protobuf topic, timestamped by the MCAP record `log_time`, so all channels appear in the tree with no config.

**Architecture:** Extend the `plan_file` reconstruction in `src/record/lazy/source.rs` with a third case that flattens a message's scalar leaves into `ChannelBinding`s (empty `ts_path` marks them). Extend the reflection decoder in `src/ingest/decode.rs` with a `log_time`-aware entry point and a deep-path walker. Design spec: `docs/superpowers/specs/2026-08-18-foreign-mcap-flatten-design.md`.

**Tech Stack:** Rust, `prost-reflect` 0.22 (`DynamicMessage`, `MessageDescriptor`, `Kind`, `Cardinality`, `Value`), `mcap` 0.8.

## Global Constraints

- Never add a co-author / self-attribution to commits or PRs.
- Never stage or commit `config.toml` (it stays dirty/untracked).
- **`ts_path.is_empty()` is the flatten marker.** Config and reconstruct bindings always have a non-empty `ts_path`; flatten bindings always have an empty one.
- **`log_time` is authoritative for flatten channels** — one `(log_time, value)` sample per message per leaf. No embedded timestamp is read.
- **Flatten is a fallback only.** It runs solely for topics that are neither config-bound nor `{t_ns,value}`-shaped (the branch that currently `continue`s). Config + reconstruct keep priority; existing files must not change.
- **Reflection only** for flatten (never the fast scanner): `FastPlan::build` must return `None` for an empty `ts_path`.
- **Backward compatibility:** the existing `decode_batch(data, bindings, store)` signature and behavior are preserved; the live path and all existing tests keep calling it unchanged.
- Sample-type mapping (matches existing `value_sample_type`): `double`/`float` → `Float`; `int32`/`int64`/`uint32`/`uint64`/`sint32`/`sint64`/`fixed32`/`fixed64`/`sfixed32`/`sfixed64` → `Int`; `bool` → `Bool`; `string` → `Text`. Enum/bytes/group/message-typed-leaf → not a leaf (skip / recurse).

---

### Task 1: Leaf enumeration

**Files:**
- Modify: `src/record/lazy/source.rs` (add `enumerate_leaves` + `scalar_sample_type` near `value_sample_type`, ~line 56)
- Test: `src/record/lazy/source.rs` (unit tests in the existing `#[cfg(test)] mod tests`)

**Interfaces:**
- Consumes: `prost_reflect::{Cardinality, Kind, MessageDescriptor}` (already imported at top of file), `crate::types::SampleType` (already imported).
- Produces: `pub(crate) fn enumerate_leaves(desc: &MessageDescriptor) -> Vec<(Vec<String>, SampleType)>` — each tuple is `(field-name path from the top message to the leaf, leaf sample type)`. Used by Task 3.

- [ ] **Step 1: Write the failing tests**

Add to `mod tests` in `src/record/lazy/source.rs`. Uses the existing `ProtoSchema` import pattern (see `first_message_name` tests / `make_proto_and_registry`). Add a small proto-building helper if none is in scope:

```rust
#[test]
fn enumerate_leaves_collects_scalars_recurses_singular_skips_repeated() {
    use crate::ingest::loader::ProtoSchema;
    use std::io::Write;
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("foreign.proto");
    let mut f = std::fs::File::create(&path).unwrap();
    write!(f, r#"
syntax = "proto3";
message Health {{
  uint64 scheduler_got_sync = 1;
  bool ok = 2;
  Inner cmd = 3;
  repeated uint64 samples = 4;
  message Inner {{
    string label = 1;
    int64 code = 2;
  }}
}}
"#).unwrap();
    let schema = ProtoSchema::from_path(&path).unwrap();
    let desc = schema.message_by_name("Health").unwrap();

    let mut leaves = super::enumerate_leaves(&desc);
    leaves.sort();
    let expected = vec![
        (vec!["cmd".to_string(), "code".to_string()], SampleType::Int),
        (vec!["cmd".to_string(), "label".to_string()], SampleType::Text),
        (vec!["ok".to_string()], SampleType::Bool),
        (vec!["scheduler_got_sync".to_string()], SampleType::Int),
    ];
    let mut expected_sorted = expected.clone();
    expected_sorted.sort();
    assert_eq!(leaves, expected_sorted, "repeated `samples` must be skipped; singular `cmd` recursed");
}

#[test]
fn enumerate_leaves_message_without_scalars_is_empty() {
    use crate::ingest::loader::ProtoSchema;
    use std::io::Write;
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("empty.proto");
    let mut f = std::fs::File::create(&path).unwrap();
    write!(f, r#"
syntax = "proto3";
message Wrap {{
  repeated Wrap children = 1;
}}
"#).unwrap();
    let schema = ProtoSchema::from_path(&path).unwrap();
    let desc = schema.message_by_name("Wrap").unwrap();
    assert!(super::enumerate_leaves(&desc).is_empty());
}
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test -p datavis --lib record::lazy::source::tests::enumerate_leaves -- --nocapture`
Expected: FAIL — `enumerate_leaves` not found.

- [ ] **Step 3: Implement `enumerate_leaves` + `scalar_sample_type`**

Add after `value_sample_type` (around line 56) in `src/record/lazy/source.rs`:

```rust
/// Sample type for a scalar protobuf kind, or None for non-scalar kinds
/// (enum, bytes, group, message).
fn scalar_sample_type(kind: &Kind) -> Option<SampleType> {
    Some(match kind {
        Kind::Double | Kind::Float => SampleType::Float,
        Kind::Int64 | Kind::Int32 | Kind::Uint64 | Kind::Uint32 | Kind::Sint64 | Kind::Sint32
        | Kind::Fixed64 | Kind::Fixed32 | Kind::Sfixed64 | Kind::Sfixed32 => SampleType::Int,
        Kind::Bool => SampleType::Bool,
        Kind::String => SampleType::Text,
        _ => return None, // Enum, Bytes, Message, Group
    })
}

/// Every scalar leaf field of `desc` as (field-name path, sample type). Recurses
/// into singular sub-messages; skips repeated fields, enums, bytes, and groups.
/// Depth-capped to guard against recursive schemas.
pub(crate) fn enumerate_leaves(desc: &MessageDescriptor) -> Vec<(Vec<String>, SampleType)> {
    let mut out = Vec::new();
    let mut prefix = Vec::new();
    walk_leaves(desc, &mut prefix, 0, &mut out);
    out
}

fn walk_leaves(
    desc: &MessageDescriptor,
    prefix: &mut Vec<String>,
    depth: usize,
    out: &mut Vec<(Vec<String>, SampleType)>,
) {
    if depth > 8 {
        return;
    }
    for f in desc.fields() {
        if f.cardinality() == Cardinality::Repeated {
            continue;
        }
        match f.kind() {
            Kind::Message(sub) => {
                prefix.push(f.name().to_string());
                walk_leaves(&sub, prefix, depth + 1, out);
                prefix.pop();
            }
            k => {
                if let Some(st) = scalar_sample_type(&k) {
                    let mut path = prefix.clone();
                    path.push(f.name().to_string());
                    out.push((path, st));
                }
            }
        }
    }
}
```

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test -p datavis --lib record::lazy::source::tests::enumerate_leaves`
Expected: PASS (both tests).

- [ ] **Step 5: Commit**

```bash
git add src/record/lazy/source.rs
git commit -m "feat(replay): enumerate scalar leaf fields of a protobuf message"
```

---

### Task 2: `log_time`-aware reflection decode

**Files:**
- Modify: `src/ingest/decode.rs` (`FastPlan::build` ~line 63; `decode_batch` ~line 258; `decode_channel` ~line 280; add `decode_deep_channel`)
- Test: `src/ingest/decode.rs` (unit tests in existing `mod tests`)

**Interfaces:**
- Consumes: `ChannelBinding` (fields incl. `val_path`, `ts_path`, `msg_desc`), `now_ns`, `resolve_ts`, `get_named_field`, `write_value` (all already in this file).
- Produces:
  - `pub fn decode_batch_at(data: &[u8], bindings: &[ChannelBinding], store: &dyn ChannelStore, log_time: i64) -> usize` — used by `decode_message` in Task 3.
  - `decode_batch` unchanged in signature; delegates to `decode_batch_at(.., now_ns())`.
  - Flatten binding contract: `ts_path.is_empty()` → the leaf at `val_path` (walked through singular messages) is written stamped with `log_time`.

- [ ] **Step 1: Write the failing tests**

Add to `mod tests` in `src/ingest/decode.rs`. A helper builds a foreign message and a flatten binding:

```rust
fn make_foreign_schema() -> (ProtoSchema, tempfile::TempDir) {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("foreign.proto");
    let mut f = std::fs::File::create(&path).unwrap();
    write!(f, r#"
syntax = "proto3";
message Health {{
  int64 code = 1;
  Inner cmd = 2;
  message Inner {{
    int64 depth_val = 1;
  }}
}}
"#).unwrap();
    (ProtoSchema::from_path(&path).unwrap(), dir)
}

fn encode_health(schema: &ProtoSchema, code: i64, depth_val: i64) -> Vec<u8> {
    use prost_reflect::prost::Message as _;
    let pool = schema.pool_for_test();
    let health = pool.get_message_by_name("Health").unwrap();
    let inner = pool.get_message_by_name("Health.Inner").unwrap();
    let mut im = DynamicMessage::new(inner.clone());
    im.set_field(&inner.get_field_by_name("depth_val").unwrap(), Value::I64(depth_val));
    let mut hm = DynamicMessage::new(health.clone());
    hm.set_field(&health.get_field_by_name("code").unwrap(), Value::I64(code));
    hm.set_field(&health.get_field_by_name("cmd").unwrap(), Value::Message(im));
    hm.encode_to_vec()
}

fn flatten_binding(schema: &ProtoSchema, val_path: &[&str], id: ChannelId) -> ChannelBinding {
    let desc = schema.message_by_name("Health").unwrap();
    ChannelBinding {
        id,
        fast: None,
        msg_desc: desc,
        val_path: val_path.iter().map(|s| s.to_string()).collect(),
        ts_path: Vec::new(), // flatten marker
        eu_scale: 1.0,
        eu_offset: 0.0,
        sample_type: SampleType::Int,
    }
}

#[test]
fn fast_plan_build_none_for_empty_ts_path() {
    let (schema, _d) = make_foreign_schema();
    let desc = schema.message_by_name("Health").unwrap();
    assert!(
        super::FastPlan::build(&desc, &["code".to_string()], &[], SampleType::Int, 1.0, 0.0, ChannelId(0)).is_none(),
        "empty ts_path must never take the fast path"
    );
}

#[test]
fn decode_deep_channel_depth1_stamps_log_time() {
    let (schema, _d) = make_foreign_schema();
    let registry = ChannelRegistry::from_toml_str("").unwrap();
    let store = LiveStore::from_registry(&registry);
    let id = ChannelId(0);
    let b = flatten_binding(&schema, &["code"], id);
    let data = encode_health(&schema, 7, 0);
    let n = super::decode_batch_at(&data, std::slice::from_ref(&b), &store, 1_234);
    assert_eq!(n, 1);
    match store.snapshot(id, ALL) {
        ChannelSnapshot::Int { ts, vals } => { assert_eq!(ts, vec![1_234]); assert_eq!(vals, vec![7]); }
        other => panic!("wrong variant: {other:?}"),
    }
}

#[test]
fn decode_deep_channel_depth2_walks_singular_message() {
    let (schema, _d) = make_foreign_schema();
    let registry = ChannelRegistry::from_toml_str("").unwrap();
    let store = LiveStore::from_registry(&registry);
    let id = ChannelId(0);
    let b = flatten_binding(&schema, &["cmd", "depth_val"], id);
    let data = encode_health(&schema, 0, 99);
    let n = super::decode_batch_at(&data, std::slice::from_ref(&b), &store, 5_000);
    assert_eq!(n, 1);
    match store.snapshot(id, ALL) {
        ChannelSnapshot::Int { ts, vals } => { assert_eq!(ts, vec![5_000]); assert_eq!(vals, vec![99]); }
        other => panic!("wrong variant: {other:?}"),
    }
}

#[test]
fn decode_deep_channel_missing_field_writes_proto3_default() {
    // `cmd` unset on the wire → walk yields the default (0) leaf, still written.
    let (schema, _d) = make_foreign_schema();
    let registry = ChannelRegistry::from_toml_str("").unwrap();
    let store = LiveStore::from_registry(&registry);
    let id = ChannelId(0);
    let b = flatten_binding(&schema, &["cmd", "depth_val"], id);
    let data = encode_health(&schema, 3, 0); // depth_val defaulted, cmd present-but-default
    let n = super::decode_batch_at(&data, std::slice::from_ref(&b), &store, 42);
    assert_eq!(n, 1);
    match store.snapshot(id, ALL) {
        ChannelSnapshot::Int { ts, vals } => { assert_eq!(ts, vec![42]); assert_eq!(vals, vec![0]); }
        other => panic!("wrong variant: {other:?}"),
    }
}
```

Note: `ChannelRegistry::from_toml_str("")` must yield an empty registry; `LiveStore` writes address channels by raw `ChannelId` (existing tests at `src/store/live.rs:308-309` write to `ChannelId(reg.len())` against a store built from an empty-ish registry). If an empty registry sizes the store to 0 slots and `write_numeric(ChannelId(0), ..)` panics with `channel id out of range`, instead build the registry with one dynamic channel first: `let registry = ChannelRegistry::from_toml_str("").unwrap(); let id = registry.add_dynamic("x", "x", SampleType::Int); let store = LiveStore::from_registry(&registry);` and use that `id`. Pick whichever the store API supports; the assertion targets are the `ts`/`vals` values, not the id.

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test -p datavis --lib ingest::decode::tests::decode_deep_channel`
Expected: FAIL — `decode_batch_at` not found.

- [ ] **Step 3: Add the empty-`ts_path` guard to `FastPlan::build`**

At the very top of `FastPlan::build` body (before the `match` at ~line 75) in `src/ingest/decode.rs`:

```rust
// Flatten bindings (empty ts_path) are reflection-only: their value comes from
// the MCAP log_time, not an embedded field, and their path may be arbitrary
// singular-message depth that the fast scanner does not walk.
if ts_path.is_empty() {
    return None;
}
```

- [ ] **Step 4: Split `decode_batch` into `decode_batch` + `decode_batch_at`**

Replace the existing `decode_batch` (lines ~258-278) with:

```rust
pub fn decode_batch(data: &[u8], bindings: &[ChannelBinding], store: &dyn ChannelStore) -> usize {
    decode_batch_at(data, bindings, store, now_ns())
}

/// Like `decode_batch`, but stamps flatten channels (empty `ts_path`) with the
/// caller's `log_time` instead of the message's own timestamp field.
pub fn decode_batch_at(
    data: &[u8],
    bindings: &[ChannelBinding],
    store: &dyn ChannelStore,
    log_time: i64,
) -> usize {
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
                    Err(e) => {
                        eprintln!("ingest: proto decode error: {e}");
                        return total;
                    }
                }
            }
            total += decode_channel(reflected.as_ref().unwrap(), binding, log_time, store);
        }
    }
    total
}
```

- [ ] **Step 5: Add the flatten dispatch + `decode_deep_channel`**

Change `decode_channel` (line ~280) to take `log_time` and dispatch flatten first:

```rust
fn decode_channel(msg: &DynamicMessage, binding: &ChannelBinding, log_time: i64, store: &dyn ChannelStore) -> usize {
    if binding.ts_path.is_empty() {
        return decode_deep_channel(msg, binding, log_time, store);
    }
    match binding.val_path.len() {
        2 => decode_batch_channel(msg, binding, store),
        1 => decode_single_channel(msg, binding, store),
        n => {
            eprintln!("ingest: unsupported field path depth {n} for channel {:?}", binding.val_path);
            0
        }
    }
}
```

Add `decode_deep_channel` next to it:

```rust
/// Walk `binding.val_path` through singular message fields to the leaf value and
/// write it stamped with `log_time`. Flatten channels carry no embedded ts, so
/// `log_time` (the MCAP record time) is authoritative. A missing intermediate or
/// leaf yields the proto3 default, matching the rest of the reflection path.
fn decode_deep_channel(msg: &DynamicMessage, binding: &ChannelBinding, log_time: i64, store: &dyn ChannelStore) -> usize {
    let mut it = binding.val_path.iter();
    let Some(first) = it.next() else { return 0 };
    let Some(mut cur) = get_named_field(msg, first) else { return 0 };
    for name in it {
        let Value::Message(m) = &cur else { return 0 };
        let Some(v) = get_named_field(m, name) else { return 0 };
        cur = v;
    }
    let ts = resolve_ts(Some(log_time));
    usize::from(write_value(binding, ts, &cur, store))
}
```

- [ ] **Step 6: Run the new tests and the full decode suite**

Run: `cargo test -p datavis --lib ingest::decode`
Expected: PASS — new flatten tests plus all pre-existing `decode` tests (fast/reflection parity, etc.) green, proving `decode_batch` behavior is unchanged.

- [ ] **Step 7: Commit**

```bash
git add src/ingest/decode.rs
git commit -m "feat(ingest): log_time-stamped deep-path reflection decode for flatten channels"
```

---

### Task 3: Wire flattening into load

**Files:**
- Modify: `src/record/lazy/source.rs` (`plan_file` else-branch ~lines 142-171; `decode_message` ~line 74; imports ~line 10)
- Test: `src/record/lazy/source.rs` (integration test in `mod tests`)

**Interfaces:**
- Consumes: `enumerate_leaves` (Task 1), `decode_batch_at` (Task 2), `registry.add_dynamic`, `registry.meta`.
- Produces: on load, foreign topics register N flatten channels named `topic/field/subfield` and decode one sample per message per leaf at `log_time`.

- [ ] **Step 1: Write the failing integration test**

Add to `mod tests` in `src/record/lazy/source.rs`. Writes a foreign MCAP (no `t_ns`/`value`; a nested singular message; a repeated field to prove it is skipped), then opens it and checks channels + samples. Model the writer on `write_test_mcap` (lines 366-414).

```rust
fn write_foreign_mcap(path: &std::path::Path, msgs: &[(i64, i64, i64)]) {
    use crate::ingest::loader::ProtoSchema;
    use prost_reflect::prost::Message as _;
    use prost_reflect::{DynamicMessage, Value};
    use std::borrow::Cow;
    use std::collections::BTreeMap;
    use std::io::Write;
    use std::sync::Arc;

    let dir = tempfile::tempdir().unwrap();
    let ppath = dir.path().join("foreign.proto");
    let mut f = std::fs::File::create(&ppath).unwrap();
    write!(f, r#"
syntax = "proto3";
message Health {{
  int64 code = 1;
  Inner cmd = 2;
  repeated uint64 samples = 3;
  message Inner {{
    int64 depth_val = 1;
  }}
}}
"#).unwrap();
    let schema = ProtoSchema::from_path(&ppath).unwrap();
    let pool = schema.pool_for_test();
    let health = pool.get_message_by_name("Health").unwrap();
    let inner = pool.get_message_by_name("Health.Inner").unwrap();

    let mcap_schema = Arc::new(mcap::Schema {
        name: "Health".to_string(),
        encoding: "protobuf".to_string(),
        data: Cow::Owned(schema.schema_bytes().to_vec()),
    });
    let channel = Arc::new(mcap::Channel {
        topic: "S1/HEALTH".to_string(),
        schema: Some(mcap_schema),
        message_encoding: "protobuf".to_string(),
        metadata: BTreeMap::new(),
    });
    let file = std::io::BufWriter::new(std::fs::File::create(path).unwrap());
    let mut writer = mcap::Writer::new(file).unwrap();
    for (log_time, code, depth_val) in msgs {
        let mut im = DynamicMessage::new(inner.clone());
        im.set_field(&inner.get_field_by_name("depth_val").unwrap(), Value::I64(*depth_val));
        let mut hm = DynamicMessage::new(health.clone());
        hm.set_field(&health.get_field_by_name("code").unwrap(), Value::I64(*code));
        hm.set_field(&health.get_field_by_name("cmd").unwrap(), Value::Message(im));
        let data = hm.encode_to_vec();
        writer.write(&mcap::Message {
            channel: channel.clone(),
            sequence: 0,
            log_time: *log_time as u64,
            publish_time: *log_time as u64,
            data: Cow::Owned(data),
        }).unwrap();
    }
    writer.finish().unwrap();
}

#[test]
fn foreign_topic_flattens_into_leaf_channels_stamped_by_log_time() {
    let registry = ChannelRegistry::from_toml_str("").unwrap();
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("foreign.mcap");
    write_foreign_mcap(&path, &[(1_000_000_000, 7, 11), (2_000_000_000, 8, 22)]);

    let src = RecordingSource::open(&path, &registry).unwrap();

    // Channels registered: two scalar leaves; the repeated `samples` skipped.
    let code_id = registry.id("S1/HEALTH/code").expect("code leaf channel");
    let depth_id = registry.id("S1/HEALTH/cmd/depth_val").expect("nested leaf channel");
    assert!(registry.id("S1/HEALTH/samples").is_none(), "repeated field must not become a channel");

    // Decode and check samples stamped by log_time.
    let metas = metas_of(&registry);
    let chunk = src.decode_chunk(0, &metas);
    let all = TimeWindow { start_ns: i64::MIN, end_ns: i64::MAX };
    match chunk.window(code_id.0 as usize, all) {
        ChannelSnapshot::Int { ts, vals } => {
            assert_eq!(ts, vec![1_000_000_000, 2_000_000_000]);
            assert_eq!(vals, vec![7, 8]);
        }
        other => panic!("wrong variant for code: {other:?}"),
    }
    match chunk.window(depth_id.0 as usize, all) {
        ChannelSnapshot::Int { ts, vals } => {
            assert_eq!(ts, vec![1_000_000_000, 2_000_000_000]);
            assert_eq!(vals, vec![11, 22]);
        }
        other => panic!("wrong variant for depth_val: {other:?}"),
    }
}
```

- [ ] **Step 2: Run the test to verify it fails**

Run: `cargo test -p datavis --lib record::lazy::source::tests::foreign_topic_flattens`
Expected: FAIL — `registry.id("S1/HEALTH/code")` is `None` (topic still hits the `continue`).

- [ ] **Step 3: Replace the final `else { continue }` in `plan_file` with flatten reconstruction**

In `src/record/lazy/source.rs`, the shape-detection `let (leaf_desc, val_path, ts_path) = if ... else if ... else { continue };` (lines 127-144). Replace the trailing `} else {\n            continue;\n        };` with the flatten block below. It builds and inserts all leaf bindings, then `continue`s (so the single-binding tail at lines 146-171 runs only for the flat/batch arms):

```rust
        } else {
            // Foreign structured message: not datavis's {t_ns, value} shape.
            // Flatten every scalar leaf into its own channel, timestamped by the
            // MCAP record log_time (these messages carry no embedded ts).
            let mut bindings = Vec::new();
            for (path, sample_type) in enumerate_leaves(&desc) {
                let name = format!("{topic}/{}", path.join("/"));
                let id = registry.add_dynamic(&name, topic, sample_type);
                if registry.meta(id).sample_type != sample_type {
                    eprintln!(
                        "replay: channel {name:?} already registered as {:?}, but this file records it as {:?}; skipping",
                        registry.meta(id).sample_type, sample_type
                    );
                    continue;
                }
                bindings.push(ChannelBinding {
                    id,
                    fast: None,
                    msg_desc: desc.clone(),
                    val_path: path,
                    ts_path: Vec::new(),
                    eu_scale: 1.0,
                    eu_offset: 0.0,
                    sample_type,
                });
            }
            if !bindings.is_empty() {
                reconstructed.insert(topic.clone(), bindings);
            }
            continue;
        };
```

- [ ] **Step 4: Thread `log_time` through `decode_message`**

Change the import at the top of `src/record/lazy/source.rs` (line 10) from `use crate::ingest::decode::decode_batch;` to `use crate::ingest::decode::decode_batch_at;`, and update `decode_message` (line 74):

```rust
    decode_batch_at(&msg.data, bindings, store, msg.log_time as i64);
```

(If `decode_batch` is referenced elsewhere in this file, keep both in the `use`.)

- [ ] **Step 5: Run the integration test**

Run: `cargo test -p datavis --lib record::lazy::source::tests::foreign_topic_flattens`
Expected: PASS.

- [ ] **Step 6: Run the whole suite (both feature sets)**

Run: `cargo test -p datavis` then `cargo test -p datavis --no-default-features`
Expected: PASS — no regression in existing replay/decode/store tests.

- [ ] **Step 7: Commit**

```bash
git add src/record/lazy/source.rs
git commit -m "feat(replay): flatten foreign MCAP topics into per-leaf channels"
```

---

## Self-Review

**1. Spec coverage:**
- Automatic fallback for unbound topics → Task 3 (replaces the `continue`). ✓
- One channel per scalar leaf, recursive singular, skip repeated → Task 1 `enumerate_leaves`. ✓
- Sample-type mapping incl. strings→Text → Task 1 `scalar_sample_type`. ✓
- Timestamp = `log_time`; empty `ts_path` marker → Task 2 (`decode_deep_channel`, dispatch) + Task 3 (`ts_path: Vec::new()`, `decode_batch_at(.., msg.log_time)`). ✓
- Reflection only; `FastPlan::build` None on empty ts → Task 2 Step 3. ✓
- Backward-compatible `decode_batch` → Task 2 Step 4 (delegation), verified by Step 6. ✓
- Naming `topic/field/subfield`, tree grouping → Task 3 Step 3 (`format!`/`join("/")`). ✓
- No `channel id out of range`: all `add_dynamic` calls happen in `plan_file` before the store is sized. ✓

**2. Placeholder scan:** No TBD/TODO; every code step has full code. The one conditional note (empty-registry store sizing in Task 2 Step 1) gives a concrete fallback. ✓

**3. Type consistency:** `enumerate_leaves(&MessageDescriptor) -> Vec<(Vec<String>, SampleType)>` produced in Task 1, consumed identically in Task 3. `decode_batch_at(&[u8], &[ChannelBinding], &dyn ChannelStore, i64) -> usize` produced in Task 2, called in Task 3 with `msg.log_time as i64`. `ChannelBinding` fields match `src/ingest/router.rs:9-18`. `ChannelSnapshot::Int { ts, vals }` matches `src/types.rs:105`. ✓
```
