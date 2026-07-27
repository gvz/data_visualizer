# datavis

A desktop tool for watching streaming telemetry in real time. It pulls in live
channels over a few different transports, decodes them (Protobuf schemas can be
picked up on the fly), and draws them in a workspace of panels you arrange
yourself. You can record a session and scrub back through it later.

Written in Rust on top of [egui](https://github.com/emilk/egui).

## What it does

- Reads live data from MQTT, WebSocket, ZeroMQ, and InfluxDB line protocol.
  Protobuf payloads are decoded at runtime with `prost-reflect`, so you don't
  have to compile schemas in ahead of time.
- Lets you carve the window into tiles, then drag channels out of the sidebar
  onto whichever panel you want. Select a few at once and drop them together.
- Ships a handful of panel types: waveform, numeric readout, gauge, FFT
  spectrum, XY scatter, state graph, status, and a text log.
- Records to [MCAP](https://mcap.dev) — including any Protobuf schemas it
  discovered — and plays the file back through the same panels. Replay and live
  look identical because panels never know which one they're getting.
- Reads its channels and layout from `config.toml`. There's a demo source built
  in, so you can poke at it without wiring up anything real.

## How it's laid out

The `ChannelStore` trait is the seam that hides live-vs-replay from everything
above it.

- `ingest/` — the transports, payload decoding, and routing into the store
- `store/` — live ring buffers and text history sitting behind `ChannelStore`
- `viz/` — the `VizPanel` trait and every panel, looked up through a registry
- `record/` — MCAP writer, playback, and MQTT schema capture
- `config/` — channel and layout files
- `workspace.rs` / `app.rs` — the tiled UI and the eframe app that hosts it

## Running it

```bash
cargo run     # start the app — the demo source is enough to see it working
cargo test    # run the tests
```

Channels and layout live in `config.toml`.

## Checking for dead dependencies

The unused-dep tools aren't library deps, so they're just noted in `Cargo.toml`
under `[package.metadata.dev-tools]`. Install and run them separately:

```bash
cargo machete          # quick heuristic scan
cargo +nightly udeps   # slower, compiles the crate to be sure
```
