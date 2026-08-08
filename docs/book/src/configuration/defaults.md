# Defaults

The optional top-level `[defaults]` table supplies `max_rate` and `history_s`
for any channel that omits them. It saves you from repeating the same buffer
sizing on every channel when a whole file targets, say, 100 kHz.

```toml
[defaults]
max_rate  = 100000
history_s = 5.0

[channels."x"]
type = "float"        # inherits max_rate and history_s from [defaults]

[channels."y"]
type      = "float"
max_rate  = 1000      # a per-channel value always wins over [defaults]
```

## What it governs

`[defaults]` feeds both statically-configured channels **and**
runtime-discovered ones (an MQTT or WebSocket topic dragged onto a panel), so a
whole session can share one buffer-sizing policy.

Ring capacity is still computed the same way — `max_rate × history_s × 1.2` —
just with the resolved values.

## What it does *not* govern

Only `max_rate` and `history_s` are settable in `[defaults]`. Other fields keep
their per-field built-in defaults and cannot be set here:

- `color`, `unit`, `eu_scale`, `eu_offset`, `max_lines`

There is a single global table — no per-group or per-pattern defaults.

## Precedence

1. A per-channel `max_rate` / `history_s` in the channel's own table.
2. Otherwise the value from `[defaults]`.
3. Otherwise the built-in default (`1000` Hz / `10.0` s for configured
   channels).

Omitting `[defaults]`, or omitting either key within it, preserves the built-in
behaviour exactly. Either key may appear alone.
