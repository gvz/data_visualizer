# Data Sources Overview

datavis ingests live data from several transports. They can run **concurrently** —
enable as many as you need — and all feed the same channel store. The status
indicator reads **LIVE** if *any* configured source is receiving data.

| Source | Wire format | How to enable | Chapter |
|---|---|---|---|
| ZeroMQ | Protobuf | `--endpoint` + `--schema` (default source) | [ZeroMQ](zmq.md) |
| MQTT | Protobuf, on-the-fly schema | `--mqtt-endpoint` | [MQTT](mqtt.md) |
| WebSocket | InfluxDB line protocol (text) | `--ws-listen` | [WebSocket](websocket.md) |
| Bridge | Custom, via subprocess | `[[sources.bridge]]` in config | [Bridge](bridge.md) |
| Demo | Synthesised | `--demo` | [Demo](demo.md) |

## The common source interface

Every transport implements one `DataSource` trait, giving them a shared
lifecycle: build, spawn onto the store, and report a `conn_state`. This is why
sources compose freely — the app pushes each onto a list and treats them
uniformly. The MQTT-family sources (MQTT, WebSocket) additionally share a scalar
ingest helper that writes a single decoded value into the store.

## Enabling several at once

Nothing stops you running, say, a ZeroMQ source *and* an MQTT source *and* a
bridge:

```bash
cargo run -- \
  --endpoint tcp://localhost:5555 --schema schema.proto \
  --mqtt-endpoint localhost:1883 \
  --ws-listen 127.0.0.1:8086
```

Bridges are added through `config.toml` rather than the command line, since a
bridge needs a command and arguments, and several may run at once. See
[Bridge Adapters](bridge.md).

> **Topic uniqueness.** Because all sources share one channel registry, a topic
> name must be unique across every coexisting source. Two sources cannot both
> claim topic `accel`.

## Recording is source-agnostic

Recording is available whenever **any** ingest source is active. Each source
feeds the same record queue, and the MCAP writer captures whatever schema each
message carries. See [Recording to MCAP](../recording/recording.md).
