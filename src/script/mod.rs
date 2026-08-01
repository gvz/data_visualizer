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

use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU8, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use crossbeam_channel::{Receiver, Sender};

use crate::config::ChannelRegistry;
use crate::ingest::{DataSource, SourceHandle, CONNECTING, LIVE, TIMEOUT};
use crate::script::runner::ScriptRunner;
use crate::script::types::{validate_meta, ScriptLoader};
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

/// GUI → engine live control.
pub enum ScriptCommand {
    Enable(String),
    Disable(String),
}

/// Background engine that loads, compiles, and ticks scripts.
pub struct ScriptEngine {
    dir: PathBuf,
    enabled: Vec<String>,
    window_s: f64,
    loader: Box<dyn ScriptLoader>,
    registry: Arc<ChannelRegistry>,
    status: SharedStatus,
    commands: (Sender<ScriptCommand>, Receiver<ScriptCommand>),
    disabled: Arc<Mutex<Option<String>>>,
    /// Output names this engine has registered, to distinguish a re-enable
    /// (reuse the slot) from a real collision with a non-script channel.
    script_outputs: Arc<Mutex<HashSet<String>>>,
    /// Capability probe run before ticking. Real engine uses numba; tests pass
    /// `|| Ok(())` or `|| Err(..)`.
    probe: Box<dyn Fn() -> Result<(), String> + Send>,
}

impl ScriptEngine {
    pub fn new(
        dir: PathBuf,
        enabled: Vec<String>,
        window_s: f64,
        loader: Box<dyn ScriptLoader>,
        registry: Arc<ChannelRegistry>,
        probe: Box<dyn Fn() -> Result<(), String> + Send>,
    ) -> Self {
        Self {
            dir,
            enabled,
            window_s,
            loader,
            registry,
            status: Arc::new(Mutex::new(Vec::new())),
            commands: crossbeam_channel::unbounded(),
            disabled: Arc::new(Mutex::new(None)),
            script_outputs: Arc::new(Mutex::new(HashSet::new())),
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

    /// Read a script's source from `dir/<name>.py`.
    fn read_source(&self, name: &str) -> Result<String, String> {
        let path = self.dir.join(format!("{name}.py"));
        std::fs::read_to_string(&path).map_err(|e| format!("reading {}: {e}", path.display()))
    }

    /// Load, validate, register, and build a runner for one script.
    fn build_runner(
        &self,
        name: &str,
        store: &dyn ChannelStore,
    ) -> Result<ScriptRunner, ScriptStatus> {
        let fail = |e: String| ScriptStatus { name: name.into(), state: ScriptState::Failed(e) };
        let source = self.read_source(name).map_err(&fail)?;
        let loaded = self.loader.load(&source, name).map_err(&fail)?;

        // Collision check: an output name already known and NOT one of ours is a
        // real clash with a non-script channel.
        let mut owned = self.script_outputs.lock().unwrap();
        let exists = |n: &str| self.registry.id(n).is_some() && !owned.contains(n);
        validate_meta(&loaded.meta, exists).map_err(&fail)?;
        for o in &loaded.meta.outputs {
            owned.insert(o.name.clone());
        }
        drop(owned);

        Ok(ScriptRunner::new(
            name.to_string(),
            loaded.meta,
            loaded.compiled,
            store,
            &self.registry,
        ))
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

    /// Load one script into `runners`, or record it in `failed`.
    ///
    /// Split out of `run_loop` as an inherent method rather than a closure so it
    /// can capture `self` by shared reference while `run_loop`'s body
    /// independently borrows `self.commands`, `self.status`, etc. See the
    /// borrow-checker note in the report; behavior is identical to the brief.
    fn load_into(
        &self,
        name: &str,
        store: &dyn ChannelStore,
        runners: &mut Vec<ScriptRunner>,
        failed: &mut Vec<ScriptStatus>,
    ) {
        if runners.iter().any(|r| r.name() == name) {
            return; // already loaded
        }
        failed.retain(|f| f.name != name);
        match self.build_runner(name, store) {
            Ok(runner) => runners.push(runner),
            Err(status) => failed.push(status),
        }
    }

    fn run_loop(self, store: Arc<dyn ChannelStore>, conn_state: Arc<AtomicU8>) {
        // Capability gate: without numba the whole feature is disabled.
        if let Err(e) = (self.probe)() {
            *self.disabled.lock().unwrap() = Some(format!("scripting unavailable: {e}"));
            conn_state.store(TIMEOUT, Ordering::Relaxed);
            return;
        }

        let mut runners: Vec<ScriptRunner> = Vec::new();
        // Scripts that failed to load — kept for the GUI as Failed entries.
        let mut failed: Vec<ScriptStatus> = Vec::new();

        for name in self.enabled.clone() {
            self.load_into(&name, store.as_ref(), &mut runners, &mut failed);
        }
        conn_state.store(LIVE, Ordering::Relaxed);

        let tick = Duration::from_millis(16);
        loop {
            // Drain one command, blocking up to a tick so toggles are prompt.
            match self.commands.1.recv_timeout(tick) {
                Ok(ScriptCommand::Enable(name)) => {
                    self.load_into(&name, store.as_ref(), &mut runners, &mut failed)
                }
                Ok(ScriptCommand::Disable(name)) => {
                    runners.retain(|r| r.name() != name);
                    failed.retain(|f| f.name != name);
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
            vec!["dbl".into()],
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
