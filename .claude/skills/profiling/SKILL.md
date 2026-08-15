---
name: profiling
description: Use when profiling datavis performance — measuring where the app spends time, burns CPU, or allocates memory under live data load. Runs a timed hotpath capture: launches the app with instrumentation, feeds synthetic WebSocket traffic via the ws_influx_load example, self-exits after a set duration, and produces a JSON report to analyse.
---

# Profiling datavis with hotpath

Timed, repeatable performance capture. The app is instrumented with
[hotpath](https://hotpath.rs) (feature-gated, zero cost when off). This skill
runs it under synthetic load for a fixed number of seconds and emits a JSON
report you then analyse.

## How it works

hotpath prints its report **only on clean process exit** (the `#[hotpath::main]`
guard flushes when `main` returns). There is no window tool (`xdotool`/`wmctrl`)
here to close the GUI, so the app self-exits instead: when `DATAVIS_EXIT_AFTER_S`
is set, `update()` sends `ViewportCommand::Close` once that many seconds have
elapsed → `run_native` returns → `main` returns → report is written. This env
var is unconditional app code (not behind the hotpath feature) and is a no-op
when unset.

The load comes from the `ws_influx_load` example, which connects to the app's
WebSocket source (config.toml: `listen = "127.0.0.1:9002"`) and pushes Influx
line-protocol frames: `--channels` numeric sine fields + `--state-channels`
cycling string fields.

## Run a capture

```bash
.claude/skills/profiling/profile.sh
```

Defaults: `--release` (optimized — representative numbers, not debug), 60 s,
`hotpath,hotpath-alloc`, `--no-default-features` (drops the PyO3/Python
scripting threads so the report is GUI + ingest only), report to
`hotpath-<timestamp>.json` in the repo root.

The app and the `ws_influx_load` example are both built `--release`. The first
run pays the optimized-build cost once; later runs reuse it.

Override any knob via env:

```bash
DURATION=120 FREQ=50000 FEATURES=hotpath,hotpath-cpu \
  .claude/skills/profiling/profile.sh
```

| Env | Default | Meaning |
|-----|---------|---------|
| `DURATION` | `60` | seconds to measure (drives both app self-exit and feeder) |
| `FEATURES` | `hotpath,hotpath-alloc` | `hotpath` (timing) / `+hotpath-alloc` / `+hotpath-cpu` |
| `NO_DEFAULT` | `1` | `1` = `--no-default-features` (no Python threads); `0` keeps scripting |
| `OUT` | `hotpath-<ts>.json` | report path |
| `WS_PORT` | `9002` | must match a `[[sources.websocket]]` listen port in config.toml |
| `CHANNELS` `FREQ` `STATE_CHANNELS` `STATES` `STATE_DWELL` `MEASUREMENT` | 8 / 100000 / 4 / `aaaa,bbbbb,cccc` / 2 / `load` | ws_influx_load load shape |

The reference load (what the defaults reproduce):

```bash
cargo run --example ws_influx_load -- --url ws://127.0.0.1:9002 \
  --channels 8 --freq 100000 --measurement load \
  --state-channels 4 --states aaaa,bbbbb,cccc --state-dwell 2 --duration 600
```

(The example's flag is `--states`, plural.)

## Instrumented hot paths

`#[hotpath::measure]` (feature-gated) is on: `DataVisApp::update` (per frame),
the `render` of waveform / spectrum / state_graph panels, and
`LiveStore::write_numeric` / `write_text` (the ingest path). Add more with
`#[cfg_attr(feature = "hotpath", hotpath::measure)]` on a function or
`hotpath::measure_block!("label", { … })` around a block.

## Analyse the report (this is the payoff)

Never dump the raw JSON into context. Parse it in the sandbox and print only
findings:

```
ctx_execute_file(path: "<OUT>", language: "javascript", code: `
  const r = JSON.parse(FILE_CONTENT);
  // print top functions by total time and by alloc bytes, P95 outliers, etc.
`)
```

Report format: sections keyed like `functions-timing`, `functions-alloc`,
`functions-cpu`, `threads`. Each function row carries calls, avg, the requested
percentiles (P95 default), total, and % of total. When reporting:

- rank by **Total** time and by % — that is where optimisation pays off;
- flag high **P95 vs avg** (tail latency / occasional stalls);
- in alloc mode, compare **bytes vs count** (`HOTPATH_ALLOC_METRIC=count`) and
  watch per-frame allocations in `update`/`render` (steady-state churn);
- tie each hot row back to the source and say *why* it is hot and what to change.

## Compare two runs (prove a win)

```bash
OUT=hotpath-before.json .claude/skills/profiling/profile.sh
# ...make a change...
OUT=hotpath-after.json  .claude/skills/profiling/profile.sh
```

Then diff the two JSONs per function (total, P95, alloc) in the sandbox.

## Gotchas

- **No report written** → the app didn't exit cleanly. Check
  `/tmp/datavis-profile.log`. Under Xvfb this is usually a GL failure — the
  script sets `LIBGL_ALWAYS_SOFTWARE=1`, but a real `$DISPLAY` is more reliable.
- **`ws server never came up`** → config.toml in the repo root must declare the
  `WS_PORT` websocket source; the reference config listens on 9001 and 9002.
- **Noisy thread/alloc rows** from Python → keep `NO_DEFAULT=1`, or read past
  the `hp-*` and Python threads.
- Timing is wall-clock; a busy machine inflates it. Compare runs on the same
  idle host.
```
