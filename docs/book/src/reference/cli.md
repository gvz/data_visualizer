# Command-Line Options

```
datavis [OPTIONS]
```

Run `datavis --help` (or `-h`) to print this list.

| Option | Argument | Meaning |
|---|---|---|
| `--demo` | — | Run with the built-in demo source (no live inputs). |
| `--demo-freq` | `<HZ>` | Sine frequency for the demo source. Default `1.0`. |
| `--endpoint` | `<ADDR>` | ZMQ SUB endpoint for live proto data. Default `tcp://localhost:5555`. |
| `--schema` | `<PATH>` | Proto schema file for the ZMQ source. Default `schema.proto`. |
| `--mqtt-endpoint` | `<ADDR>` | MQTT broker as `host:port` (or `host`, port 1883). Enables the MQTT source; off when omitted. |
| `--ws-listen` | `<ADDR>` | Bind a WebSocket server (`host:port`) that receives InfluxDB line protocol. Off when omitted. |
| `-h`, `--help` | — | Print help and exit. |

## Notes

- Channels and layout share `config.toml`; the layout section persists there.
- The status indicator reads **LIVE** if any configured source is receiving
  data.
- Sources can be combined: pass `--endpoint`, `--mqtt-endpoint`, and
  `--ws-listen` together to run all three. Bridges are configured in
  `config.toml`, not on the command line (see
  [Bridge Adapters](../sources/bridge.md)).
- If `config.toml` is absent in the working directory, datavis offers to save
  the built-in default, load another file, or run with defaults once.

## Examples

```bash
# Fastest look — demo source
cargo run -- --demo

# Live ZeroMQ + Protobuf
cargo run -- --endpoint tcp://localhost:5555 --schema schema.proto

# Everything at once
cargo run -- \
  --endpoint tcp://localhost:5555 --schema schema.proto \
  --mqtt-endpoint localhost:1883 \
  --ws-listen 127.0.0.1:8086
```
