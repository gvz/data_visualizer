# Writing scripts

datavis can run your own Python to read channels, compute, and publish new
channels — at native speed (each script is compiled with numba when loaded).

## Requirements

- **numba** (and numpy) must be available. Linux `.deb` installs pull them in
  automatically; the Windows build bundles them. Without numba the scripting
  panel shows a warning and the feature is simply off.

## Where scripts live

Put `.py` files in a `scripts/` directory next to your `config.toml`. Enable
them from the **Scripts** panel in the sidebar (a checkbox per file). Your
choices are saved back to `config.toml`:

```toml
[scripts]
dir = "scripts"          # directory to scan, relative to config.toml
enabled = ["accel_mag"]  # active script file stems (no .py)
window_s = 10.0          # seconds of history each script sees per tick
```

## The contract

A script declares its inputs and outputs, and provides a numba-compiled
`compute` function:

```python
import numpy as np
import numba

INPUTS  = ["accel.x", "accel.y", "accel.z"]
OUTPUTS = [{"name": "accel.magnitude", "type": "float", "unit": "m/s2"}]

@numba.njit
def compute(ts, vals):
    t = ts[0]                            # timestamps of accel.x (int64 ns)
    x, y, z = vals[0], vals[1], vals[2]  # values, one array per input
    return (t, np.sqrt(x**2 + y**2 + z**2))
```

- **`INPUTS`** — channel names to read, in order.
- **`OUTPUTS`** — channels you publish. `type` is `"float"`, `"int"`, or
  `"bool"`; `unit` is optional.
- **`compute(ts, vals)`** must be decorated `@numba.njit`.
  - `ts[i]` / `vals[i]` are the timestamp and value arrays of input `i`
    (in `INPUTS` order), for the last `window_s` seconds.
  - They are **tuples of separate 1‑D arrays**, not a 2‑D array — each input
    has its own length and its own timestamps.
  - Return **one `(ts, vals)` pair per output**: a bare pair for a single
    output, or a tuple of pairs in `OUTPUTS` order.

## Element-wise vs. reduction

Return arrays the length of your chosen timestamps for a per-sample transform,
or length‑1 arrays for one value per tick:

```python
# Reduction: RMS of a window -> one sample at the latest input time
@numba.njit
def compute(ts, vals):
    t, v = ts[0], vals[0]
    return (t[-1:], np.array([np.sqrt(np.mean(v**2))]))
```

The engine appends only samples newer than the last it wrote, so overlapping
windows never duplicate output.

## Different-rate inputs

The engine never resamples. Each input keeps its own timestamps, so aligning
channels that arrive at different rates is up to you:

```python
INPUTS  = ["accel.x", "gps.speed"]
OUTPUTS = [{"name": "norm.speed", "type": "float", "unit": "m/s"}]

@numba.njit
def compute(ts, vals):
    tx, x = ts[0], vals[0]
    tg, g = ts[1], vals[1]
    g_on_x = np.interp(tx.astype(np.float64), tg.astype(np.float64), g)
    return (tx, g_on_x * x)
```

If your inputs are co-sampled (equal length, e.g. `x`/`y`/`z` from one
message) you can `np.stack((vals[0], vals[1], vals[2]))` for matrix math.

## Notes

- Outputs are ordinary channels — drop them onto any panel like live data.
- An input with no data yet leaves the script "waiting"; it starts as soon as
  data arrives.
- A compile error or an exception in `compute` marks only that script failed
  (shown in the Scripts panel); the app and other scripts keep running.
- Keep output arrays as `int64`/`float64`. Explicitly narrowing to `int32` or
  `float32` will be rejected.
