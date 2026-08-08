# Python Scripting

datavis can derive new channels from existing ones by running Python scripts.
Scripts are numba-compiled for speed and run live *and* during replay, feeding
their outputs back into the store as ordinary channels you can drop on panels.

If numba (or the Python toolchain) is unavailable, scripting degrades
gracefully — the engine disables itself with a warning and the rest of the app
runs normally. Scripting is also a build-time feature (`scripting`, on by
default); build with `--no-default-features` to drop the Python dependency
entirely.

## The scripts directory

Scripts live in a directory next to `config.toml` (default `scripts/`). The
engine scans it for `*.py` files at startup — these are the *available* scripts.
Availability is discovery; which scripts are *enabled* is separate and persisted
in config (see [Script Bindings](bindings.md)).

## The script contract

A script is a `.py` file that self-declares its bindings as module globals and
implements a numba-compiled `compute` function. Because `compute` is compiled
with numba it must be **pure**: tuples of numpy arrays in, `(ts, vals)` array
pairs out. No dicts of objects, no `.push()`, no channel handles cross the
`@njit` boundary — the engine handles all of that.

### Element-wise transform

Magnitude of a co-sampled 3-axis vector:

```python
import numpy as np
import numba

INPUTS  = ["accel.x", "accel.y", "accel.z"]
OUTPUTS = [{"name": "accel.magnitude", "type": "float", "unit": "m/s2"}]

@numba.njit
def compute(ts, vals):
    t = ts[0]                            # accel.x timestamps
    x, y, z = vals[0], vals[1], vals[2]
    mag = np.sqrt(x**2 + y**2 + z**2)
    return (t, mag)                      # one (ts, vals) pair per output
```

### Multi-rate combine

The engine never resamples for you — do it yourself:

```python
import numpy as np
import numba

INPUTS  = ["accel.x", "gps.speed"]
OUTPUTS = [{"name": "norm.speed", "type": "float", "unit": "m/s"}]

@numba.njit
def compute(ts, vals):
    tx, x = ts[0], vals[0]
    tg, g = ts[1], vals[1]
    g_on_x = np.interp(tx.astype(np.float64), tg.astype(np.float64), g)
    return (tx, g_on_x * x)
```

### Window reduction

Reduce a window to a single sample:

```python
import numpy as np
import numba

INPUTS  = ["motor.current"]
OUTPUTS = [{"name": "motor.current.rms", "type": "float", "unit": "A"}]

@numba.njit
def compute(ts, vals):
    t, v = ts[0], vals[0]
    rms = np.sqrt(np.mean(v**2))
    return (t[-1:], np.array([rms]))     # length-1 ts + vals -> one sample
```

## Declaring the engine in config

```toml
[scripts]
dir = "scripts"          # scripts directory, relative to config.toml
window_s = 10.0          # sliding window of input handed to compute
```

- `dir` — scripts directory. Defaults to `"scripts"` when the section is absent.
- `window_s` — seconds of input history passed to each `compute` call.

The `INPUTS` / `OUTPUTS` in a script are its *defaults*. You override them per
instance — reusing one script on different channels — in
[`[[scripts.instances]]`](bindings.md).
