# Script Bindings

A script declares *default* inputs and outputs. A **binding** is one concrete
use of that script — pointing its inputs at real channels and naming its
outputs. Bindings let you reuse one script on several channel sets, each
producing its own derived channels.

## Instances in config

Each active binding is a `[[scripts.instances]]` entry:

```toml
[scripts]
dir = "scripts"
window_s = 10.0

[[scripts.instances]]
id = "ch0_squared"        # unique instance id
script = "sine_squared"   # script stem (file name without .py)
enabled = true            # inputs/outputs omitted → use the script's defaults

[[scripts.instances]]
id = "ch1_rms"
script = "sine_rms"
inputs  = ["load/ch0"]
outputs = [{ name = "scripts/rms", type = "float", unit = "" }]
enabled = true

[[scripts.instances]]
id = "diff"
script = "channel_diff"
inputs  = ["load/ch0", "load/ch7"]
outputs = [{ name = "scripts/diff", type = "float", unit = "" }]
enabled = true
```

- `id` — unique per instance; identifies it in the UI and config.
- `script` — the script file stem in the scripts `dir`.
- `inputs` — channel names to feed `compute`. Omit to use the script's own
  `INPUTS`. The count must match the script's arity.
- `outputs` — output channel definitions (`name`, `type`, `unit`). Omit to use
  the script's `OUTPUTS`.
- `enabled` — whether the binding runs.

## Output-name templates

Output names may use placeholders that expand from the bound inputs, so one
script reused on many inputs yields distinct output channels:

- `{in0}` — the full name of input 0.
- `{in0.stem}` — the last path segment of input 0.
- `{in1}`, `{in1.stem}`, … for further inputs.

A literal name passes through unchanged. An unknown placeholder is an error.

## Resolution rules

- **Arity must match.** If the number of bound inputs (or outputs) does not
  match the script, the instance is marked `Failed` with a reason.
- **Output names must be unique.** Two instances declaring the same output
  channel collide; the second is `Failed`. Rebuilding or removing an instance
  releases the output names it owned, so re-applying does not self-collide.
- **Unresolved inputs block Apply.** In the panel editor, an input that does not
  resolve to a channel prevents applying that binding.

## The script panel

The script panel is a full editor for bindings: pick a script, resolve its
inputs against available channels (with a fuzzy filter that ranks matches), name
the outputs, and enable it. Applying or removing a binding updates the store's
derived channels live; changes persist to `[[scripts.instances]]`.

## Lifecycle & status

Each instance reports status — running, disabled, or `Failed` with a reason
(arity mismatch, output collision, compile error). A failing script never takes
down the app or other scripts; it is isolated and surfaced in the UI. If the
whole engine cannot start (numba missing), it reports a single disabled reason
and the app runs without derived channels.
