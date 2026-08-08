# Channels

A **channel** is a named, typed stream of samples. The `[channels."name"]`
tables map raw source data — a Protobuf field, an MQTT topic, an Influx field —
onto channels you can drop on panels. The channel registry built from these
tables is what the whole app reasons about; panels resolve channel *names* to
ids through it and never parse names themselves.

## A Protobuf channel

For a ZeroMQ/Protobuf source you declare which message field carries the value
and which carries the timestamp:

```toml
[channels."sensor.acceleration.x"]
topic      = "accel"                       # source topic this field arrives on
proto_path = "AccelBatch.samples.x"        # value field path in the proto
ts_path    = "AccelBatch.samples.t_ns"     # timestamp field path (nanoseconds)
type       = "float"                       # float | int | bool | text
unit       = "m/s²"
color      = "#ff0000"
max_rate   = 100000                        # Hz — used to size the ring buffer
history_s  = 10.0                           # seconds of history retained
eu_scale   = 1.0                            # raw → engineering-unit scale
eu_offset  = 0.0                            # raw → engineering-unit offset
```

### Fields

| Field | Required | Meaning |
|---|---|---|
| `topic` | yes | Source topic/subject the data arrives on |
| `proto_path` | proto sources | Dotted path to the value field in the message |
| `ts_path` | proto sources | Dotted path to the nanosecond timestamp field |
| `type` | yes | Sample type: `float`, `int`, `bool`, or `text` |
| `unit` | no | Display unit label |
| `color` | no | `#rrggbb` line/marker colour |
| `max_rate` | no | Expected sample rate in Hz; sizes the ring buffer |
| `history_s` | no | Seconds of history kept in the live ring |
| `eu_scale` / `eu_offset` | no | Engineering-unit scaling applied on ingest |
| `max_lines` | text only | Max retained log lines for `type = "text"` |

Ring-buffer capacity is derived as `max_rate × history_s × 1.2`. If you omit
`max_rate` / `history_s`, per-channel values fall back to the file-wide
[`[defaults]`](defaults.md) (or the built-in `1000` Hz / `10.0` s).

## Sample types

| `type` | Rust type | Typical panels |
|---|---|---|
| `float` | `f64` | waveform, gauge, FFT, XY |
| `int` | `i64` | waveform, state graph, status |
| `bool` | boolean | waveform, state graph, status |
| `text` | string (log line) | log, status |

## A state / integer channel

```toml
[channels."motor.state"]
topic      = "status"
proto_path = "StatusBatch.samples.state"
ts_path    = "StatusBatch.samples.t_ns"
type       = "int"
max_rate   = 1000
history_s  = 30.0
```

## A text / log channel

```toml
[channels."system.log"]
topic      = "log"
proto_path = "LogBatch.samples.message"
ts_path    = "LogBatch.samples.t_ns"
type       = "text"
max_lines  = 500
```

## Engineering-unit scaling

`eu_scale` and `eu_offset` convert a raw source value into engineering units
*on ingest*, before it reaches the store: `eu = raw × eu_scale + eu_offset`.
Everything downstream — panels, cursors, recordings — sees the scaled value.

## Channels without a proto path

Not every source needs `proto_path` / `ts_path`:

- **MQTT / WebSocket** channels can be *discovered at runtime* — a topic dragged
  onto a panel — and never appear in `config.toml` at all. See
  [MQTT](../sources/mqtt.md) and [WebSocket](../sources/websocket.md).
- **Bridge** channels declare only `topic` + `sample_type`, because the bridge's
  payload schema is fixed and built in. See [Bridge Adapters](../sources/bridge.md).

> **Constraint:** topic names must be unique across coexisting sources. A bridge
> and a ZeroMQ source cannot both emit topic `accel` — they share one registry.
