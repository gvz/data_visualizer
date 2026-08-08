# Demo Source

The demo source synthesises channels so you can explore datavis without wiring
up any real transport. It is the fastest way to see the app working.

## Enabling it

```bash
cargo run -- --demo
```

- `--demo` — run with the built-in demo source and no live inputs.
- `--demo-freq <HZ>` — the sine frequency for the demo source. Default `1.0`.

## What it produces

The demo feeds a set of channels (sine waves at the configured frequency, plus a
few state/discrete channels) into the store, exactly as a real source would.
Because everything above the store reads through the same `ChannelStore` seam,
panels, cursors, recording, and scripting all behave identically on demo data.

That makes the demo source useful beyond a first look:

- Try out panel layouts and the drag-and-drop workflow.
- Exercise [scripting](../scripting/overview.md) against known inputs.
- Record a short demo session and practise [playback](../recording/playback.md).

## Note

When `--demo` is set, live transports are not started — the demo source stands
in for them. Drop it (omit `--demo`) to go back to real sources.
