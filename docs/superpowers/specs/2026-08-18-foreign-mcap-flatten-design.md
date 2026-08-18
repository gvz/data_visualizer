# Foreign MCAP Channel Flattening — Design

**Date:** 2026-08-18
**Status:** approved for planning

## Problem

MCAP files recorded by *other* tools carry arbitrary structured protobuf
messages — one message type per topic, each with many scalar fields (see the
rocket-telemetry recording: 351 topics, messages like
`S1_EngineControlUnit1.SENSOR_VALUES`). datavis today only surfaces channels for
topics it can bind:

1. **Config/registry** — a `config.toml` channel entry with `proto_path` / `ts_path`.
2. **Reconstruct** — topics whose message is the datavis shape `{ t_ns, value }`
   (flat) or `repeated Sample { t_ns, value }` (batch).

A foreign message is neither, so `plan_file` hits the final `continue` and the
topic yields **zero channels** — the tree stays empty even though the file is
full of data. There is no config for these files (recorded elsewhere), so the
config route is unavailable by definition.

## Goal

On load, for every topic datavis can't otherwise bind, automatically create one
channel per scalar leaf field of its message, timestamped by the MCAP message
`log_time`, so all channels appear in the tree with no config.

## Data shape (observed)

- One message type per topic; mostly scalar fields
  (`double`/`float`/`int*`/`uint*`/`bool`/`string`).
- Shallow nesting: singular sub-messages ≤2 levels
  (e.g. `GROUND_SYSTEMS_OUTPUT_CONTROL_LV.OPERATOR_ENGINE_SEQUENCER_COMMAND_value.numeric_value`).
- **No repeated scalar/numeric arrays** in the sample file.
- Timestamps embedded inconsistently (some messages have `acu_timestamp`, most
  don't). The only universal, reliable time is the MCAP record `log_time`.

## Decisions

| Decision | Choice |
|----------|--------|
| Trigger | **Automatic fallback.** Fires only for topics that produce no channels today (config + `{t_ns,value}` reconstruct keep priority). Cannot regress existing files. |
| Timestamp | **MCAP `log_time`.** One `(log_time, value)` sample per message per leaf. |
| String fields | **Included** as text channels (state-graph style). |
| Repeated fields | **Skipped.** None in the data; revisit when a real file needs them. |
| Nested naming | Leaf channel name = `topic` + `/` + proto field path joined by `/`, so the existing `ChannelTree` (`split('/')`) groups them by topic then sub-message. |
| Decode path | **Reflection**, not the fast scanner. The whole message reflection-decodes once per topic; all its leaf bindings share that `DynamicMessage`. Correctness first; a fast flatten scanner can follow if high-rate topics bog down. |

## Leaf enumeration

Walk the message descriptor depth-first:

- **Scalar field** (`double`/`float`/`int32`/`int64`/`uint32`/`uint64`/`sint*`/
  `fixed*`/`sfixed*`/`bool`/`string`), cardinality singular → emit a leaf:
  - path = the field-name chain from the top message to this field
  - sample type: floating/integer kinds → `Float`/`Int` per existing
    `value_sample_type` mapping (double/float→Float, all integer kinds→Int),
    `bool`→`Bool`, `string`→`Text`.
- **Singular message field** → recurse into the sub-message, prefixing the path.
- **Repeated field** (any kind) → skip.
- **Enum / bytes / group** → skip (not plottable).
- Depth cap of 8 guards against pathological/recursive schemas.

A message with no scalar leaves contributes nothing.

## Wiring

### `plan_file` (`src/record/lazy/source.rs`)

Replace the final `else { continue }` (the branch reached when a topic is neither
config-bound nor `{t_ns,value}`-shaped) with flatten reconstruction:

```
for each scalar leaf (path, sample_type) in enumerate_leaves(top_desc):
    name = format!("{topic}/{}", path.join("/"))
    id   = registry.add_dynamic(&name, topic, sample_type)   // dedups by name
    push ChannelBinding {
        id,
        fast: None,               // flatten never uses the fast scanner
        msg_desc: top_desc.clone(),
        val_path: path,           // full field-name chain
        ts_path: vec![],          // EMPTY = "stamp with log_time"
        eu_scale: 1.0, eu_offset: 0.0,
        sample_type,
    }
```

All leaf bindings for a topic land in the same `reconstructed[topic]` vec, so
`decode_message` routes the message once through them.

`register_dynamic` already appends registry id + store slot under one lock, and
all registration happens in `plan_file` *before* the store is sized from the
registry — so no `channel id out of range`. Thousands of channels are fine
(append-only vectors).

### Decode (`src/ingest/decode.rs`)

- **`ts_path.is_empty()` marks a flatten binding.** Existing bindings always
  have a non-empty `ts_path`; flatten bindings have an empty one.
- **`FastPlan::build` returns `None` when `ts_path` is empty** — guard added so a
  depth-2 flatten path is never mis-read as the batch/repeated shape.
- **New `decode_batch_at(data, bindings, store, log_time)`**; `decode_batch`
  becomes `decode_batch_at(data, bindings, store, now_ns())`. Only the playback
  `decode_message` calls the `_at` form (passing `msg.log_time as i64`); the live
  `thread.rs` path and all existing tests keep calling `decode_batch` unchanged.
- **Dispatch in `decode_channel`:** if `binding.ts_path.is_empty()` →
  `decode_deep_channel(msg, binding, log_time, store)`; else the existing
  depth-1 / depth-2 logic (unchanged).
- **`decode_deep_channel`:** walk `val_path` through singular message fields to
  the leaf `Value`, then `write_value(binding, resolve_ts(Some(log_time)), &leaf, store)`.
  A missing intermediate/leaf yields the proto3 default (walk still succeeds),
  matching how existing reflection decoding treats defaults.

### `decode_message` (`src/record/lazy/source.rs`)

Thread `msg.log_time as i64` into `decode_batch_at`. The live ingest path is
unaffected.

## Non-goals

- Fast (hand-rolled) scanner for flatten channels — reflection only for now.
- Repeated-field expansion.
- Enum decoding to label strings, bytes fields.
- Config-driven selection/renaming of foreign channels.

## Testing

- `enumerate_leaves`: nested singular message → correct paths + sample types;
  repeated field skipped; string leaf → `Text`; bool → `Bool`; message with no
  scalar leaves → empty.
- `decode_deep_channel`: depth-1 and depth-2 singular paths write the leaf value
  stamped with the passed `log_time`; missing field → proto3 default written.
- `FastPlan::build` returns `None` for empty `ts_path`.
- `decode_batch` (no `_at`) still behaves exactly as today (regression: existing
  suite green).
- End-to-end in `plan_file`: a synthetic foreign message (no `t_ns`/`value`,
  nested sub-message, a string field) yields the expected set of channel names,
  and replaying two messages produces two samples per leaf at their `log_time`s.
```
