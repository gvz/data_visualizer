# Panels & Layout

The panel layout lives in `config.toml` under `[screens.*]`. Unlike the channel
tables, this section is **GUI-managed**: datavis writes it back as you rearrange
panels and on exit. You can hand-edit it, but the app owns its formatting.

## Screens

A screen is one named free-canvas workspace of panels. Multiple screens are
switchable from the toolbar. Each screen carries its list of panels, each with
its own canvas position:

```toml
default_window_s = 10.0

[screens.main]

[[screens.main.panels]]
type = "waveform"
frame = [0.0, 0.0, 0.5, 0.4]
channels = ["load/ch0", "load/ch7", "scripts/rms"]
cursors = false
dots = false

[[screens.main.panels]]
type = "state_graph"
channel = "load/state0"
```

### `frame`

Each panel carries its canvas geometry as `frame = [x, y, w, h]`, four numbers
in `0..1` normalized to the screen area (`x`/`y` = top-left corner, `w`/`h` =
size). Panels are placed freely, never overlap, and scale with the window.
Omit `frame` and datavis auto-arranges the screen's panels into a grid on load,
then writes real `frame`s back as you move them.

### `[[screens.<name>.panels]]`

One table per panel. Common keys:

- `type` — the panel type (`waveform`, `gauge`, `state_graph`, `status`,
  `spectrum`, `numeric`, `xy_scatter`, `log`). See [Panel Types](../panels.md).
- `channels` — a list of channel names, for multi-channel panels (waveform, FFT).
- `channel` — a single channel name, for single-channel panels (gauge, status,
  state graph).
- Panel-specific keys — e.g. waveform's `cursors` and `dots`, status's
  `states`, gauge's min/max. Each panel type documents its own keys.

Panels resolve channel *names* to ids through the channel registry at load. A
name that does not resolve, or a channel of the wrong type for the panel, is
shown as an inline error — never a crash.

## Building a layout in the GUI

1. Add a panel from the toolbar, or right-click empty canvas → **Add panel here**.
2. Choose a panel type from the picker.
3. Drag a panel by its title bar to move it — or hold **Ctrl** (**⌘** on macOS)
   and drag anywhere in the panel. Drag the bottom-right grip to resize. Panels
   snap to each other's edges and sizes and block on contact, so they never
   overlap.
4. Right-click a panel (title bar or body) for its menu: **Settings**, **Reset
   zoom**, and **Remove panel**. The title-bar gear and ✕ do the same.
5. Drag channels from the sidebar onto a panel. Select several and drop them
   together onto a multi-channel panel.
6. The arrangement is saved back to `config.toml` automatically.

## Time window

`default_window_s` (top level) sets the visible span new time-series panels open
with. Zooming a panel overrides it locally; see [Zooming & Cursors](../zoom.md).
