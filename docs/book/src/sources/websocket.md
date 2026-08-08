# WebSocket / Influx Line Protocol

The WebSocket source binds a WebSocket **server** and accepts InfluxDB line
protocol text frames. It is the easiest way to push data in from a script or a
device that already speaks line protocol, with no Protobuf schema required.

## Enabling it

```bash
cargo run -- --ws-listen 127.0.0.1:8086
```

`--ws-listen <ADDR>` binds a WebSocket server on `host:port` that receives line
protocol. Omit the flag to leave it off. Clients connect and stream text frames;
`conn_state` transitions to **LIVE** once a client connects.

## Line protocol

Each frame is one or more InfluxDB line-protocol measurements:

```
measurement,tag=value field1=1.23,field2=42i 1700000000000000000
```

### Topic mapping

datavis maps a `measurement` + `field` key onto a channel topic, so a single
measurement with several fields feeds several channels. Bind them in
`config.toml` by topic, or drag discovered topics onto panels at runtime, just
like MQTT.

### Field value normalisation

Line protocol field values are normalised to datavis sample types:

- Floats → `float`
- Integers (the trailing-`i` form) → `int`
- Booleans (`t`/`true`/`f`/`false`) → `bool`
- Quoted strings → `text`

### Timestamps

The optional trailing nanosecond timestamp is used when present; otherwise the
sample is stamped at receipt.

## When to use it

Reach for the WebSocket source when the producer already emits line protocol
(Telegraf, an Influx client library, a quick script) and you would rather not
define a Protobuf schema. For high-rate binary telemetry, prefer
[ZeroMQ](zmq.md) or [MQTT](mqtt.md).
