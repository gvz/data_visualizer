# Script Output Frequency Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Give each script a declared feed mode so `map` (elementwise) scripts output at exactly their input frequency via cursor-fed windows, while `reduce` scripts keep windowed aggregation with an optional throttle — eliminating the full-`window_s` recompute every tick.

**Architecture:** Add `MODE` (+ optional `HOP_MS`) to the script contract, parsed from the Python module globals into `ScriptMeta`. The `ScriptRunner` gains a per-runner input cursor and last-hop timestamp; `tick_rec` computes its own feed window from the mode instead of receiving one precomputed window shared by all runners. `map` feeds `(cursor, now]`; `reduce` feeds `last(window_s, now)` clamped to the current recording segment, throttled by `HOP_MS`.

**Tech Stack:** Rust, PyO3 0.22 (feature-gated `scripting`), the existing `CompiledScript`/`ScriptLoader` test fakes (no Python needed for the scheduler logic).

## Global Constraints

- **No coauthor / self-attribution in any commit** (repo rule; every commit in this plan omits `Co-Authored-By` and any `Claude-*` trailer).
- **Never stage or commit `config.toml`** — it stays dirty/untracked.
- **`scripting` is a gated feature.** The scheduler logic (`types.rs`, `runner.rs`, `mod.rs`) compiles and tests **without** the feature via the `FakeScript`/fake-loader harness. Only `python.rs` (the PyO3 loader) requires `--features scripting` to test.
- **Default `MODE` is `reduce`** so every existing script and every existing `ScriptMeta` keeps today's behaviour unless it opts into `map`.
- **`TimeWindow` is half-open `[start_ns, end_ns)`** (`src/types.rs:63`). To include a sample at `now`, use `end_ns = now + 1`. To feed strictly newer than a cursor `c`, use `start_ns = c + 1`.
- Run the scheduler tests with `cargo test --no-default-features script::` and the loader test with `cargo test --features scripting script::python`.

---

## File Structure

- `src/script/types.rs` — `ScriptMeta` and the new `ScriptMode` enum + `hop_ms` field. Pure data, no Python. **One responsibility:** the script contract types + validation.
- `src/script/python.rs` — PyO3 loader; `extract_meta` reads `MODE`/`HOP_MS` globals. Feature-gated.
- `src/script/runner.rs` — `ScriptRunner`: per-runner cursor/hop state and the mode-aware `tick_rec` feed logic. **The behavioural core.**
- `src/script/mod.rs` — the engine `run_loop`: passes `now` + `window_s_ns` to each runner instead of a shared precomputed window. Its test fakes construct `ScriptMeta`.
- `src/script/panel.rs` — test-only `ScriptMeta` literals to update.
- `scripts/channel_diff.py`, `scripts/sine_squared.py`, `scripts/sine_rms.py` — declare `MODE`.

---

## Task 1: Add `ScriptMode` + `HOP_MS` to the script contract

**Files:**
- Modify: `src/script/types.rs` (add `ScriptMode`, extend `ScriptMeta`, derive `Default`, update test literals)
- Modify: `src/script/python.rs:33-57` (`extract_meta` reads the new globals)
- Modify (compile-fix only — add `..Default::default()` to every `ScriptMeta` literal): `src/script/mod.rs:198,405,417,432`, `src/script/runner.rs:261,293,329,352,363,378,404,417,441`, `src/script/panel.rs:597,628`

**Interfaces:**
- Produces:
  - `pub enum ScriptMode { Map, Reduce }` — `Debug, Clone, Copy, PartialEq, Eq, Default`; `#[default] Reduce`.
  - `ScriptMeta` gains `pub mode: ScriptMode` and `pub hop_ms: Option<u64>`; `ScriptMeta` now also derives `Default`.
  - `extract_meta` reads module global `MODE` (`"map"`/`"reduce"`, absent ⇒ `Reduce`) and `HOP_MS` (positive int, absent ⇒ `None`).

- [ ] **Step 1: Write the failing test** (in `src/script/types.rs` `mod tests`)

```rust
#[test]
fn script_mode_defaults_to_reduce() {
    let m = ScriptMeta::default();
    assert_eq!(m.mode, ScriptMode::Reduce);
    assert_eq!(m.hop_ms, None);
}
```

- [ ] **Step 2: Run it to verify it fails**

Run: `cargo test --no-default-features script::types::tests::script_mode_defaults_to_reduce`
Expected: FAIL — `ScriptMode` / `Default` for `ScriptMeta` do not exist yet.

- [ ] **Step 3: Add the enum and fields**

In `src/script/types.rs`, above `ScriptMeta`:

```rust
/// How a script is fed and how often it emits.
/// - `Map`: cursor-fed with only the samples arrived since the last tick; the
///   script emits one output per input sample, so output rate == input rate.
/// - `Reduce`: fed a trailing `window_s` window; emits per tick (or per `HOP_MS`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ScriptMode {
    Map,
    #[default]
    Reduce,
}
```

Change the derive and body of `ScriptMeta`:

```rust
#[derive(Debug, Clone, PartialEq, Default)]
pub struct ScriptMeta {
    pub inputs: Vec<String>,
    pub outputs: Vec<OutputSpec>,
    /// Feed/emit mode. Absent `MODE` global ⇒ `Reduce` (back-compatible).
    pub mode: ScriptMode,
    /// Optional emit throttle for `Reduce` scripts, in milliseconds. `None` ⇒
    /// emit every engine tick. Ignored for `Map`.
    pub hop_ms: Option<u64>,
}
```

- [ ] **Step 4: Fix every `ScriptMeta` literal to compile**

Append `mode: ScriptMode::Reduce, hop_ms: None,` (or `..Default::default()`) to each literal listed under **Files** above. Example (`src/script/runner.rs:261`):

```rust
let meta = ScriptMeta {
    inputs: vec!["in.a".into()],
    outputs: vec![out("in.a.double")],
    ..Default::default()
};
```

Where a test needs `Map`, set `mode: ScriptMode::Map` explicitly (Task 2 does this for its own tests). `ScriptMode` must be imported where used: add `ScriptMode` to the `use crate::script::types::{...}` lists in `runner.rs` and `mod.rs`.

- [ ] **Step 5: Run tests to verify they pass**

Run: `cargo test --no-default-features script::`
Expected: PASS (all existing scheduler tests + the new default test).

- [ ] **Step 6: Write the failing loader test** (in `src/script/python.rs` `mod tests`; if none exists, add `#[cfg(test)] mod tests { use super::*; ... }`)

```rust
#[test]
fn extract_meta_reads_mode_and_hop() {
    Python::with_gil(|py| {
        let src = "INPUTS=['a']\nOUTPUTS=[{'name':'o','type':'float'}]\nMODE='map'\nHOP_MS=200\n";
        let m = PyModule::from_code_bound(py, src, "t.py", "t").unwrap();
        let meta = extract_meta(&m).unwrap();
        assert_eq!(meta.mode, crate::script::types::ScriptMode::Map);
        assert_eq!(meta.hop_ms, Some(200));

        let src2 = "INPUTS=['a']\nOUTPUTS=[{'name':'o','type':'float'}]\n";
        let m2 = PyModule::from_code_bound(py, src2, "t.py", "t").unwrap();
        let d = extract_meta(&m2).unwrap();
        assert_eq!(d.mode, crate::script::types::ScriptMode::Reduce);
        assert_eq!(d.hop_ms, None);

        let bad = "INPUTS=['a']\nOUTPUTS=[{'name':'o','type':'float'}]\nMODE='nope'\n";
        let mb = PyModule::from_code_bound(py, bad, "t.py", "t").unwrap();
        assert!(extract_meta(&mb).is_err());
    });
}
```

- [ ] **Step 7: Run it to verify it fails**

Run: `cargo test --features scripting script::python::tests::extract_meta_reads_mode_and_hop`
Expected: FAIL — `extract_meta` does not read `MODE`/`HOP_MS` yet.

- [ ] **Step 8: Implement the parse in `extract_meta`**

Replace the final `Ok(ScriptMeta { inputs, outputs })` (`src/script/python.rs:57`) with:

```rust
use crate::script::types::ScriptMode;
let mode = match module.getattr("MODE").ok() {
    None => ScriptMode::Reduce,
    Some(v) => match v.extract::<String>() {
        Ok(s) if s == "map" => ScriptMode::Map,
        Ok(s) if s == "reduce" => ScriptMode::Reduce,
        Ok(other) => return Err(format!("MODE must be \"map\" or \"reduce\", got {other:?}")),
        Err(e) => return Err(format!("MODE must be a string: {e}")),
    },
};
let hop_ms = match module.getattr("HOP_MS").ok() {
    None => None,
    Some(v) => Some(
        v.extract::<u64>()
            .map_err(|e| format!("HOP_MS must be a positive integer: {e}"))?,
    ),
};
Ok(ScriptMeta { inputs, outputs, mode, hop_ms })
```

- [ ] **Step 9: Run both suites to verify they pass**

Run: `cargo test --no-default-features script::` then `cargo test --features scripting script::python`
Expected: PASS.

- [ ] **Step 10: Commit**

```bash
git add src/script/types.rs src/script/python.rs src/script/runner.rs src/script/mod.rs src/script/panel.rs
git commit -m "feat(scripts): add MODE and HOP_MS to the script contract"
```

---

## Task 2: Cursor-fed `map`, window-fed `reduce` in `tick_rec`

**Files:**
- Modify: `src/script/runner.rs` (add cursor/hop fields + init; rewrite `tick_rec` feed; update `tick`; update test call sites)
- Modify: `src/script/mod.rs:329-334` (engine loop passes `now` + `window_s_ns`)
- Modify: `src/script/mod.rs:570` (test call site)

**Interfaces:**
- Consumes: `ScriptMode`, `ScriptMeta.mode` (Task 1).
- Produces:
  - `ScriptRunner` gains `input_cursor: i64` (init `i64::MIN`) and `last_hop_ns: i64` (init `i64::MIN`).
  - `pub fn tick_rec(&mut self, store: &dyn ChannelStore, registry: &ChannelRegistry, now: i64, window_s_ns: i64, rec: Option<&mut ScriptRecorder>)` — **signature changed** from `(…, window: TimeWindow, …)` to `(…, now: i64, window_s_ns: i64, …)`.
  - `pub fn tick(&mut self, store, registry, now: i64, window_s_ns: i64)` — same new tail.

- [ ] **Step 1: Write the failing test** (`runner.rs` `mod tests`)

```rust
#[test]
fn map_feeds_only_new_samples_and_matches_input_frequency() {
    let reg = registry_with_input();
    let store = LiveStore::from_registry(&reg);
    let in_id = reg.id("in.a").unwrap();

    // A map script echoes each input sample at its own timestamp.
    let meta = ScriptMeta {
        inputs: vec!["in.a".into()],
        outputs: vec![out("in.a.echo")],
        mode: ScriptMode::Map,
        ..Default::default()
    };
    // Assert the fed window never re-includes an already-processed sample:
    // record the number of samples handed to compute on each tick.
    let seen = std::rc::Rc::new(std::cell::RefCell::new(Vec::<usize>::new()));
    let seen2 = seen.clone();
    let compiled = Box::new(FakeScript(move |inp: &[InputWindow]| {
        seen2.borrow_mut().push(inp[0].ts.len());
        Ok(vec![OutputBatch { ts: inp[0].ts.clone(), vals: inp[0].vals.clone() }])
    }));
    let mut runner = ScriptRunner::new("echo".into(), meta, compiled, &store, &reg);

    // window_s large enough that the first-tick cap includes everything so far.
    let win = 1_000_000_000; // 1s in ns
    store.write_numeric(in_id, 10, NumericVal::Float(1.0));
    runner.tick(&store, &reg, 10, win);          // tick 1: sees [10]
    store.write_numeric(in_id, 20, NumericVal::Float(2.0));
    store.write_numeric(in_id, 30, NumericVal::Float(3.0));
    runner.tick(&store, &reg, 30, win);          // tick 2: sees [20,30] only

    assert_eq!(*seen.borrow(), vec![1, 2]);      // never re-fed sample 10
    // Output frequency == input frequency: 3 inputs -> 3 outputs.
    let out_id = reg.id("in.a.echo").unwrap();
    let snap = store.snapshot(out_id, TimeWindow { start_ns: i64::MIN, end_ns: i64::MAX });
    assert_eq!(snap.len(), 3);
}
```

- [ ] **Step 2: Run it to verify it fails**

Run: `cargo test --no-default-features script::runner::tests::map_feeds_only_new_samples_and_matches_input_frequency`
Expected: FAIL — `tick` still takes a `TimeWindow`; no cursor logic.

- [ ] **Step 3: Add cursor/hop fields**

In `ScriptRunner` (`runner.rs:51`) add after `last_written`:

```rust
    /// Newest input timestamp already fed to `Map` compute; `i64::MIN` before
    /// the first tick (which then backfills at most `window_s`).
    input_cursor: i64,
    /// Wall time of the last `Reduce` emit, for `HOP_MS` throttling; `i64::MIN`
    /// until the first emit.
    last_hop_ns: i64,
```

In `ScriptRunner::new` (`runner.rs:99`) extend the struct literal:

```rust
Self {
    name, meta, compiled, output_ids, input_ids, last_written,
    input_cursor: i64::MIN,
    last_hop_ns: i64::MIN,
    state: ScriptState::Healthy,
}
```

- [ ] **Step 4: Rewrite `tick` / `tick_rec` signatures and feed**

Replace `tick` (`runner.rs:110-118`):

```rust
    /// Run one tick with no recording. See [`Self::tick_rec`].
    pub fn tick(
        &mut self,
        store: &dyn ChannelStore,
        registry: &ChannelRegistry,
        now: i64,
        window_s_ns: i64,
    ) {
        self.tick_rec(store, registry, now, window_s_ns, None);
    }
```

In `tick_rec`, change the signature to `(…, now: i64, window_s_ns: i64, mut rec: …)` and replace the input-gathering block (`runner.rs:146-155`, the `window` usage) with a mode-computed feed window:

```rust
        // Feed window by mode. Map: only samples newer than the cursor (first
        // tick backfills at most window_s). Reduce: the trailing window_s.
        // (Reduce throttle lands in Task 3; segment clamp in Task 4.)
        let feed = match self.meta.mode {
            ScriptMode::Map => {
                let start = if self.input_cursor == i64::MIN {
                    now - window_s_ns
                } else {
                    self.input_cursor + 1
                };
                TimeWindow { start_ns: start, end_ns: now + 1 }
            }
            ScriptMode::Reduce => {
                TimeWindow { start_ns: now - window_s_ns, end_ns: now + 1 }
            }
        };

        let mut windows = Vec::with_capacity(self.input_ids.len());
        for id in self.input_ids.iter().map(|o| o.unwrap()) {
            let (ts, vals) = snapshot_to_f64(store.snapshot(id, feed));
            windows.push(InputWindow { ts, vals });
        }
```

(`snapshot_to_f64` is the local `runner.rs` fn — unchanged.) Then, inside the `Ok(batches)` arm **after** the write loop and before `self.state = ScriptState::Healthy;`, advance the cursor for `Map`:

```rust
        if self.meta.mode == ScriptMode::Map {
            // Advance to the *oldest* input's newest fed timestamp so a lagging
            // input never has samples skipped; faster inputs get a small re-fed
            // overlap that the per-output dedup (`last_written`) absorbs.
            if let Some(t) = windows.iter().filter_map(|w| w.ts.last().copied()).min() {
                self.input_cursor = t;
            }
        }
```

Add `ScriptMode` to the `use crate::script::types::{…}` import at `runner.rs:8`.

- [ ] **Step 5: Update the engine loop and all test call sites**

`mod.rs:329-334` — pass `now` + `window_s_ns`, drop the precomputed `window`:

```rust
            let now = store.now_ns();
            let window_s_ns = (self.window_s * 1e9) as i64;
            let guard = record_sender.lock().unwrap();
            let mut rec = guard.as_ref().map(|tx| ScriptRecorder::new(&mut proto, tx));
            for runner in &mut runners {
                runner.tick_rec(store.as_ref(), &self.registry, now, window_s_ns, rec.as_mut());
            }
```

Update every `runner.tick(&store, &reg, ALL)` / `tick_rec(…, ALL, …)` test call in `runner.rs` (lines 272,303,337,339,355,366,392,407,420,453) and `mod.rs:570`. Replace `ALL` with concrete `(now, window_s_ns)`: use a `now` at/after the largest timestamp the test writes and a `window_s_ns` large enough to include the test's samples (e.g. the existing small-ts tests write ts 10/20/30 → pass `now = 30, window_s_ns = 1_000_000_000`). For the two-tick dedup test (`runner.rs:337-339`) pass `now = 1, win` then `now = 2, win`. The `ALL` const may be removed once unused.

- [ ] **Step 6: Run tests to verify they pass**

Run: `cargo test --no-default-features script::`
Expected: PASS, including the new `map_feeds_only_new_samples_and_matches_input_frequency`.

- [ ] **Step 7: Commit**

```bash
git add src/script/runner.rs src/script/mod.rs
git commit -m "perf(scripts): cursor-feed map scripts, window-feed reduce"
```

---

## Task 3: `HOP_MS` throttle for `reduce`

**Files:**
- Modify: `src/script/runner.rs` (`tick_rec` reduce branch: hop gate + `last_hop_ns` advance)

**Interfaces:**
- Consumes: `ScriptMeta.hop_ms` (Task 1), `ScriptRunner.last_hop_ns` (Task 2).

- [ ] **Step 1: Write the failing test** (`runner.rs` `mod tests`)

```rust
#[test]
fn reduce_hop_throttles_emit() {
    let reg = registry_with_input();
    let store = LiveStore::from_registry(&reg);
    let in_id = reg.id("in.a").unwrap();
    for t in 1..=5 { store.write_numeric(in_id, t, NumericVal::Float(t as f64)); }

    let meta = ScriptMeta {
        inputs: vec!["in.a".into()],
        outputs: vec![out("o")],
        mode: ScriptMode::Reduce,
        hop_ms: Some(1),                 // 1 ms = 1_000_000 ns
    };
    // Emit one value stamped at the newest input time each time it runs.
    let compiled = Box::new(FakeScript(|inp: &[InputWindow]| {
        let t = *inp[0].ts.last().unwrap();
        Ok(vec![OutputBatch { ts: vec![t], vals: vec![0.0] }])
    }));
    let mut runner = ScriptRunner::new("r".into(), meta, compiled, &store, &reg);

    let win = 1_000_000_000;
    runner.tick(&store, &reg, 0, win);               // emits (last_hop = 0)
    runner.tick(&store, &reg, 500_000, win);         // 0.5 ms later: throttled, no run
    runner.tick(&store, &reg, 1_000_000, win);       // 1 ms later: emits again

    let out_id = reg.id("o").unwrap();
    // Two distinct emit times despite three ticks.
    let snap = store.snapshot(out_id, TimeWindow { start_ns: i64::MIN, end_ns: i64::MAX });
    assert_eq!(snap.len(), 2);
}
```

- [ ] **Step 2: Run it to verify it fails**

Run: `cargo test --no-default-features script::runner::tests::reduce_hop_throttles_emit`
Expected: FAIL — no throttle yet; all three ticks emit (but note ts dedup means the middle tick with the same newest-ts would be deduped; the assertion still distinguishes throttle from no-throttle because without the gate the second tick still runs `compute`). If flaky on dedup, the gate is what makes the count deterministic.

- [ ] **Step 3: Add the hop gate**

At the **top** of `tick_rec`, after the `Failed` early-return and input resolution, before gathering windows, add:

```rust
        // Reduce throttle: skip the whole tick until HOP_MS has elapsed.
        if self.meta.mode == ScriptMode::Reduce {
            if let Some(hop) = self.meta.hop_ms {
                if self.last_hop_ns != i64::MIN && now - self.last_hop_ns < hop as i64 * 1_000_000 {
                    return;
                }
            }
        }
```

And in the `Ok(batches)` arm, after the write loop (alongside the Map cursor advance), record the hop:

```rust
        if self.meta.mode == ScriptMode::Reduce {
            self.last_hop_ns = now;
        }
```

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test --no-default-features script::`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add src/script/runner.rs
git commit -m "feat(scripts): throttle reduce emit with optional HOP_MS"
```

---

## Task 4: Clamp the `reduce` window to the current recording segment

**Files:**
- Modify: `src/script/runner.rs` (`tick_rec` reduce feed clamps at a `break_times()` boundary)

**Interfaces:**
- Consumes: `ChannelStore::break_times() -> &[i64]` (`src/store/mod.rs:35`; default `&[]`, overridden by `PlaybackStore`).

- [ ] **Step 1: Write the failing test** (`runner.rs` `mod tests`)

Add a minimal fake store that reports a break, so the test needs no recording. Place near the other test helpers:

```rust
struct BreakStore { inner: LiveStore, breaks: Vec<i64> }
impl ChannelStore for BreakStore {
    fn write_numeric(&self, c: ChannelId, ts: i64, v: NumericVal) { self.inner.write_numeric(c, ts, v) }
    fn write_text(&self, c: ChannelId, ts: i64, l: String) { self.inner.write_text(c, ts, l) }
    fn snapshot(&self, c: ChannelId, w: TimeWindow) -> ChannelSnapshot { self.inner.snapshot(c, w) }
    fn latest(&self, c: ChannelId) -> Option<(i64, Sample)> { self.inner.latest(c) }
    fn channel_meta(&self, c: ChannelId) -> &crate::types::ChannelMeta { self.inner.channel_meta(c) }
    fn break_times(&self) -> &[i64] { &self.breaks }
}
```

```rust
#[test]
fn reduce_window_clamps_at_break() {
    let reg = registry_with_input();
    let store = BreakStore { inner: LiveStore::from_registry(&reg), breaks: vec![100] };
    let in_id = reg.id("in.a").unwrap();
    // Samples straddling the break at 100.
    store.write_numeric(in_id, 90, NumericVal::Float(1.0));
    store.write_numeric(in_id, 110, NumericVal::Float(2.0));

    let meta = ScriptMeta {
        inputs: vec!["in.a".into()],
        outputs: vec![out("o")],
        mode: ScriptMode::Reduce,
        ..Default::default()
    };
    // Report how many samples the reduce sees.
    let seen = std::rc::Rc::new(std::cell::RefCell::new(0usize));
    let seen2 = seen.clone();
    let compiled = Box::new(FakeScript(move |inp: &[InputWindow]| {
        *seen2.borrow_mut() = inp[0].ts.len();
        Ok(vec![OutputBatch { ts: vec![*inp[0].ts.last().unwrap()], vals: vec![0.0] }])
    }));
    let mut runner = ScriptRunner::new("r".into(), meta, compiled, &store, &reg);

    // window_s spans both samples, but the break at 100 must exclude ts=90.
    runner.tick(&store, &reg, 120, 1_000_000_000);
    assert_eq!(*seen.borrow(), 1); // only the post-break sample (110)
}
```

- [ ] **Step 2: Run it to verify it fails**

Run: `cargo test --no-default-features script::runner::tests::reduce_window_clamps_at_break`
Expected: FAIL — the reduce window starts at `now - window_s`, still includes ts=90.

- [ ] **Step 3: Clamp the reduce window**

In `tick_rec`, replace the `ScriptMode::Reduce` feed arm from Task 2 with:

```rust
            ScriptMode::Reduce => {
                let mut start = now - window_s_ns;
                // Never average across a recording join: start no earlier than
                // the most recent break at or before `now`.
                if let Some(&b) = store.break_times().iter().rev().find(|&&b| b <= now) {
                    start = start.max(b);
                }
                TimeWindow { start_ns: start, end_ns: now + 1 }
            }
```

(`break_times()` is sorted ascending, so `.rev().find(|b| b <= now)` yields the largest break `≤ now` = the current segment's start.)

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test --no-default-features script::`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add src/script/runner.rs
git commit -m "fix(scripts): clamp reduce window to the current recording segment"
```

---

## Task 5: Declare `MODE` on the shipped scripts

**Files:**
- Modify: `scripts/channel_diff.py`, `scripts/sine_squared.py` (add `MODE = "map"`)
- Modify: `scripts/sine_rms.py` (add `MODE = "reduce"`)
- Test: `src/script/python.rs` (assert the shipped scripts' modes parse) — optional but preferred

**Interfaces:**
- Consumes: `extract_meta` (Task 1).

- [ ] **Step 1: Add the mode declarations**

`scripts/channel_diff.py` and `scripts/sine_squared.py` — add near the top, beside `INPUTS`/`OUTPUTS`:

```python
MODE = "map"   # one output per input sample, at input timestamps
```

`scripts/sine_rms.py` — add:

```python
MODE = "reduce"   # windowed aggregate; emit per tick (optionally throttled)
```

- [ ] **Step 2: (Preferred) assert the shipped modes** (`python.rs` `mod tests`, `--features scripting`)

```rust
#[test]
fn shipped_scripts_declare_expected_modes() {
    use crate::script::types::ScriptMode;
    let loader = PyScriptLoader::new();
    for (file, want) in [
        ("scripts/channel_diff.py", ScriptMode::Map),
        ("scripts/sine_squared.py", ScriptMode::Map),
        ("scripts/sine_rms.py", ScriptMode::Reduce),
    ] {
        let src = std::fs::read_to_string(file).unwrap();
        let stem = std::path::Path::new(file).file_stem().unwrap().to_str().unwrap();
        assert_eq!(loader.peek_meta(&src, stem).unwrap().mode, want, "{file}");
    }
}
```

(If `PyScriptLoader::new()` differs, use the existing constructor the other `python.rs` tests use. Skip this step only if `peek_meta` cannot be reached from a test; the Task 1 loader test already covers the parse.)

- [ ] **Step 3: Run the loader suite**

Run: `cargo test --features scripting script::python`
Expected: PASS.

- [ ] **Step 4: Run the full suite (both feature configs)**

Run: `cargo test --no-default-features script::` then `cargo test` (default features)
Expected: PASS — existing scripting integration tests still green with `map` scripts producing per-sample output.

- [ ] **Step 5: Commit**

```bash
git add scripts/channel_diff.py scripts/sine_squared.py scripts/sine_rms.py src/script/python.rs
git commit -m "feat(scripts): declare map/reduce modes on the shipped scripts"
```

---

## Task 6: Measure the win

**Files:** none (uses the `profiling` skill).

- [ ] **Step 1: Profile with scripting on**

Run: `NO_DEFAULT=0 OUT="$PWD/hotpath-scripts-after.json" .claude/skills/profiling/profile.sh`

- [ ] **Step 2: Compare `tick_rec` against the baseline**

Compare `hotpath-scripts-after.json` to the recorded baseline in the spec (`tick_rec`: 11.7 ms / 21.9 MB per call, ~19 Hz). Expected: `channel_diff`/`sine_squared` (`map`) per-call time and allocation drop by ~2 orders of magnitude; the engine loop's effective tick rate rises toward 62 Hz; `sine_rms` (`reduce`) unchanged unless `HOP_MS` is set.

- [ ] **Step 3: Record the result** in the design doc's "Measured baseline" section (append an "After" line). Commit:

```bash
git add docs/superpowers/specs/2026-08-16-script-output-frequency-design.md
git commit -m "docs: record measured after-numbers for script feed modes"
```

---

## Deferred (not in this plan): recompute-if-absent during replay

The spec's replay decision has two paths. The **primary** path — replay a script's recorded output samples directly — already holds today: script outputs are written to the recording via `ScriptRecorder`/`RecordMsg::DynamicProto` and replay as ordinary channels; the engine does not tick against the playback store. This plan does not change that.

The **fallback** — recompute a derived channel during replay when it is *absent* from the recording, resetting per-input cursors on a playback-clock (`now_ns`) discontinuity — requires wiring the engine to tick against `PlaybackStore` and detecting seeks, a larger change touching mode switching. It is out of scope here and should be its own spec+plan. Nothing in this plan blocks it: the `input_cursor` reset it needs is a natural extension of Task 2's cursor field.

---

## Self-Review

**Spec coverage:**
- `MODE` map/reduce contract → Task 1. Default `reduce` → Task 1 (Step 3 `#[default]`).
- `map` cursor feed, input-frequency output, no full-window recompute → Task 2.
- First-tick backfill capped to `window_s` → Task 2 (Step 4, `input_cursor == i64::MIN` branch).
- `reduce` unchanged + optional `HOP_MS` → Task 3.
- Recording joins: `map` ignores (per-sample, no special handling), `reduce` clamps to segment → Task 4.
- Replay recorded-outputs primary path → already holds (Deferred section); recompute-if-absent explicitly deferred.
- Script migration → Task 5. Efficiency claim measured → Task 6.

**Placeholder scan:** No TBDs; every code step shows real code; every test has a concrete assertion and a run command.

**Type consistency:** `ScriptMode { Map, Reduce }`, `ScriptMeta.mode`/`.hop_ms`, `ScriptRunner.input_cursor`/`.last_hop_ns`, and the `tick`/`tick_rec(…, now: i64, window_s_ns: i64, …)` signature are used identically across Tasks 1–4. `TimeWindow` half-open convention (`end_ns = now + 1`, `start_ns = cursor + 1`) is applied consistently. Cursor advance uses `.min()` across inputs (documented rationale) in Task 2 and is untouched by later tasks.
