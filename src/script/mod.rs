//! Python scripting engine: run numba-compiled user scripts that read
//! channels, compute, and publish new channels.
//!
//! [`ScriptEngine`] is a [`crate::ingest::DataSource`] that ticks every enabled
//! script on a background thread at ~60 Hz. Each script self-declares `INPUTS`
//! and `OUTPUTS` and provides a `@numba.njit` `compute(ts, vals)`; the engine
//! marshals per-input `(ts, vals)` numpy arrays in, appends the `(ts, vals)`
//! pairs it returns (deduped by timestamp), and registers its outputs as
//! ordinary channels. numba is required — without it the engine disables
//! itself. See the crate-level "Writing a Python script" guide for the script
//! contract, and [`config::ScriptsConfig`] for the `[scripts]` config.

pub mod config;
pub mod types;
pub mod runner;
#[cfg(feature = "scripting")]
pub mod python;
pub mod panel;

pub use runner::ScriptState;

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU8, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use crossbeam_channel::{Receiver, Sender};

use crate::config::ChannelRegistry;
use crate::ingest::{DataSource, SourceHandle, CONNECTING, LIVE, TIMEOUT};
use crate::script::config::ScriptInstance;
use crate::script::runner::ScriptRunner;
use crate::script::types::{
    expand_output_name, parse_sample_type, validate_meta, OutputSpec, ScriptLoader, ScriptMeta,
};
use crate::store::ChannelStore;
use crate::types::TimeWindow;

/// One script's status for the GUI.
#[derive(Debug, Clone, PartialEq)]
pub struct ScriptStatus {
    pub name: String,
    pub state: ScriptState,
}

/// Shared per-script status list, updated by the engine thread each tick.
pub type SharedStatus = Arc<Mutex<Vec<ScriptStatus>>>;

/// Stem → default `ScriptMeta`, peeked from each discovered script for the GUI editor.
pub type SharedMetas = Arc<Mutex<HashMap<String, ScriptMeta>>>;

/// GUI → engine live control.
pub enum ScriptCommand {
    /// Add a new instance or replace an existing one with the same id.
    Upsert(ScriptInstance),
    /// Remove the instance with this id.
    Remove(String),
}

/// Background engine that loads, compiles, and ticks scripts.
pub struct ScriptEngine {
    dir: PathBuf,
    instances: Vec<ScriptInstance>,
    window_s: f64,
    loader: Box<dyn ScriptLoader>,
    registry: Arc<ChannelRegistry>,
    status: SharedStatus,
    commands: (Sender<ScriptCommand>, Receiver<ScriptCommand>),
    disabled: Arc<Mutex<Option<String>>>,
    /// Per-instance owned output names (id → output channel names). Used to
    /// distinguish a rebuild (reuse the slot) from a real collision with a
    /// non-script channel.
    owned_outputs: Arc<Mutex<HashMap<String, Vec<String>>>>,
    /// Stem → default `ScriptMeta`, peeked on startup for the GUI editor.
    metas: SharedMetas,
    /// Capability probe run before ticking. Real engine uses numba; tests pass
    /// `|| Ok(())` or `|| Err(..)`.
    probe: Box<dyn Fn() -> Result<(), String> + Send>,
}

impl ScriptEngine {
    pub fn new(
        dir: PathBuf,
        instances: Vec<ScriptInstance>,
        window_s: f64,
        loader: Box<dyn ScriptLoader>,
        registry: Arc<ChannelRegistry>,
        probe: Box<dyn Fn() -> Result<(), String> + Send>,
    ) -> Self {
        Self {
            dir,
            instances,
            window_s,
            loader,
            registry,
            status: Arc::new(Mutex::new(Vec::new())),
            commands: crossbeam_channel::unbounded(),
            disabled: Arc::new(Mutex::new(None)),
            owned_outputs: Arc::new(Mutex::new(HashMap::new())),
            metas: Arc::new(Mutex::new(HashMap::new())),
            probe,
        }
    }

    pub fn status(&self) -> SharedStatus {
        self.status.clone()
    }

    pub fn commands(&self) -> Sender<ScriptCommand> {
        self.commands.0.clone()
    }

    pub fn disabled_reason(&self) -> Arc<Mutex<Option<String>>> {
        self.disabled.clone()
    }

    pub fn script_metas(&self) -> SharedMetas {
        self.metas.clone()
    }

    /// Read a script's source from `dir/<name>.py`.
    fn read_source(&self, name: &str) -> Result<String, String> {
        let path = self.dir.join(format!("{name}.py"));
        std::fs::read_to_string(&path).map_err(|e| format!("reading {}: {e}", path.display()))
    }

    /// Load, validate, register, and build a runner for one instance.
    pub fn build_runner(
        &self,
        inst: &ScriptInstance,
        store: &dyn ChannelStore,
    ) -> Result<ScriptRunner, ScriptStatus> {
        let fail = |e: String| ScriptStatus { name: inst.id.clone(), state: ScriptState::Failed(e) };
        let source = self.read_source(&inst.script).map_err(&fail)?;
        let loaded = self.loader.load(&source, &inst.script).map_err(&fail)?;

        // Resolve inputs: instance override, else the file's declared inputs.
        let inputs = inst.inputs.clone().unwrap_or_else(|| loaded.meta.inputs.clone());
        if inputs.len() != loaded.meta.inputs.len() {
            return Err(fail(format!(
                "instance binds {} inputs but script '{}' declares {}",
                inputs.len(),
                inst.script,
                loaded.meta.inputs.len()
            )));
        }

        // Resolve outputs: instance override (parsed), else the file's outputs.
        let raw_outputs: Vec<OutputSpec> = match &inst.outputs {
            Some(obs) => {
                let mut v = Vec::with_capacity(obs.len());
                for o in obs {
                    v.push(OutputSpec {
                        name: o.name.clone(),
                        sample_type: parse_sample_type(&o.ty).map_err(&fail)?,
                        unit: o.unit.clone(),
                    });
                }
                v
            }
            None => loaded.meta.outputs.clone(),
        };

        // Expand output-name templates against the resolved inputs.
        let mut outputs = Vec::with_capacity(raw_outputs.len());
        for o in raw_outputs {
            outputs.push(OutputSpec {
                name: expand_output_name(&o.name, &inputs).map_err(&fail)?,
                sample_type: o.sample_type,
                unit: o.unit,
            });
        }

        let meta = ScriptMeta { inputs, outputs };

        // Collision check: an output name already known and owned by NO instance is a
        // real clash with a non-script channel. Names this same id already owns are
        // fine (rebuild reuses the slot).
        let mut owned = self.owned_outputs.lock().unwrap();
        let owned_by_others: std::collections::HashSet<&str> = owned
            .iter()
            .filter(|(k, _)| k.as_str() != inst.id)
            .flat_map(|(_, v)| v.iter().map(String::as_str))
            .collect();
        let exists = |n: &str| self.registry.id(n).is_some() && !owned_by_others.contains(n);
        validate_meta(&meta, exists).map_err(&fail)?;
        owned.insert(inst.id.clone(), meta.outputs.iter().map(|o| o.name.clone()).collect());
        drop(owned);

        Ok(ScriptRunner::new(inst.id.clone(), meta, loaded.compiled, store, &self.registry))
    }

    /// Publish the current runners' states plus any load failures into the
    /// shared status list the GUI reads.
    fn publish_status(status: &SharedStatus, runners: &[ScriptRunner], failed: &[ScriptStatus]) {
        let mut out: Vec<ScriptStatus> = runners
            .iter()
            .map(|r| ScriptStatus { name: r.name().to_string(), state: r.state().clone() })
            .collect();
        out.extend_from_slice(failed);
        *status.lock().unwrap() = out;
    }

    /// Load/reload one instance into `runners`, removing any prior version.
    /// Disabled instances are skipped (removed from active set but not failed).
    fn load_into(
        &self,
        inst: &ScriptInstance,
        store: &dyn ChannelStore,
        runners: &mut Vec<ScriptRunner>,
        failed: &mut Vec<ScriptStatus>,
    ) {
        runners.retain(|r| r.name() != inst.id);
        failed.retain(|f| f.name != inst.id);
        self.owned_outputs.lock().unwrap().remove(&inst.id);
        if !inst.enabled {
            return;
        }
        match self.build_runner(inst, store) {
            Ok(runner) => runners.push(runner),
            Err(status) => failed.push(status),
        }
    }

    /// Remove an instance entirely from the active set.
    fn remove_instance(
        &self,
        id: &str,
        runners: &mut Vec<ScriptRunner>,
        failed: &mut Vec<ScriptStatus>,
    ) {
        runners.retain(|r| r.name() != id);
        failed.retain(|f| f.name != id);
        self.owned_outputs.lock().unwrap().remove(id);
    }

    fn run_loop(self, store: Arc<dyn ChannelStore>, conn_state: Arc<AtomicU8>) {
        // Capability gate: without numba the whole feature is disabled.
        if let Err(e) = (self.probe)() {
            *self.disabled.lock().unwrap() = Some(format!("scripting unavailable: {e}"));
            conn_state.store(TIMEOUT, Ordering::Relaxed);
            return;
        }

        // Peek each discovered script's default meta for the GUI editor.
        {
            let mut metas = self.metas.lock().unwrap();
            for stem in discover_scripts(&self.dir) {
                if let Ok(src) = self.read_source(&stem) {
                    if let Ok(meta) = self.loader.peek_meta(&src, &stem) {
                        metas.insert(stem, meta);
                    }
                }
            }
        }

        let mut runners: Vec<ScriptRunner> = Vec::new();
        let mut failed: Vec<ScriptStatus> = Vec::new();
        for inst in self.instances.clone() {
            self.load_into(&inst, store.as_ref(), &mut runners, &mut failed);
        }
        conn_state.store(LIVE, Ordering::Relaxed);

        let tick = Duration::from_millis(16);
        loop {
            // Drain one command, blocking up to a tick so updates are prompt.
            match self.commands.1.recv_timeout(tick) {
                Ok(ScriptCommand::Upsert(inst)) => {
                    self.load_into(&inst, store.as_ref(), &mut runners, &mut failed)
                }
                Ok(ScriptCommand::Remove(id)) => {
                    self.remove_instance(&id, &mut runners, &mut failed)
                }
                Err(crossbeam_channel::RecvTimeoutError::Timeout) => {}
                Err(crossbeam_channel::RecvTimeoutError::Disconnected) => break,
            }

            // Tick all runners.
            let now = store.now_ns();
            let window = TimeWindow::last((self.window_s * 1e9) as i64, now);
            for runner in &mut runners {
                runner.tick(store.as_ref(), &self.registry, window);
            }
            Self::publish_status(&self.status, &runners, &failed);
        }
    }
}

impl DataSource for ScriptEngine {
    fn name(&self) -> &str {
        "scripts"
    }

    fn spawn(self: Box<Self>, store: Arc<dyn ChannelStore>) -> SourceHandle {
        let conn_state = Arc::new(AtomicU8::new(CONNECTING));
        let record_sender = Arc::new(Mutex::new(None));
        let state_for_thread = conn_state.clone();
        let engine = *self;
        std::thread::spawn(move || engine.run_loop(store, state_for_thread));
        SourceHandle {
            name: "scripts".to_string(),
            conn_state,
            record_sender,
            discovery: None,
            schema_bytes: None,
        }
    }
}

/// Every `*.py` stem in `dir` (sorted). Missing dir → empty.
pub fn discover_scripts(dir: &std::path::Path) -> Vec<String> {
    let mut names = Vec::new();
    if let Ok(entries) = std::fs::read_dir(dir) {
        for e in entries.flatten() {
            let p = e.path();
            if p.extension().and_then(|x| x.to_str()) == Some("py") {
                if let Some(stem) = p.file_stem().and_then(|s| s.to_str()) {
                    names.push(stem.to_string());
                }
            }
        }
    }
    names.sort();
    names
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::script::types::{
        CompiledScript, InputWindow, LoadedScript, OutputBatch, OutputSpec, ScriptMeta,
    };
    use crate::store::LiveStore;
    use crate::types::{ChannelSnapshot, NumericVal, SampleType};

    struct DoublerLoader;
    struct Doubler;
    impl CompiledScript for Doubler {
        fn run(&mut self, inputs: &[InputWindow]) -> Result<Vec<OutputBatch>, String> {
            let w = &inputs[0];
            Ok(vec![OutputBatch { ts: w.ts.clone(), vals: w.vals.iter().map(|v| v * 2.0).collect() }])
        }
    }
    impl ScriptLoader for DoublerLoader {
        fn load(&self, _source: &str, _name: &str) -> Result<LoadedScript, String> {
            Ok(LoadedScript {
                meta: ScriptMeta {
                    inputs: vec!["in.a".into()],
                    outputs: vec![OutputSpec {
                        name: "in.a.double".into(),
                        sample_type: SampleType::Float,
                        unit: String::new(),
                    }],
                },
                compiled: Box::new(Doubler),
            })
        }
        fn peek_meta(&self, _source: &str, _name: &str) -> Result<ScriptMeta, String> {
            Ok(ScriptMeta {
                inputs: vec!["in.a".into()],
                outputs: vec![OutputSpec {
                    name: "in.a.double".into(),
                    sample_type: SampleType::Float,
                    unit: String::new(),
                }],
            })
        }
    }

    struct TemplateDoublerLoader;
    impl ScriptLoader for TemplateDoublerLoader {
        fn load(&self, _s: &str, _n: &str) -> Result<LoadedScript, String> {
            Ok(LoadedScript {
                meta: ScriptMeta {
                    inputs: vec!["in.a".into()],
                    outputs: vec![OutputSpec {
                        name: "{in0}.double".into(),
                        sample_type: SampleType::Float,
                        unit: String::new(),
                    }],
                },
                compiled: Box::new(Doubler),
            })
        }
        fn peek_meta(&self, _s: &str, _n: &str) -> Result<ScriptMeta, String> {
            self.load("", "").map(|l| l.meta)
        }
    }

    fn instance(id: &str, script: &str, inputs: Option<Vec<&str>>) -> crate::script::config::ScriptInstance {
        crate::script::config::ScriptInstance {
            id: id.into(),
            script: script.into(),
            inputs: inputs.map(|v| v.into_iter().map(String::from).collect()),
            outputs: None,
            enabled: true,
        }
    }

    #[test]
    fn engine_ticks_and_writes_outputs() {
        let reg = Arc::new(
            ChannelRegistry::from_toml_str(
                "[channels.\"in.a\"]\ntype = \"float\"\nmax_rate = 100\nhistory_s = 1.0\n",
            )
            .unwrap(),
        );
        let store: Arc<dyn ChannelStore> = Arc::new(LiveStore::from_registry(&reg));
        let in_id = reg.id("in.a").unwrap();
        // Write at a current timestamp so the sample falls inside the engine's
        // "last window_s" real-time window (unlike a fixed tiny ts, which would
        // be far outside [now - window, now] and leave the runner Waiting).
        store.write_numeric(in_id, store.now_ns(), NumericVal::Float(2.0));

        // A temp dir with a placeholder file (the fake loader ignores contents).
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("dbl.py"), "# fake").unwrap();

        let engine = ScriptEngine::new(
            dir.path().to_path_buf(),
            vec![instance("dbl", "dbl", None)],
            10.0,
            Box::new(DoublerLoader),
            reg.clone(),
            Box::new(|| Ok(())),
        );
        let status = engine.status();
        let handle = Box::new(engine).spawn(store.clone());
        assert_eq!(handle.name, "scripts");

        // Give the loop a few ticks to load and run.
        let out_id = loop_until(|| reg.id("in.a.double"), 2000);
        let deadline = std::time::Instant::now() + Duration::from_millis(2000);
        loop {
            if let ChannelSnapshot::Float { vals, .. } = store.snapshot(out_id, super::TimeWindow { start_ns: i64::MIN, end_ns: i64::MAX }) {
                if vals == vec![4.0] {
                    break;
                }
            }
            assert!(std::time::Instant::now() < deadline, "output never written");
            std::thread::sleep(Duration::from_millis(20));
        }
        let s = status.lock().unwrap();
        assert_eq!(s.iter().find(|x| x.name == "dbl").map(|x| &x.state), Some(&ScriptState::Healthy));
    }

    #[test]
    fn failed_probe_disables_engine() {
        let reg = Arc::new(ChannelRegistry::from_toml_str("default_window_s = 5.0\n").unwrap());
        let store: Arc<dyn ChannelStore> = Arc::new(LiveStore::from_registry(&reg));
        let dir = tempfile::tempdir().unwrap();
        let engine = ScriptEngine::new(
            dir.path().to_path_buf(),
            vec![],
            10.0,
            Box::new(DoublerLoader),
            reg,
            Box::new(|| Err("no numba".into())),
        );
        let disabled = engine.disabled_reason();
        let _ = Box::new(engine).spawn(store);
        let deadline = std::time::Instant::now() + Duration::from_millis(1000);
        loop {
            if disabled.lock().unwrap().is_some() {
                break;
            }
            assert!(std::time::Instant::now() < deadline, "engine never reported disabled");
            std::thread::sleep(Duration::from_millis(10));
        }
        assert!(disabled.lock().unwrap().as_ref().unwrap().contains("no numba"));
    }

    #[test]
    fn build_runner_uses_instance_id_and_input_override() {
        // Registry has two inputs; TemplateDoublerLoader declares arity 1 with a templated
        // default output. An instance overrides the input to "in.b".
        let reg = Arc::new(
            ChannelRegistry::from_toml_str(
                "[channels.\"in.a\"]\ntype=\"float\"\nmax_rate=100\nhistory_s=1.0\n\
                 [channels.\"in.b\"]\ntype=\"float\"\nmax_rate=100\nhistory_s=1.0\n",
            )
            .unwrap(),
        );
        let store: Arc<dyn ChannelStore> = Arc::new(LiveStore::from_registry(&reg));
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("dbl.py"), "# fake").unwrap();

        let engine = ScriptEngine::new(
            dir.path().to_path_buf(),
            vec![instance("first", "dbl", Some(vec!["in.b"]))],
            10.0,
            Box::new(TemplateDoublerLoader),
            reg.clone(),
            Box::new(|| Ok(())),
        );
        let runner = engine.build_runner(&instance("first", "dbl", Some(vec!["in.b"])), store.as_ref()).unwrap();
        assert_eq!(runner.name(), "first");                 // keyed by id, not stem
        assert!(reg.id("in.b.double").is_some());           // {in0}.double expanded from in.b
    }

    #[test]
    fn build_runner_rejects_arity_mismatch() {
        let reg = Arc::new(
            ChannelRegistry::from_toml_str(
                "[channels.\"in.a\"]\ntype=\"float\"\nmax_rate=100\nhistory_s=1.0\n",
            )
            .unwrap(),
        );
        let store: Arc<dyn ChannelStore> = Arc::new(LiveStore::from_registry(&reg));
        let dir = tempfile::tempdir().unwrap();
        let engine = ScriptEngine::new(
            dir.path().to_path_buf(),
            vec![],
            10.0,
            Box::new(TemplateDoublerLoader),
            reg,
            Box::new(|| Ok(())),
        );
        // Loader declares arity 1; instance binds two inputs.
        let bad = instance("x", "dbl", Some(vec!["in.a", "in.a"]));
        let result = engine.build_runner(&bad, store.as_ref());
        assert!(result.is_err());
        let err = result.err().unwrap();
        assert!(matches!(err.state, ScriptState::Failed(_)));
    }

    #[test]
    fn two_instances_of_one_script_make_distinct_channels() {
        let reg = Arc::new(
            ChannelRegistry::from_toml_str(
                "[channels.\"in.a\"]\ntype=\"float\"\nmax_rate=100\nhistory_s=1.0\n\
                 [channels.\"in.b\"]\ntype=\"float\"\nmax_rate=100\nhistory_s=1.0\n",
            )
            .unwrap(),
        );
        let store: Arc<dyn ChannelStore> = Arc::new(LiveStore::from_registry(&reg));
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("dbl.py"), "# fake").unwrap();
        let engine = ScriptEngine::new(
            dir.path().to_path_buf(),
            vec![],
            10.0,
            Box::new(TemplateDoublerLoader),
            reg.clone(),
            Box::new(|| Ok(())),
        );
        engine.build_runner(&instance("a", "dbl", Some(vec!["in.a"])), store.as_ref()).unwrap();
        engine.build_runner(&instance("b", "dbl", Some(vec!["in.b"])), store.as_ref()).unwrap();
        assert!(reg.id("in.a.double").is_some());
        assert!(reg.id("in.b.double").is_some());
    }

    fn loop_until<T>(mut f: impl FnMut() -> Option<T>, ms: u64) -> T {
        let deadline = std::time::Instant::now() + Duration::from_millis(ms);
        loop {
            if let Some(v) = f() {
                return v;
            }
            assert!(std::time::Instant::now() < deadline, "condition never met");
            std::thread::sleep(Duration::from_millis(10));
        }
    }
}
