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
- **Compilation:** each script is `compile()`d to bytecode once at load. No
  per-tick parse. Heavy math is numpy, which runs in C and releases the GIL.
- **Channel binding:** each `.py` self-declares `INPUTS` and `OUTPUTS`.
- **Selection:** a GUI panel lists available scripts with checkboxes; the
  enabled set persists in `config.toml`.
- **Failure mode:** graceful degradation with a visible warning. A missing
  Python runtime, a compile error, or a `compute()` exception disables only the
  affected script (or the whole engine, if Python is absent) and surfaces the
  message in the GUI. Core telemetry visualization is never blocked.
- **Packaging:** Linux resolves Python + numba + numpy through `.deb`
  dependencies; Windows bundles a self-contained interpreter in the zip.

## Architecture

The scripting engine is a `ScriptEngine` that implements the existing
`DataSource` trait (`src/ingest/source.rs`), exactly like the MQTT, ZMQ, and
WebSocket sources. `DataSource::spawn` takes `Arc<dyn ChannelStore>` and returns
a `SourceHandle`, so the engine plugs into the app's source-wiring path with no
new integration surface.

`spawn` starts **one** background thread that owns the Python interpreter (via
PyO3). The thread runs a fixed-cadence loop:

```
loop every ~16 ms (~60 Hz):
    for each enabled, healthy script:
        build input snapshot  (numpy arrays per input channel)
        call compiled compute(inputs, out)
        drain out -> store.write_numeric / store.write_text
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
implements a `compute` function.

```python
INPUTS  = ["accel.x", "accel.y", "accel.z"]
OUTPUTS = [{"name": "accel.magnitude", "type": "float", "unit": "m/s2"}]

def compute(inputs, out):
    x = inputs["accel.x"].vals      # numpy float64 array (window snapshot)
    y = inputs["accel.y"].vals
    z = inputs["accel.z"].vals
    ts = inputs["accel.x"].ts       # numpy int64 array, ns since Unix epoch
    mag = (x**2 + y**2 + z**2) ** 0.5
    out["accel.magnitude"].push(ts[-1], mag[-1])
```

### Input objects

`inputs[name]` exposes two numpy arrays for the current time window (the same
window semantics panels use via `ChannelStore::snapshot`):

- `.ts` — `int64` array, nanoseconds since the Unix epoch.
- `.vals` — `float64` for float/int/bool channels; text channels are out of
  scope for v1 inputs (numeric only).

Arrays are read-only views/copies of a `ChannelSnapshot`. An input whose channel
has no data yet yields empty arrays.

### Output objects

`OUTPUTS` declares each published channel: `name` (string), `type` (`"float"`,
`"int"`, or `"bool"`), and optional `unit`. `out[name].push(ts_ns, value)`
appends one sample. The engine converts to the declared type and calls
`store.write_numeric`. A script may push zero or more samples per tick.

### Type mapping

| Declared `type` | Rust `SampleType` | `NumericVal` written        |
| --------------- | ----------------- | --------------------------- |
| `float`         | `Float`           | `NumericVal::Float(f64)`    |
| `int`           | `Int`             | `NumericVal::Int(i64)`      |
| `bool`          | `Bool`            | `NumericVal::Bool(bool)`    |

Text outputs are out of scope for v1.

## Load Flow and Channel Registration

When a script is enabled (at startup or via the GUI toggle):

1. Read the `.py` file, `compile()` the source, and `exec` the module in a fresh
   namespace.
2. Read `INPUTS` and `OUTPUTS` globals; validate shapes. Grab the `compute`
   callable and cache it.
3. Register each output via the existing runtime-growth path:
   `ChannelRegistry::add_dynamic(name, name, sample_type)` then
   `store.add_channel(registry.meta(id).clone())` — the same lockstep append
   `dynamic_channel.rs` uses for dropped MQTT topics. Registry id and store slot
   advance together; both calls are `&self`, safe while ingest writes.
4. Resolve each input name to a `ChannelId` (`registry.id(name)`). Inputs that do
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
| Python runtime / numpy import unavailable | Engine disables itself; GUI shows "Python runtime unavailable." App runs. |
| Script compile error / bad INPUTS/OUTPUTS | That script marked failed with the exception text; others keep running.  |
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

- A fixture `.py` compiles, runs `compute` on a known input snapshot, and its
  output channel receives the expected values.
- A `compute()` that raises is caught; the script is paused, the engine survives,
  a sibling script keeps producing output.
- A script referencing a missing module (simulating absent numba) is flagged
  failed without aborting the engine.

**Graceful-disable test:**

- With the interpreter unavailable, the engine reports disabled and the rest of
  the app initializes normally.

## Out of Scope (v1)

- Text-typed inputs and outputs (numeric only).
- `@numba.njit`-decorated scalar-loop JIT is *available* to scripts where numba
  is installed, but the engine does not require or warm it; numpy vectorized math
  is the baseline.
- Sub-interpreters / parallel per-script threads (numpy does not support
  sub-interpreters yet). One engine thread runs all scripts sequentially.
- Live editing inside the app; scripts are edited in an external editor and
  reloaded via the toggle.
