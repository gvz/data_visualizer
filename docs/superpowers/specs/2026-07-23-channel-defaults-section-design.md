# Global `[defaults]` Section in channels.toml — Design

**Date:** 2026-07-23
**Status:** Approved for planning

## Goal

Add an optional top-level `[defaults]` table to `channels.toml` that supplies
`max_rate` and `history_s` for any channel that omits them — governing both
statically-configured channels and runtime-discovered (dropped MQTT/WebSocket)
channels. This lets a whole file target, e.g., 100 kHz without repeating the
value on every channel.

## Background

`channels.toml` deserializes into `ChannelsFile { channels: BTreeMap<String,
ChannelConfig> }` (`src/config/channels.rs`). `ChannelConfig` uses compile-time
serde field defaults: `max_rate` defaults to `1000`, `history_s` to `10.0`.
Ring capacity is derived downstream as `max_rate × history_s × 1.2`
(`src/store/live.rs`), so those two fields size the per-channel buffer.

Runtime-discovered channels (an MQTT/WebSocket topic dragged onto a panel) take
a separate path, `dynamic_channel` (`channels.rs:86`), which hardcodes
`max_rate = 100`, `history_s = 30.0`.

The only readers of `cfg.max_rate` / `cfg.history_s` are inside `channels.rs`
itself (mirroring into `ChannelMeta`); all other code reads the resolved values
via `meta().max_rate` / `meta().history_s`. So resolution can happen at parse
time with the change contained to `channels.rs`.

## Non-Goals

- Only `max_rate` and `history_s` are defaultable via `[defaults]`. Other
  fields (`color`, `unit`, `eu_scale`, `eu_offset`, `max_lines`) keep their
  existing per-field hardcoded defaults and are NOT settable in `[defaults]`.
- No per-group or per-pattern defaults; a single global table only.
- No change to how ring capacity is computed from the resolved values.

## Config Surface

```toml
[defaults]
max_rate  = 100000
history_s = 5.0

[channels."x"]
type = "float"        # inherits max_rate and history_s from [defaults]

[channels."y"]
type = "float"
max_rate = 1000       # per-channel value wins over [defaults]
```

The `[defaults]` table is optional. Omitting it — or omitting either key within
it — preserves current behavior exactly. Either key may appear alone.

## Precedence

Resolution order for each of `max_rate` and `history_s`:

1. The channel's own value, if present.
2. The `[defaults]` value, if present.
3. The hardcoded fallback.

The hardcoded fallback differs by channel origin (preserving today's behavior
when `[defaults]` is absent):

| Field       | Static fallback | Dynamic fallback |
|-------------|-----------------|------------------|
| `max_rate`  | `1000`          | `100`            |
| `history_s` | `10.0`          | `30.0`           |

- Static channel: `cfg.max_rate.or(defaults.max_rate).unwrap_or(1000)`;
  `cfg.history_s.or(defaults.history_s).unwrap_or(10.0)`.
- Dynamic channel: `defaults.max_rate.unwrap_or(100)`;
  `defaults.history_s.unwrap_or(30.0)` (a dynamic channel has no per-channel
  value in the file, so step 1 does not apply).

## Components (all in `src/config/channels.rs`)

### `ChannelDefaults`

```rust
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
struct ChannelDefaults {
    max_rate: Option<u32>,
    history_s: Option<f64>,
}
```

### `ChannelsFile`

Gains the table (kept `deny_unknown_fields`):

```rust
#[serde(default)]
defaults: ChannelDefaults,
```

### `ChannelConfig`

`max_rate` and `history_s` become optional so "omitted" is distinguishable from
"set to the default value":

```rust
#[serde(default)]
pub max_rate: Option<u32>,
#[serde(default)]
pub history_s: Option<f64>,
```

The `default_max_rate` / `default_history_s` functions are removed; the fallback
constants (`1000`, `10.0`, and the dynamic `100`, `30.0`) live in the
resolution helpers. `config()` continues to return `&ChannelConfig`, now
exposing the raw `Option`s, which honestly reflect the file; resolved values are
obtained via `meta()`.

### Resolution helpers

```rust
fn resolve_static_rate(cfg: Option<u32>, def: Option<u32>) -> u32 {
    cfg.or(def).unwrap_or(1000)
}
fn resolve_static_history(cfg: Option<f64>, def: Option<f64>) -> f64 {
    cfg.or(def).unwrap_or(10.0)
}
```

Applied in `from_toml_str` when building each `ChannelMeta`.

### Registry storage for the dynamic path

`ChannelRegistry` gains a field holding the parsed defaults:

```rust
defaults: ChannelDefaults,
```

`from_toml_str` stores `file.defaults` there. `dynamic_channel` (or its caller)
reads it and applies `defaults.max_rate.unwrap_or(100)` /
`defaults.history_s.unwrap_or(30.0)` instead of the current literals. If
`dynamic_channel` cannot easily see the registry, it takes `&ChannelDefaults`
(or the two resolved values) as a parameter.

## Data Flow

`channels.toml` → `toml::from_str` → `ChannelsFile { defaults, channels }` →
per static channel, resolve `max_rate`/`history_s` into `ChannelMeta` →
`LiveStore::from_registry` sizes rings from `meta`. Registry retains `defaults`;
on a runtime drop, `dynamic_channel` applies them to the new channel's `meta`.

## Error Handling

- Unknown key in `[defaults]`, or wrong value type → hard error from
  `toml::from_str`, wrapped by the existing `.context("parsing channels.toml")`.
- `max_rate = 0` or negative `history_s` → capacity computes to ≤ 0; `SoaRing::new`
  already clamps to a 16-slot minimum, so there is no panic. No extra validation
  is added (YAGNI); the clamp is the defined behavior.

## Testing Strategy

Unit tests in `channels.rs` using `ChannelRegistry::from_toml_str`:

- `[defaults]` applied when a channel omits both fields → `meta` reflects the
  `[defaults]` values.
- Per-channel value overrides `[defaults]`.
- `[defaults]` absent → static channel gets `1000` / `10.0` (unchanged).
- Partial `[defaults]` (only `max_rate`) → `history_s` falls to the static
  hardcoded `10.0`.
- `[defaults]` governs a runtime-registered dynamic channel → its `meta` uses
  the `[defaults]` values instead of `100` / `30.0`.
- Dynamic channel with `[defaults]` absent → still `100` / `30.0` (unchanged).
- Unknown field in `[defaults]` → `from_toml_str` returns `Err`.
```

