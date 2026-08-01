use crate::config::ChannelRegistry;
use crate::script::types::{CompiledScript, InputWindow, ScriptMeta};
use crate::store::ChannelStore;
use crate::types::{ChannelId, ChannelSnapshot, NumericVal, SampleType, TimeWindow};

/// Per-script health, surfaced in the GUI.
#[derive(Debug, Clone, PartialEq)]
pub enum ScriptState {
    Healthy,
    Waiting(String),
    Failed(String),
}

/// One running script: its compiled callable plus the bookkeeping the engine
/// needs to route channels and dedup output samples.
pub struct ScriptRunner {
    name: String,
    meta: ScriptMeta,
    compiled: Box<dyn CompiledScript>,
    /// Output channel ids, aligned to `meta.outputs`.
    output_ids: Vec<ChannelId>,
    /// Input channel ids, aligned to `meta.inputs`; `None` until resolved.
    input_ids: Vec<Option<ChannelId>>,
    /// Last timestamp written per output, aligned to `meta.outputs`.
    last_written: Vec<i64>,
    state: ScriptState,
}

/// Convert a numeric snapshot into parallel (ts, f64 vals). Text snapshots
/// (never a script input) yield empty arrays.
fn snapshot_to_f64(snap: ChannelSnapshot) -> (Vec<i64>, Vec<f64>) {
    match snap {
        ChannelSnapshot::Float { ts, vals } => (ts, vals),
        ChannelSnapshot::Int { ts, vals } => {
            let f = vals.into_iter().map(|v| v as f64).collect();
            (ts, f)
        }
        ChannelSnapshot::Bool { ts, vals } => {
            let f = vals.into_iter().map(|v| v as f64).collect();
            (ts, f)
        }
        ChannelSnapshot::Text { .. } => (Vec::new(), Vec::new()),
    }
}

/// Cast a computed f64 to the channel's declared numeric type.
fn cast_to(sample_type: SampleType, v: f64) -> NumericVal {
    match sample_type {
        SampleType::Float => NumericVal::Float(v),
        SampleType::Int => NumericVal::Int(v as i64),
        SampleType::Bool => NumericVal::Bool(v != 0.0),
        // Text outputs are rejected at load; treat defensively as float.
        SampleType::Text => NumericVal::Float(v),
    }
}

impl ScriptRunner {
    /// Register each declared output as a runtime channel (same lockstep append
    /// the MQTT drop path uses) and build the runner. Callers must have already
    /// run `validate_meta` so output names are unique and collision-free.
    pub fn new(
        name: String,
        meta: ScriptMeta,
        compiled: Box<dyn CompiledScript>,
        store: &dyn ChannelStore,
        registry: &ChannelRegistry,
    ) -> Self {
        let mut output_ids = Vec::with_capacity(meta.outputs.len());
        for out in &meta.outputs {
            let is_new = registry.id(&out.name).is_none();
            let id = registry.add_dynamic(&out.name, &out.name, out.sample_type);
            if is_new {
                store.add_channel(registry.meta(id).clone());
            }
            output_ids.push(id);
        }
        let input_ids = vec![None; meta.inputs.len()];
        let last_written = vec![i64::MIN; meta.outputs.len()];
        Self { name, meta, compiled, output_ids, input_ids, last_written, state: ScriptState::Healthy }
    }

    pub fn name(&self) -> &str {
        &self.name
    }

    pub fn state(&self) -> &ScriptState {
        &self.state
    }

    /// Run one tick: resolve inputs, gather windows, call the compiled script,
    /// and append new output samples (dedup by timestamp).
    pub fn tick(
        &mut self,
        store: &dyn ChannelStore,
        registry: &ChannelRegistry,
        window: TimeWindow,
    ) {
        if let ScriptState::Failed(_) = self.state {
            return; // A failed script stays parked until reloaded.
        }

        // Resolve any inputs not yet bound (a later-registered channel, e.g.
        // another script's output, resolves on a subsequent tick).
        for (i, name) in self.meta.inputs.iter().enumerate() {
            if self.input_ids[i].is_none() {
                self.input_ids[i] = registry.id(name);
            }
        }
        if let Some(k) = self.input_ids.iter().position(|o| o.is_none()) {
            self.state = ScriptState::Waiting(self.meta.inputs[k].clone());
            return;
        }

        // Gather each input's window.
        let mut windows = Vec::with_capacity(self.input_ids.len());
        for id in self.input_ids.iter().map(|o| o.unwrap()) {
            let (ts, vals) = snapshot_to_f64(store.snapshot(id, window));
            windows.push(InputWindow { ts, vals });
        }
        if windows.iter().any(|w| w.ts.is_empty()) {
            self.state = ScriptState::Waiting("data".to_string());
            return;
        }

        // Run and publish.
        match self.compiled.run(&windows) {
            Ok(batches) => {
                if batches.len() != self.meta.outputs.len() {
                    self.state = ScriptState::Failed(format!(
                        "compute returned {} outputs, expected {}",
                        batches.len(),
                        self.meta.outputs.len()
                    ));
                    return;
                }
                for (i, batch) in batches.iter().enumerate() {
                    if batch.ts.len() != batch.vals.len() {
                        self.state = ScriptState::Failed(format!(
                            "output '{}': ts/vals length mismatch ({} vs {})",
                            self.meta.outputs[i].name,
                            batch.ts.len(),
                            batch.vals.len()
                        ));
                        return;
                    }
                    let id = self.output_ids[i];
                    let sty = self.meta.outputs[i].sample_type;
                    for (&t, &v) in batch.ts.iter().zip(&batch.vals) {
                        if t > self.last_written[i] {
                            store.write_numeric(id, t, cast_to(sty, v));
                            self.last_written[i] = t;
                        }
                    }
                }
                self.state = ScriptState::Healthy;
            }
            Err(e) => self.state = ScriptState::Failed(e),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::script::types::{OutputBatch, OutputSpec};
    use crate::store::LiveStore;
    use crate::types::Sample;

    /// A fake compiled script that runs a Rust closure — lets us test the
    /// scheduler with no Python.
    struct FakeScript<F: FnMut(&[InputWindow]) -> Result<Vec<OutputBatch>, String> + Send>(F);
    impl<F: FnMut(&[InputWindow]) -> Result<Vec<OutputBatch>, String> + Send> CompiledScript
        for FakeScript<F>
    {
        fn run(&mut self, inputs: &[InputWindow]) -> Result<Vec<OutputBatch>, String> {
            (self.0)(inputs)
        }
    }

    fn registry_with_input() -> ChannelRegistry {
        ChannelRegistry::from_toml_str(
            "[channels.\"in.a\"]\ntype = \"float\"\nmax_rate = 100\nhistory_s = 1.0\n",
        )
        .unwrap()
    }

    fn out(name: &str) -> OutputSpec {
        OutputSpec { name: name.into(), sample_type: SampleType::Float, unit: String::new() }
    }

    const ALL: TimeWindow = TimeWindow { start_ns: i64::MIN, end_ns: i64::MAX };

    #[test]
    fn registers_output_and_writes_element_wise() {
        let reg = registry_with_input();
        let store = LiveStore::from_registry(&reg);
        let in_id = reg.id("in.a").unwrap();
        store.write_numeric(in_id, 10, NumericVal::Float(2.0));
        store.write_numeric(in_id, 20, NumericVal::Float(3.0));

        let meta = ScriptMeta { inputs: vec!["in.a".into()], outputs: vec![out("in.a.double")] };
        // Element-wise: double each value, keep timestamps.
        let compiled = Box::new(FakeScript(|inp: &[InputWindow]| {
            let w = &inp[0];
            Ok(vec![OutputBatch {
                ts: w.ts.clone(),
                vals: w.vals.iter().map(|v| v * 2.0).collect(),
            }])
        }));
        let mut runner = ScriptRunner::new("dbl".into(), meta, compiled, &store, &reg);

        runner.tick(&store, &reg, ALL);
        assert_eq!(runner.state(), &ScriptState::Healthy);

        let out_id = reg.id("in.a.double").unwrap();
        match store.snapshot(out_id, ALL) {
            ChannelSnapshot::Float { ts, vals } => {
                assert_eq!(ts, vec![10, 20]);
                assert_eq!(vals, vec![4.0, 6.0]);
            }
            other => panic!("wrong variant: {other:?}"),
        }
    }

    #[test]
    fn dedups_overlapping_windows() {
        let reg = registry_with_input();
        let store = LiveStore::from_registry(&reg);
        let in_id = reg.id("in.a").unwrap();
        let meta = ScriptMeta { inputs: vec!["in.a".into()], outputs: vec![out("o")] };
        let compiled = Box::new(FakeScript(|inp: &[InputWindow]| {
            let w = &inp[0];
            Ok(vec![OutputBatch { ts: w.ts.clone(), vals: w.vals.clone() }])
        }));
        let mut runner = ScriptRunner::new("id".into(), meta, compiled, &store, &reg);
        let out_id = reg.id("o").unwrap();

        store.write_numeric(in_id, 1, NumericVal::Float(1.0));
        runner.tick(&store, &reg, ALL); // writes ts=1
        store.write_numeric(in_id, 2, NumericVal::Float(2.0));
        runner.tick(&store, &reg, ALL); // window is {1,2}; only ts=2 is new

        match store.snapshot(out_id, ALL) {
            ChannelSnapshot::Float { ts, .. } => assert_eq!(ts, vec![1, 2]),
            other => panic!("wrong variant: {other:?}"),
        }
    }

    #[test]
    fn waits_for_unregistered_input() {
        let reg = registry_with_input();
        let store = LiveStore::from_registry(&reg);
        let meta = ScriptMeta { inputs: vec!["not.there".into()], outputs: vec![out("o")] };
        let compiled = Box::new(FakeScript(|_: &[InputWindow]| Ok(vec![])));
        let mut runner = ScriptRunner::new("w".into(), meta, compiled, &store, &reg);
        runner.tick(&store, &reg, ALL);
        assert_eq!(runner.state(), &ScriptState::Waiting("not.there".to_string()));
    }

    #[test]
    fn waits_when_input_has_no_data() {
        let reg = registry_with_input();
        let store = LiveStore::from_registry(&reg);
        let meta = ScriptMeta { inputs: vec!["in.a".into()], outputs: vec![out("o")] };
        let compiled = Box::new(FakeScript(|_: &[InputWindow]| Ok(vec![])));
        let mut runner = ScriptRunner::new("w".into(), meta, compiled, &store, &reg);
        runner.tick(&store, &reg, ALL);
        assert_eq!(runner.state(), &ScriptState::Waiting("data".to_string()));
    }

    #[test]
    fn reduction_writes_single_sample_and_casts_int() {
        let reg = registry_with_input();
        let store = LiveStore::from_registry(&reg);
        let in_id = reg.id("in.a").unwrap();
        store.write_numeric(in_id, 10, NumericVal::Float(3.0));
        store.write_numeric(in_id, 20, NumericVal::Float(5.0));

        let meta = ScriptMeta {
            inputs: vec!["in.a".into()],
            outputs: vec![OutputSpec {
                name: "count".into(),
                sample_type: SampleType::Int,
                unit: String::new(),
            }],
        };
        // Reduction: one sample at the latest ts, value = count of samples.
        let compiled = Box::new(FakeScript(|inp: &[InputWindow]| {
            let w = &inp[0];
            Ok(vec![OutputBatch { ts: vec![*w.ts.last().unwrap()], vals: vec![w.ts.len() as f64] }])
        }));
        let mut runner = ScriptRunner::new("c".into(), meta, compiled, &store, &reg);
        runner.tick(&store, &reg, ALL);

        let out_id = reg.id("count").unwrap();
        assert_eq!(store.latest(out_id), Some((20, Sample::Int(2))));
    }

    #[test]
    fn wrong_output_count_fails_script() {
        let reg = registry_with_input();
        let store = LiveStore::from_registry(&reg);
        let in_id = reg.id("in.a").unwrap();
        store.write_numeric(in_id, 1, NumericVal::Float(1.0));
        let meta = ScriptMeta { inputs: vec!["in.a".into()], outputs: vec![out("o")] };
        let compiled = Box::new(FakeScript(|_: &[InputWindow]| Ok(vec![]))); // 0 != 1
        let mut runner = ScriptRunner::new("f".into(), meta, compiled, &store, &reg);
        runner.tick(&store, &reg, ALL);
        assert!(matches!(runner.state(), ScriptState::Failed(_)));
    }

    #[test]
    fn runtime_error_fails_script() {
        let reg = registry_with_input();
        let store = LiveStore::from_registry(&reg);
        let in_id = reg.id("in.a").unwrap();
        store.write_numeric(in_id, 1, NumericVal::Float(1.0));
        let meta = ScriptMeta { inputs: vec!["in.a".into()], outputs: vec![out("o")] };
        let compiled = Box::new(FakeScript(|_: &[InputWindow]| Err("boom".to_string())));
        let mut runner = ScriptRunner::new("f".into(), meta, compiled, &store, &reg);
        runner.tick(&store, &reg, ALL);
        assert_eq!(runner.state(), &ScriptState::Failed("boom".to_string()));
    }

    /// Re-enabling a script (disable → enable) calls `ScriptRunner::new` twice
    /// for the same output name against the same registry+store. Before the fix,
    /// the second `new` called `store.add_channel` again even though `o1` was
    /// already registered, creating an orphan slot. Any subsequent `add_dynamic`
    /// call then got a registry id that indexed the orphan slot instead of the
    /// real one, corrupting reads for every channel registered after re-enable.
    ///
    /// This test proves the invariant holds: after a re-enable cycle, a freshly
    /// registered `o2` channel's registry id correctly indexes its own store slot.
    #[test]
    fn reenable_does_not_desync_registry_and_store() {
        let reg = registry_with_input();
        let store = LiveStore::from_registry(&reg);
        let in_id = reg.id("in.a").unwrap();
        store.write_numeric(in_id, 10, NumericVal::Float(1.0));

        // First enable: creates runner, ticks, confirms o1 works.
        let make_meta = || ScriptMeta {
            inputs: vec!["in.a".into()],
            outputs: vec![out("o1")],
        };
        let make_script = || {
            Box::new(FakeScript(|inp: &[InputWindow]| {
                let w = &inp[0];
                Ok(vec![OutputBatch { ts: w.ts.clone(), vals: w.vals.clone() }])
            }))
        };

        let mut runner1 = ScriptRunner::new("s".into(), make_meta(), make_script(), &store, &reg);
        runner1.tick(&store, &reg, ALL);
        assert_eq!(runner1.state(), &ScriptState::Healthy);
        let o1_id = reg.id("o1").unwrap();
        assert!(store.latest(o1_id).is_some(), "o1 slot must be populated after first tick");

        // Simulate re-enable: second ScriptRunner::new for the same output name
        // against the same store+registry. Before the fix, this pushed an orphan
        // slot at index o1_id.0 + 1, shifting every subsequent id by one.
        let _runner2 = ScriptRunner::new("s".into(), make_meta(), make_script(), &store, &reg);

        // Now register a NEW channel (as MQTT ingest / another script would do).
        let o2_is_new = reg.id("o2").is_none();
        assert!(o2_is_new, "o2 must not exist yet");
        let o2_id = reg.add_dynamic("o2", "o2", SampleType::Float);
        store.add_channel(reg.meta(o2_id).clone());

        // Write a distinctive value to o2's slot and read it back via o2_id.
        store.write_numeric(o2_id, 99, NumericVal::Float(42.0));

        // Before the fix: o2_id pointed at the orphan slot (empty / wrong type).
        // After the fix: o2_id correctly points at o2's slot, returning 42.0.
        assert_eq!(
            store.latest(o2_id),
            Some((99, crate::types::Sample::Float(42.0))),
            "o2 registry id must index o2's store slot — registry/store are desynced"
        );
    }
}
