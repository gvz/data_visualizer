# ZeroMQ + Protobuf

The ZeroMQ source is datavis's default live transport. It subscribes to a ZMQ
`SUB` endpoint and decodes each message against a Protobuf schema you supply.

## Enabling it

```bash
cargo run -- --endpoint tcp://localhost:5555 --schema schema.proto
```

- `--endpoint <ADDR>` — the ZMQ SUB endpoint. Default `tcp://localhost:5555`.
- `--schema <PATH>` — the `.proto` file describing the wire messages. Default
  `schema.proto`.

datavis compiles the `.proto` at startup (via `protox`) into a descriptor set
and decodes incoming messages reflectively with `prost-reflect` — you do not
compile schemas into the binary.

If the source fails to start (endpoint unreachable, schema missing), datavis
logs the error and keeps running without live data rather than aborting.

## Mapping messages to channels

Declare channels in `config.toml` with the proto field paths that carry the
value and timestamp:

```toml
[channels."sensor.acceleration.x"]
topic      = "accel"
proto_path = "AccelBatch.samples.x"
ts_path    = "AccelBatch.samples.t_ns"
type       = "float"
```

- `topic` matches the ZMQ topic the message arrives on.
- `proto_path` / `ts_path` are dotted paths into the decoded message. Repeated
  fields (batches of samples) are expanded — one channel sample per element.

See [Channels](../configuration/channels.md) for every field.

## The shared schema

All ZeroMQ channels share the one descriptor set compiled from `--schema`. When
you record, that same schema is embedded into the MCAP file, so a recording is
self-describing and replays without needing the original `.proto` on hand. See
[Recording to MCAP](../recording/recording.md).
