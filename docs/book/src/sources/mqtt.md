# MQTT

The MQTT source subscribes to a broker and ingests Protobuf payloads. Unlike the
ZeroMQ source, it discovers topics and builds their schema **on the fly** — you
do not have to enumerate every topic ahead of time.

## Enabling it

```bash
cargo run -- --mqtt-endpoint localhost:1883
```

`--mqtt-endpoint <ADDR>` takes `host:port` (or just `host`, defaulting to port
1883). Omit the flag to leave MQTT off.

## Runtime topic discovery

As publishes arrive, datavis tracks the set of discovered topics. A topic that
is not yet bound to a channel still appears in the sidebar's discovered list —
drag it onto a panel to start plotting it. Discovered channels use the
[`[defaults]`](../configuration/defaults.md) buffer sizing.

You can also bind MQTT topics explicitly in `config.toml`, the same way as any
other channel, if you want a fixed name, unit, colour, or EU scaling.

## On-the-fly schema for recording

Each MQTT topic carries its **own** generated Protobuf schema, built from the
payload the first time datavis sees the topic. The generated message has two
fields — a nanosecond timestamp (`t_ns`) and a typed `value`. When you record,
that per-topic schema is embedded into the MCAP file alongside the samples, so
MQTT recordings are self-describing and replay exactly, without the live broker.

Timestamps are stamped at receipt (`now_ns()`), and also written into the
message's `t_ns` field.

## Backpressure

If the record queue is full, the sample is dropped (standard non-blocking
`try_send` semantics) and the gap is accounted for — recording never blocks
ingest.
