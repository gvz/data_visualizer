//! The only file that touches PyO3. Compiled behind the `scripting` feature.

use pyo3::prelude::*;

use crate::script::types::{
    CompiledScript, InputWindow, LoadedScript, OutputBatch, OutputSpec, ScriptLoader, ScriptMeta,
};
use crate::types::SampleType;

/// Probe whether the numeric stack is importable. numba is the gate: because it
/// depends on numpy, a successful `import numba` proves the whole stack is
/// present. Returns the Python error text on failure.
pub fn probe_numba() -> Result<(), String> {
    Python::with_gil(|py| {
        py.import_bound("numba").map(|_| ()).map_err(|e| e.to_string())
    })
}

/// A compiled numba script held as a Python callable.
pub struct PyScript {
    compute: Py<PyAny>,
    n_outputs: usize,
}

/// Loads Python source through numba. The gate probe must have already passed.
pub struct PyScriptLoader;

/// Map a declared output `type` string to a numeric `SampleType`. Text is
/// rejected — script outputs are numeric only.
fn output_sample_type(ty: &str) -> Result<SampleType, String> {
    match ty {
        "float" => Ok(SampleType::Float),
        "int" => Ok(SampleType::Int),
        "bool" => Ok(SampleType::Bool),
        other => Err(format!("output type '{other}' is not one of float/int/bool")),
    }
}

impl ScriptLoader for PyScriptLoader {
    fn load(&self, source: &str, name: &str) -> Result<LoadedScript, String> {
        Python::with_gil(|py| {
            // Exec the module source in a fresh namespace.
            let module = PyModule::from_code_bound(py, source, &format!("{name}.py"), name)
                .map_err(|e| e.to_string())?;

            // INPUTS: list[str].
            let inputs: Vec<String> = module
                .getattr("INPUTS")
                .map_err(|_| "script is missing INPUTS".to_string())?
                .extract()
                .map_err(|e| format!("INPUTS must be a list of strings: {e}"))?;

            // OUTPUTS: list[dict] with name/type and optional unit.
            let outputs_obj = module
                .getattr("OUTPUTS")
                .map_err(|_| "script is missing OUTPUTS".to_string())?;
            let mut outputs = Vec::new();
            for item in outputs_obj.iter().map_err(|e| e.to_string())? {
                let item = item.map_err(|e| e.to_string())?;
                let out_name: String = item
                    .get_item("name")
                    .and_then(|v| v.extract())
                    .map_err(|_| "each OUTPUTS entry needs a string 'name'".to_string())?;
                let ty: String = item
                    .get_item("type")
                    .and_then(|v| v.extract())
                    .map_err(|_| "each OUTPUTS entry needs a string 'type'".to_string())?;
                let unit: String = item
                    .get_item("unit")
                    .and_then(|v| v.extract())
                    .unwrap_or_default();
                outputs.push(OutputSpec { name: out_name, sample_type: output_sample_type(&ty)?, unit });
            }

            // compute must be a numba dispatcher (has a `.compile` method).
            let compute = module
                .getattr("compute")
                .map_err(|_| "script is missing a compute function".to_string())?;
            if !compute.hasattr("compile").map_err(|e| e.to_string())? {
                return Err("compute must be decorated @numba.njit".to_string());
            }

            // Eagerly compile now: force numba to specialise compute by calling
            // it once with length-1 dummy tuples matching the input arity. This
            // compiles to native code at load, so the first real tick is warm.
            let n = inputs.len();
            warm_up(py, &compute, n).map_err(|e| format!("numba compile failed: {e}"))?;

            let n_outputs = outputs.len();
            let meta = ScriptMeta { inputs, outputs };
            let compiled: Box<dyn CompiledScript> =
                Box::new(PyScript { compute: compute.unbind(), n_outputs });
            Ok(LoadedScript { meta, compiled })
        })
    }
}

/// Call `compute` once with length-1 dummy `(ts, vals)` tuples to force numba
/// to compile the specialisation for this input arity.
fn warm_up(py: Python<'_>, compute: &Bound<'_, PyAny>, n: usize) -> PyResult<()> {
    use numpy::PyArray1;
    use pyo3::types::PyTuple;

    let ts_arrays: Vec<_> = (0..n).map(|_| PyArray1::from_slice_bound(py, &[0i64])).collect();
    let val_arrays: Vec<_> = (0..n).map(|_| PyArray1::from_slice_bound(py, &[0.0f64])).collect();
    let ts_tuple = PyTuple::new_bound(py, &ts_arrays);
    let vals_tuple = PyTuple::new_bound(py, &val_arrays);
    compute.call1((ts_tuple, vals_tuple))?;
    Ok(())
}

// Placeholder impl so the crate compiles; Task 6 replaces `run`.
impl CompiledScript for PyScript {
    fn run(&mut self, _inputs: &[InputWindow]) -> Result<Vec<OutputBatch>, String> {
        let _ = self.n_outputs;
        Err("PyScript::run not yet implemented".to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn probe_reports_a_result() {
        // In a numba-equipped environment this is Ok; elsewhere it is Err with a
        // message. Either way it must not panic.
        match probe_numba() {
            Ok(()) => {}
            Err(msg) => assert!(!msg.is_empty()),
        }
    }

    const ELEMENTWISE: &str = r#"
import numpy as np
import numba

INPUTS  = ["a", "b"]
OUTPUTS = [{"name": "sum", "type": "float", "unit": "x"}]

@numba.njit
def compute(ts, vals):
    return (ts[0], vals[0] + vals[1])
"#;

    fn skip_without_numba() -> bool {
        if probe_numba().is_err() {
            eprintln!("skipping: numba not available");
            true
        } else {
            false
        }
    }

    #[test]
    fn loads_meta_and_compiles() {
        if skip_without_numba() {
            return;
        }
        let loaded = PyScriptLoader.load(ELEMENTWISE, "elementwise").unwrap();
        assert_eq!(loaded.meta.inputs, vec!["a".to_string(), "b".to_string()]);
        assert_eq!(loaded.meta.outputs.len(), 1);
        assert_eq!(loaded.meta.outputs[0].name, "sum");
        assert_eq!(loaded.meta.outputs[0].sample_type, SampleType::Float);
        assert_eq!(loaded.meta.outputs[0].unit, "x");
    }

    #[test]
    fn rejects_non_njit_compute() {
        if skip_without_numba() {
            return;
        }
        let src = "INPUTS=[\"a\"]\nOUTPUTS=[{\"name\":\"o\",\"type\":\"float\"}]\ndef compute(ts, vals):\n    return (ts[0], vals[0])\n";
        let result = PyScriptLoader.load(src, "plain");
        let err = match result {
            Err(e) => e,
            Ok(_) => panic!("expected an error but load succeeded"),
        };
        assert!(err.contains("numba.njit"), "got: {err}");
    }
}
