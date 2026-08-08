# Getting Started

## Build and run

datavis is a Rust application. From the repository root:

```bash
cargo run     # start the app — the demo source is enough to see it working
cargo test    # run the tests
```

The `scripting` feature is on by default. It pulls in PyO3/numpy, which need a
Python interpreter (`python3` + `python3-config`) on `PATH` at build time. Build
inside the `nix develop` shell (it provides `python3` + numba) or install
`python3-dev`. To build the base app without a Python toolchain:

```bash
cargo run --no-default-features
```

## First run: the demo source

You do not need any live inputs to try datavis. The built-in demo source
synthesises channels:

```bash
cargo run -- --demo
```

Add `--demo-freq <HZ>` to change the sine frequency (default `1.0`). See the
[Demo Source](sources/demo.md) page for details.

## The config file

Channels and the panel layout both live in `config.toml`, read from the working
directory at startup. If no `config.toml` is present, datavis offers to save the
built-in default, load a different file, or run with defaults just this once.

A fresh config has no channels or panels. You add visualisations from the panel
type picker in the UI, and the layout is written back into `config.toml`
automatically as you arrange it. See [Configuration](configuration/overview.md).

## A minimal live setup

To watch a real ZeroMQ Protobuf stream:

```bash
cargo run -- --endpoint tcp://localhost:5555 --schema schema.proto
```

`--schema` points at the `.proto` file describing the wire messages; datavis
compiles it at startup. Then declare which proto fields become channels in
`config.toml` (see [Channels](configuration/channels.md)) and drag them onto
panels.

## Where to go next

- Wire up a different transport → [Data Sources](sources/overview.md)
- Lay out panels → [Panels & Layout](configuration/layout.md)
- Record and replay a session → [Recording to MCAP](recording/recording.md)
