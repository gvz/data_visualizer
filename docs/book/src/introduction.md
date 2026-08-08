# Introduction

**datavis** is a desktop tool for watching streaming telemetry in real time. It
pulls in live channels over several transports, decodes them — Protobuf schemas
can be picked up on the fly — and draws them in a workspace of panels you
arrange yourself. You can record a session and scrub back through it later
through the very same panels.

It is written in Rust on top of [egui](https://github.com/emilk/egui).

## What it does

- **Reads live data** from ZeroMQ (Protobuf), MQTT, WebSocket (InfluxDB line
  protocol), and organisation-specific transports through an external *bridge*
  adapter. Protobuf payloads are decoded at runtime with `prost-reflect`, so you
  do not have to compile schemas in ahead of time.
- **Arranges panels** by carving the window into tiles and dragging channels out
  of the sidebar onto whichever panel you want. Select a few at once and drop
  them together.
- **Ships a handful of panel types**: waveform, numeric readout, gauge, FFT
  spectrum, XY scatter, state graph, status, and a text log.
- **Records to [MCAP](https://mcap.dev)** — including any Protobuf schemas it
  discovered — and plays the file back through the same panels. Replay and live
  look identical because panels never know which one they are getting.
- **Runs Python scripts** (numba-compiled) to derive new channels from existing
  ones, live and during replay.
- **Reads its channels and layout from `config.toml`.** A demo source is built
  in, so you can poke at it without wiring up anything real.

## The core idea

One trait — `ChannelStore` — is the seam that hides *live-vs-replay* from
everything above it. Panels read samples through that trait and never learn
which mode they are in. Recording, playback, and the larger-than-RAM lazy
loader all sit behind the same interface. This design choice recurs throughout
the guide; when a feature "just works" in both live and replay, this is why.

## How to read this guide

- **[Getting Started](getting-started.md)** gets the app running in under a
  minute using the demo source.
- **[Configuration](configuration/overview.md)** covers `config.toml`: channels,
  defaults, and the panel layout.
- **[Data Sources](sources/overview.md)** covers each transport and how to wire
  it up.
- **[Visualising](panels.md)** covers panels, zoom, and cursors.
- **[Recording & Playback](recording/recording.md)** covers MCAP capture,
  size-based auto-split, and scrubbing recordings back — including files larger
  than RAM.
- **[Scripting](scripting/overview.md)** covers deriving channels in Python.
- **[Reference](reference/cli.md)** is the terse lookup: CLI flags, every
  `config.toml` section, and the internal architecture.

> **A note on provenance.** Large parts of this project — this guide included —
> were written with an AI assistant. Expect confident-looking code that has not
> met every edge case. Check anything here you would not take on faith.
