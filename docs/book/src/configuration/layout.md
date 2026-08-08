# Panels & Layout

The panel layout lives in `config.toml` under `[screens.*]`. Unlike the channel
tables, this section is **GUI-managed**: datavis writes it back as you rearrange
tiles and on exit. You can hand-edit it, but the app owns its formatting.

## Screens

A screen is one named workspace of tiles. Multiple screens are switchable from
the toolbar. Each screen carries the tile tree plus the list of panels on it:

```toml
default_window_s = 10.0

[screens.main]
tiles_json = '{"id":...,"root":1,"tiles":{ ... }}'

[[screens.main.panels]]
type = "waveform"
channels = ["load/ch0", "load/ch7", "scripts/rms"]
cursors = false
dots = false

[[screens.main.panels]]
type = "state_graph"
channel = "load/state0"
```

### `tiles_json`

The tile geometry — how the window is split into a tree of horizontal and
vertical containers — is serialised as a JSON string in `tiles_json`. This is
produced by the tiling engine (`egui_tiles`); treat it as opaque and let the GUI
manage it.

### `[[screens.<name>.panels]]`

One table per panel, in tile order. Common keys:

- `type` — the panel type (`waveform`, `gauge`, `state_graph`, `status`,
  `spectrum`, `numeric`, `xy`, `log`). See [Panel Types](../panels.md).
- `channels` — a list of channel names, for multi-channel panels (waveform, FFT).
- `channel` — a single channel name, for single-channel panels (gauge, status,
  state graph).
- Panel-specific keys — e.g. waveform's `cursors` and `dots`, status's
  `states`, gauge's min/max. Each panel type documents its own keys.

Panels resolve channel *names* to ids through the channel registry at load. A
name that does not resolve, or a channel of the wrong type for the panel, is
shown as an inline error — never a crash.

## Building a layout in the GUI

1. Split the window into tiles by dragging tile edges / using the tile menu.
2. Add a panel to a tile from the **panel type picker**.
3. Drag channels from the sidebar onto a panel. Select several and drop them
   together onto a multi-channel panel.
4. The arrangement is saved back to `config.toml` automatically.

## Time window

`default_window_s` (top level) sets the visible span new time-series panels open
with. Zooming a panel overrides it locally; see [Zooming & Cursors](../zoom.md).
