# Architecture

This chapter sketches how datavis is put together, for readers who want to
extend it or understand why features behave the way they do.

## The `ChannelStore` seam

The central abstraction is the `ChannelStore` trait. Everything above it — every
panel — reads samples through this one interface and never learns whether the
data is live or replayed. Two implementations sit behind it:

- **`LiveStore`** — live ring buffers plus text history, fed by ingest sources.
- **`PlaybackStore`** — reads from memory-mapped MCAP file(s), decoding chunks on
  demand.

This seam is why recording, replay, and larger-than-RAM playback "just work"
through the same panels, and why the demo source is indistinguishable from a
real one.

## Module map

| Module | Responsibility |
|---|---|
| `ingest/` | Transports (ZMQ, MQTT, WebSocket, bridge), payload decoding, routing into the store |
| `store/` | Live ring buffers and text history behind `ChannelStore` |
| `viz/` | The `VizPanel` trait and every panel, looked up through a registry |
| `record/` | MCAP writer, playback store, lazy loader, MQTT schema capture |
| `config/` | Channel registry, layout, defaults, and section parsers |
| `script/` | The Python/numba engine, bindings, and script panel |
| `workspace.rs` / `app.rs` | The tiled UI and the eframe app that hosts it |

## Sources

Each transport implements a common `DataSource` trait: build, `spawn` onto the
store, and report a `conn_state`. The app collects sources into a list and
treats them uniformly, so they compose. The MQTT-family sources share a scalar
ingest helper.

## Panels

Panels implement `VizPanel` (title, accepted types, `config_ui`, `render`,
`serialize`) and are constructed through a `PanelRegistry` keyed by the layout's
`type` string. A factory pattern is used because a trait object cannot call a
`Sized` deserialize directly. Panels resolve channel *names* to ids through the
`ChannelRegistry`, so they never parse names themselves.

## Recording pipeline

Ingest sources push `RecordMsg` values onto a bounded queue. A dedicated
recorder thread drains it into an `mcap::Writer`:

- ZMQ messages carry the shared descriptor set.
- MQTT messages carry a per-topic generated schema (`DynamicProto`).

The writer flushes ~once a second and, on stop or [rollover](../recording/auto-split.md),
finalises the file with a summary and chunk index — the metadata the lazy
playback loader relies on.

## Playback pipeline

`PlaybackStore::load_many(paths, registry)` memory-maps each file, builds an
envelope of chunk locations and bounds, and reads the embedded schemas so replay
is self-describing. Reads (`snapshot`, `latest_at`) decode just the chunk(s)
covering the requested window. Text channels are held fully in RAM; numeric
channels use the on-demand envelope. See
[Larger-Than-RAM Playback](../recording/larger-than-ram.md).

## Threading

Ingest sources, the recorder, and the script engine each run on their own
threads and communicate with the UI through channels and shared state. The UI
renders at its own cadence (~60 fps), decoupled from sample rates that can reach
100 kHz per channel.

## Further reading

The per-feature design specs under `docs/superpowers/specs/` and the
implementation plans under `docs/superpowers/plans/` carry the detailed
rationale, decisions, and testing strategy behind each subsystem summarised
here.
