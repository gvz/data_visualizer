# config.toml Reference

One `config.toml` in the working directory holds every section below. Each
concern parses the whole file and ignores sections it does not own, so sections
are optional and order-independent. This page is the terse lookup; each section
has a fuller chapter linked from its heading.

## Top level

```toml
default_window_s = 10.0   # default visible time span for time-series panels
```

## `[channels."name"]` — [Channels](../configuration/channels.md)

| Field | Required | Meaning |
|---|---|---|
| `topic` | yes | Source topic/subject |
| `proto_path` | proto sources | Value field path in the message |
| `ts_path` | proto sources | Nanosecond timestamp field path |
| `type` / `sample_type` | yes | `float`/`f64`, `int`/`i64`, `bool`, `text` |
| `unit` | no | Display unit |
| `color` | no | `#rrggbb` |
| `max_rate` | no | Expected Hz; sizes the ring (falls back to `[defaults]`) |
| `history_s` | no | Seconds retained (falls back to `[defaults]`) |
| `eu_scale` / `eu_offset` | no | Raw → engineering-unit scaling on ingest |
| `max_lines` | text only | Max retained log lines |

Bridge channels use `topic` + `sample_type` only (no `proto_path` / `ts_path`).

## `[defaults]` — [Defaults](../configuration/defaults.md)

```toml
[defaults]
max_rate  = 100000    # fallback Hz for channels omitting it
history_s = 5.0       # fallback seconds for channels omitting it
```

Only these two keys; per-channel values win. Omit for built-in defaults.

## `[screens.*]` — [Panels & Layout](../configuration/layout.md)

GUI-managed. `[screens.<name>]` carries `tiles_json` (opaque tile geometry) and
a list of `[[screens.<name>.panels]]` tables, each with a `type` and either a
`channel` or `channels` plus panel-specific keys.

## `[[sources.bridge]]` — [Bridge Adapters](../sources/bridge.md)

```toml
[[sources.bridge]]
name    = "vendor-x"
command = "/opt/vendor-x/adapter"
args    = ["--device", "/dev/tty0"]   # optional
```

One subprocess per entry. Its channels are declared as normal `[channels.*]`
with `topic` + `sample_type`.

## `[scripts]` / `[[scripts.instances]]` — [Scripting](../scripting/overview.md)

```toml
[scripts]
dir = "scripts"          # scripts dir, relative to config.toml
window_s = 10.0          # input window seconds

[[scripts.instances]]
id = "ch1_rms"
script = "sine_rms"
inputs  = ["load/ch0"]
outputs = [{ name = "scripts/rms", type = "float", unit = "" }]
enabled = true
```

`inputs` / `outputs` are optional (fall back to the script's declared defaults).

## `[recording]` — [Auto-Split](../recording/auto-split.md)

```toml
[recording]
max_file_mb = 512   # auto-split at this on-disk size; omit or 0 = single file
```
