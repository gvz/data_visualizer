# Python Scripting Engine Design

**Date:** 2026-08-01
**Status:** Approved, ready for implementation planning

## Goal

Let users write Python scripts that read from channels, do math/analysis, and
publish new channels. Scripts are compiled once when loaded so they run at full
speed from the first sample. Output channels are ordinary channels — every
existing panel (waveform, spectrum, gauge, status, …) works on them unchanged.

## Summary of Decisions

- **Execution cadence:** background thread on a ~60 Hz timer (matches frame
  rate, decoupled from actual egui paints). Python never runs on the UI thread.
- **Compilation:** each script's `compute` is `@numba.njit` and is **eagerly
  compiled to native code at load** (the engine forces numba compilation with a
  known signature), so the first tick is already warm — no lazy first-call JIT
  stall, no per-tick parse. Because numba compiles the function, `compute` must
  be pure array-in/array-out (numba nopython mode handles numpy arrays, scalars,
  and tuples — not dicts of Python objects). The engine does all channel
  plumbing around it.
- **Channel binding:** each `.py` self-declares `INPUTS` and `OUTPUTS`.
- **Selection:** a GUI panel lists available scripts with checkboxes; the
  enabled set persists in `config.toml`.
- **Runtime requirement:** scripting is available **only** when Python *and*
  numba import successfully. numba (which requires numpy) is the baseline, not
  an optional accelerator — on a system without it, the feature is absent, not
  degraded to pure Python.
- **Failure mode:** graceful degradation with a visible warning. A missing
  Python runtime, a missing numba, a compile error, or a `compute()` exception
  disables the engine (numba/Python absent) or only the affected script
  (compile/runtime error) and surfaces the message in the GUI. Core telemetry
  visualization is never blocked.
- **Packaging:** Linux resolves Python + numba + numpy through `.deb`
  dependencies; Windows bundles a self-contained interpreter in the zip.

## Architecture

The scripting engine is a `ScriptEngine` that implements the existing
`DataSource` trait (`src/ingest/source.rs`), exactly like the MQTT, ZMQ, and
WebSocket sources. `DataSource::spawn` takes `Arc<dyn ChannelStore>` and returns
a `SourceHandle`, so the engine plugs into the app's source-wiring path with no
new integration surface.

Before doing any work, `spawn`'s thread runs a **capability probe**: it
initializes the interpreter and attempts `import numba`. If either the
interpreter or the import fails, the engine records the reason, marks itself
disabled, and the thread exits — no scripts are loaded and no ticks run. numba
is the gate; because numba depends on numpy, a successful `import numba` proves
the whole numeric stack is present. On systems without numba the feature is
simply not available.

When the probe succeeds, `spawn` starts **one** background thread that owns the
Python interpreter (via PyO3). The thread runs a fixed-cadence loop:

```
loop every ~16 ms (~60 Hz):
    for each enabled, healthy script:
        gather ts + one float64 array per input channel (window snapshot)
        outputs = compiled_compute(ts, in0, in1, ...)   # native, pre-compiled
        for each declared output, append new samples to the store
```

Outputs are written to the shared `ChannelStore`. The store's `write_seq()`
counter bumps on every write, which the app already polls each frame
(`src/app.rs:775`) to trigger repaints. So new script outputs appear in panels
through the existing change-detection path — no new UI plumbing.

**Why a background thread, not the egui frame callback:** running Python on the
UI thread would hold the GIL and block rendering whenever a script is slow. A
dedicated thread keeps the render loop responsive regardless of script cost.

### Threading and the GIL

The engine thread holds the interpreter. numpy array operations release the GIL
internally, so other Rust threads (ingest, UI) are unaffected during array
math. Only pure-Python scalar loops hold the GIL — and only on the engine
thread, which nothing else contends for.

## The Script Contract

A script is a `.py` file that self-declares its bindings as module globals and
implements a numba-compiled `compute` function. Because the engine compiles
`compute` with numba, it must be **pure**: numpy arrays and scalars in,
numpy arrays and scalars out. No dicts of objects, no `.push()`, no channel
handles cross the `@njit` boundary — the engine handles all of that.

**Element-wise transform** (magnitude of a 3-axis vector):

```python
import numpy as np
import numba

INPUTS  = ["accel.x", "accel.y", "accel.z"]
OUTPUTS = [{"name": "accel.magnitude", "type": "float", "unit": "m/s2"}]

@numba.njit
def compute(ts, x, y, z):
    return np.sqrt(x**2 + y**2 + z**2)   # array, len == len(ts)
```

**Window reduction** (RMS of a window → one value):

```python
import numpy as np
import numba

INPUTS  = ["motor.current"]
OUTPUTS = [{"name": "motor.current.rms", "type": "float", "unit": "A"}]

@numba.njit
def compute(ts, v):
    return np.sqrt(np.mean(v**2))        # scalar
```

### Signature

`compute`'s parameters are fixed by position:

- **`ts`** — first parameter, always. `int64[:]`, nanoseconds since the Unix
  epoch, for the current window.
- **one array per `INPUTS` entry, in order** — each `float64[:]`, the window
  snapshot of that channel's values (int/bool channels are widened to
  `float64`, matching what panels get).

All inputs share the leading `ts` time base: element-wise scripts require their
inputs to be sample-aligned (equal length). Mismatched lengths raise at runtime
and flag the script. Cross-channel resampling is out of scope for v1.

### Return values

`compute` returns one value per `OUTPUTS` entry — a single value, or a tuple in
`OUTPUTS` order. Each value is interpreted by shape:

- **array of length `len(ts)`** → element-wise. Each element is a sample at the
  matching `ts`. The engine appends only samples newer than the last it wrote
  for that channel (windows overlap tick to tick; this dedups by timestamp).
- **scalar (or length-1 array)** → one reduction sample per tick, written at
  `ts[-1]` (the latest input timestamp).

An input channel with no data yet yields empty arrays; the engine skips the call
(the script stays healthy, "waiting for data").

### Type mapping

`OUTPUTS` declares each published channel: `name`, `type` (`"float"`, `"int"`,
or `"bool"`), and optional `unit`. The engine casts each returned value to the
declared type:

| Declared `type` | Rust `SampleType` | `NumericVal` written        |
| --------------- | ----------------- | --------------------------- |
| `float`         | `Float`           | `NumericVal::Float(f64)`    |
| `int`           | `Int`             | `NumericVal::Int(i64)`      |
| `bool`          | `Bool`            | `NumericVal::Bool(bool)`    |

Text inputs and outputs are out of scope for v1 (numeric only).

## Load Flow and Channel Registration

When a script is enabled (at startup or via the GUI toggle):

1. Read the `.py` file and `exec` the module in a fresh namespace.
2. Read `INPUTS` and `OUTPUTS` globals; validate shapes. Grab `compute` and
   verify it is a numba dispatcher (a `@numba.njit` function). If it is a plain
   function, flag the script failed: "`compute` must be decorated `@numba.njit`."
3. **Eagerly compile.** Build the numba signature from arity —
   `(int64[:], float64[:], … × len(INPUTS))` — and call
   `compute.compile(signature)`. This compiles to native code now, at load, so
   the first tick runs warm. A numba compilation error flags the script failed
   with the numba message (visible immediately, per the graceful-warning rule).
4. Register each output via the existing runtime-growth path:
   `ChannelRegistry::add_dynamic(name, name, sample_type)` then
   `store.add_channel(registry.meta(id).clone())` — the same lockstep append
   `dynamic_channel.rs` uses for dropped MQTT topics. Registry id and store slot
   advance together; both calls are `&self`, safe while ingest writes.
5. Resolve each input name to a `ChannelId` (`registry.id(name)`). Inputs that do
   not resolve yet leave the script in a "waiting for `<name>`" state — it is
   loaded and healthy but skipped each tick until the input exists.

Output channel names must be unique against existing channels; a collision marks
the script failed with a clear message (rather than hijacking another channel's
slot).

## GUI and Config Persistence

### Scripts directory

Scripts live in a directory next to `config.toml` (default `scripts/`). The
engine scans it for `*.py` files at startup — these are the *available* scripts.
Availability is discovery; enablement is separate and persisted.

### Config section

`config.toml` gains a `[scripts]` section:

```toml
[scripts]
dir = "scripts"
enabled = ["accel_magnitude", "rms_filter"]
```

- `dir` — scripts directory, relative to `config.toml`. Defaults to `"scripts"`
  when the section is absent.
- `enabled` — list of script stems (filename without `.py`) that are active.

Parsing follows the existing pattern: `[scripts]` is another section in the
shared `config.toml`, ignored by the channel and layout parsers (neither uses
`deny_unknown_fields` across the whole document).

### Script panel

A GUI panel lists every available script with:

- a checkbox reflecting `enabled` membership,
- a status line: healthy / waiting-for-input / failed (with the error message).

Toggling a checkbox: (a) enables/disables the script live on the engine thread,
and (b) rewrites the `enabled` list in `config.toml`. Persistence reuses the
established `toml_edit::DocumentMut` merge-and-write approach from
`LayoutConfig::save` (`src/config/layout.rs:70`) — regenerate only the
`[scripts]` keys, preserve every other section and its comments verbatim.

## Error Handling (Graceful Degradation with Warning)

Nothing about scripting can prevent the app from running or stall the UI. Every
failure is caught and surfaced:

| Failure                                   | Behavior                                                                 |
| ----------------------------------------- | ------------------------------------------------------------------------ |
| Python runtime unavailable                | Engine disables itself; GUI shows "Python runtime unavailable." App runs. |
| numba (or numpy) import fails             | Engine disables itself; GUI shows "numba not installed — scripting unavailable." App runs. |
| numba compile error at load / bad INPUTS/OUTPUTS / `compute` not `@njit` | That script marked failed with the exception text; others keep running.  |
| Output name collides with a channel       | That script marked failed with a clear message; not loaded.              |
| Input channel absent                      | Script healthy but "waiting for `<name>`"; skipped until it appears.     |
| Exception inside `compute()`              | Caught; script paused with the error shown; tick loop continues.         |

Warnings are visible on load (not just on first tick) so a user enabling a
broken script sees the problem immediately in the script panel.

## Packaging

### Linux (`.deb` via cargo-deb)

PyO3 dynamically links `libpython3.x`, so `dpkg-shlibdeps` (`$auto`, already in
use) resolves the interpreter automatically. Python packages are added
explicitly:

```toml
[package.metadata.deb]
depends = "$auto, python3, python3-numba, python3-numpy"
```

`python3-numba` pulls `python3-numpy` and `python3-llvmlite` transitively. The
per-distro container build links each `.deb` against that distro's Python
version, so there is no version skew within a release. Debian maintains and
security-patches these packages — the app inherits a vetted, signed source that
cargo-vet cannot cover for Python code.

### Windows (portable zip)

No package manager, so the runtime is bundled:

- A relocatable CPython (python-build-standalone) with numba and numpy
  pre-installed into its `site-packages`.
- `python3x.dll` shipped beside `datavis.exe`.
- At startup the app resolves the bundled Python directory relative to the
  executable and sets `PYTHONHOME`/`PYTHONPATH` before initializing the
  interpreter.

PyO3 links against the standalone interpreter for the Windows build (via
`PYO3_PYTHON`); against the container's system libpython for Linux.

### The cross-platform seam

Linux and Windows may run different numba/numpy versions (Debian's vs. the
bundled ones). Scripts should stay on stable numpy/numba surface, and the script
fixture used in tests must pass on both. This is the one compatibility risk to
watch.

## Testing

**Rust unit tests:**

- Output registration appends store slots at the ids the registry assigns
  (mirrors `add_channel_appends_writable_slot`).
- A script with an unresolved input is flagged "waiting" and skipped, not
  failed.
- `[scripts]` round-trips: parse defaults when absent; `enabled` toggle rewrites
  `config.toml` preserving other sections and comments.
- Output name collision marks the script failed.

**Script-contract tests (with the interpreter):**

- An element-wise fixture (`@njit`) eagerly compiles at load, runs on a known
  input snapshot, and its output channel receives the expected per-sample values.
- A reduction fixture returning a scalar writes one sample at `ts[-1]`.
- The dedup rule: across two overlapping-window ticks, an element-wise output
  gains only the newly-arrived samples, no duplicates.
- A `compute` that is not `@numba.njit`, or that raises a numba compilation
  error at load, is flagged failed with the message; sibling scripts still load.
- A `compute()` that raises at runtime is caught; the script is paused, the
  engine survives, a sibling script keeps producing output.

**Graceful-disable tests:**

- With numba unavailable (import fails), the capability probe disables the whole
  engine, the GUI reports it, and the rest of the app initializes normally.
- With the interpreter unavailable, the engine reports disabled and the rest of
  the app initializes normally.

## Out of Scope (v1)

- Text-typed inputs and outputs (numeric only).
- Cross-channel resampling: element-wise scripts assume their inputs share a
  time base (equal-length windows). Mismatched inputs are a runtime error, not
  auto-aligned.
- Sub-interpreters / parallel per-script threads (numpy does not support
  sub-interpreters yet). One engine thread runs all scripts sequentially.
- Live editing inside the app; scripts are edited in an external editor and
  reloaded via the toggle.
