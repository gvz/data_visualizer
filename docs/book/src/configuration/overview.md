# Configuration Overview

datavis reads everything it needs from a single `config.toml` in the working
directory. That one file holds several independent sections:

| Section | Purpose | Chapter |
|---|---|---|
| `[channels."name"]` | Map source data to named, typed channels | [Channels](channels.md) |
| `[defaults]` | File-wide `max_rate` / `history_s` fallbacks | [Defaults](defaults.md) |
| `[screens.*]` / `[[screens.*.panels]]` | The panel layout (GUI-managed) | [Panels & Layout](layout.md) |
| `[[sources.bridge]]` | External bridge adapter processes | [Bridge Adapters](../sources/bridge.md) |
| `[scripts]` / `[[scripts.instances]]` | Python derived-channel scripts | [Scripting](../scripting/overview.md) |
| `[recording]` | Size-based recording auto-split | [Auto-Split](../recording/auto-split.md) |
| `default_window_s` | Default time window for time-series panels | below |

## How the file is parsed

Each concern parses the *whole* `config.toml` independently and ignores sections
it does not own. No parser uses `deny_unknown_fields` across the whole document,
so unrelated sections never cause a parse error. This is why you can drop a
`[recording]` or `[scripts]` block into the same file the layout parser reads
without either stepping on the other.

The practical consequence: **sections are optional and order-independent.** Omit
one and its feature falls back to a sensible default (or turns off).

## What is hand-edited vs. GUI-managed

- **Hand-edited:** `[channels.*]`, `[defaults]`, `[[sources.bridge]]`,
  `[scripts]`, `[recording]`. These are stable declarations you write once.
- **GUI-managed:** the `[screens.*]` layout. datavis writes the tile arrangement
  and panel list back to `config.toml` as you drag things around and on exit.
  You *can* hand-edit it, but expect the app to overwrite your formatting.

## Top-level keys

```toml
default_window_s = 10.0   # default visible time span for time-series panels
```

`default_window_s` seeds the time window new time-series panels (waveform, state
graph) open with. Individual panels can override it once you zoom.

A complete field-by-field listing of every section lives in the
[config.toml Reference](../reference/config.md).
