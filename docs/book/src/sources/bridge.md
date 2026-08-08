# External Bridge Adapters

A **bridge** is a separate process that speaks a proprietary transport on one
side and datavis's simple pipe protocol on the other. It lets an organisation
feed data from a closed-source or vendor-specific system without touching any
datavis source code.

## Why a subprocess

datavis is licensed under **GPL-3.0**. A bridge adapter is a *separate process*
communicating over a pipe — it does not link against any datavis library or
include its source. That subprocess relationship is not a derivative work under
the GPL, so **your organisation may keep the adapter source closed.**

## Configuring a bridge

Bridges are declared in `config.toml` as an array of tables, because a bridge
needs a command and arguments (not just an endpoint string) and several may run
at once:

```toml
[[sources.bridge]]
name    = "vendor-x"                 # shown in the status bar / logs
command = "/opt/vendor-x/adapter"    # the org's proprietary executable
args    = ["--device", "/dev/tty0"]  # optional; defaults to empty
```

datavis spawns one subprocess per entry and owns its lifecycle — starting it at
launch and tearing it down on exit.

## Declaring bridge channels

Bridge channels carry a **fixed, built-in payload schema**, so they drop
`proto_path` / `ts_path` and declare only the topic and sample type:

```toml
[channels."vx/accel_x"]
topic       = "accel"        # matches the Column.topic on the wire
sample_type = "f64"
```

Topic→channel resolution reuses the normal registry lookup. As always, topics
must be unique across coexisting sources.

## The wire contract

The adapter writes a one-time **preamble**, then a stream of **frames**, each
carrying a Protobuf `Batch` of columns (topic + typed samples at possibly
different rates). The full byte-level specification — preamble, framing, the
`Batch` schema, and channel mapping rules — is the **External Source Protocol
guide** shipped in the repo at `docs/external-source-protocol.md`.

## Reference adapter

A minimal working adapter lives at `examples/echo_bridge.rs`. It writes the
preamble once and emits a single `Batch` with three columns at different rates
and types. Inspect its raw bytes:

```sh
cargo run --example echo_bridge | xxd | head
```

A real adapter swaps the hard-coded samples for data from your transport but
keeps the same framing. The example *is* the complete wire contract in
executable form.
