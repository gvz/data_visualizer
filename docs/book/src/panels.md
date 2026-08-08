# Panel Types

datavis draws channels in a workspace of panels. Each panel type accepts certain
sample types and is looked up through a registry keyed by the `type` string in
the layout. Bind a channel of the wrong type and the panel renders an inline
error — it never crashes.

## The panels

| `type` | Accepted sample types | What it shows |
|---|---|---|
| `waveform` | float, int, bool | Scrolling time-series with a configurable window, cursors and measurements |
| `spectrum` | float, int | FFT spectrum (via `rustfft`), configurable window function; warns on non-uniform timestamps |
| `numeric` | float, int, bool | A large single-value readout with a unit label |
| `gauge` | float, int | Arc / bar gauge with a configurable min/max |
| `xy` | float, int | Two channels as X/Y (index-aligned), Lissajous-style |
| `state_graph` | bool, int | Grafana-style coloured bands over time |
| `status` | text, int, bool | A single current-value state indicator (see below) |
| `log` | text | Scrolling, filterable timestamped log |

Single-channel panels (gauge, numeric, status, state graph) take a `channel`
key; multi-channel panels (waveform, spectrum, xy) take a `channels` list. See
[Panels & Layout](configuration/layout.md).

## Waveform

The workhorse time-series panel. Scrolls live, retains its configured window,
and supports cursors plus min/max/mean/RMS measurements over a selection. It has
a rich zoom gesture — see [Zooming & Cursors](zoom.md). Layout keys include
`cursors` and `dots`.

## Spectrum (FFT)

Computes an FFT over the channel's current window with `rustfft`, with a
configurable window function. It warns when the input timestamps are non-uniform
(the transform assumes uniform sampling).

## State graph

Renders `bool` / `int` channels as coloured horizontal bands over time — good
for discrete states and modes. It interns text/int/bool snapshots itself, so it
works on live and replayed data alike.

## Status

A single current-value indicator for discrete channels (`text`, `int`, `bool` —
**not** `float`, which has no discrete states). You map raw values to labelled,
coloured states in the layout:

```toml
type    = "status"
channel = "motor.state"
label   = "Motor"
states  = [
  { match = "2", label = "FAULT",   color = "#d62728" },
  { match = "1", label = "RUNNING", color = "#2ca02c" },
  { match = "0", label = "IDLE",    color = "#7f7f7f" },
  { match = "true", label = "ON",   color = "#2ca02c" },
]
```

- `match` — the raw value key (required). Booleans match `true` / `false`.
- `label` — display text; defaults to `match` when omitted.
- `color` — `#rrggbb` (required). Entries with a missing/malformed colour are
  skipped on load.

An unmatched value falls back to the raw value. For continuous quantities use a
`gauge` or `numeric` instead.

## Log

A scrolling, filterable view of a `text` channel's lines, each timestamped.
`max_lines` on the channel bounds retention.
