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

fn output_sample_type(ty: &str) -> Result<SampleType, String> {
    crate::script::types::parse_sample_type(ty)
}

/// Extract `INPUTS`/`OUTPUTS` from an executed module into a `ScriptMeta`.
fn extract_meta(module: &Bound<'_, PyModule>) -> Result<ScriptMeta, String> {
    let inputs: Vec<String> = module
        .getattr("INPUTS")
        .map_err(|_| "script is missing INPUTS".to_string())?
        .extract()
        .map_err(|e| format!("INPUTS must be a list of strings: {e}"))?;

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
        let unit: String = item.get_item("unit").and_then(|v| v.extract()).unwrap_or_default();
        outputs.push(OutputSpec { name: out_name, sample_type: output_sample_type(&ty)?, unit });
    }
    Ok(ScriptMeta { inputs, outputs })
}

impl ScriptLoader for PyScriptLoader {
    fn load(&self, source: &str, name: &str) -> Result<LoadedScript, String> {
        Python::with_gil(|py| {
            // Exec the module source in a fresh namespace.
            let module = PyModule::from_code_bound(py, source, &format!("{name}.py"), name)
                .map_err(|e| e.to_string())?;

            let meta = extract_meta(&module)?;

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
            warm_up(py, &compute, meta.inputs.len())
                .map_err(|e| format!("numba compile failed: {e}"))?;

            let n_outputs = meta.outputs.len();
            let compiled: Box<dyn CompiledScript> =
                Box::new(PyScript { compute: compute.unbind(), n_outputs });
            Ok(LoadedScript { meta, compiled })
        })
    }

    fn peek_meta(&self, source: &str, name: &str) -> Result<ScriptMeta, String> {
        Python::with_gil(|py| {
            let module = PyModule::from_code_bound(py, source, &format!("{name}.py"), name)
                .map_err(|e| e.to_string())?;
            extract_meta(&module)
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

impl CompiledScript for PyScript {
    fn run(&mut self, inputs: &[InputWindow]) -> Result<Vec<OutputBatch>, String> {
        use numpy::PyArray1;
        use pyo3::types::PyTuple;

        Python::with_gil(|py| {
            // Build ts and vals tuples of numpy arrays, in INPUTS order.
            let ts_arrays: Vec<_> =
                inputs.iter().map(|w| PyArray1::from_slice_bound(py, &w.ts)).collect();
            let val_arrays: Vec<_> =
                inputs.iter().map(|w| PyArray1::from_slice_bound(py, &w.vals)).collect();
            let ts_tuple = PyTuple::new_bound(py, &ts_arrays);
            let vals_tuple = PyTuple::new_bound(py, &val_arrays);

            let result = self
                .compute
                .bind(py)
                .call1((ts_tuple, vals_tuple))
                .map_err(|e| e.to_string())?;

            // Normalise the return into a list of (ts, vals) pairs. A single
            // output may be returned as a bare 2-tuple; several as a tuple of
            // pairs. Distinguish by inspecting the first element.
            let pairs: Vec<Bound<'_, PyAny>> = if self.n_outputs == 1 {
                // Could be (ts, vals) directly, or ((ts, vals),) — handle both.
                let tup = result.downcast::<PyTuple>().map_err(|_| {
                    "compute must return a (ts, vals) tuple".to_string()
                })?;
                if tup.len() == 2 && tup.get_item(0).map_or(false, |x| is_array(&x)) {
                    vec![result.clone()]
                } else {
                    tup.iter().collect()
                }
            } else {
                let tup = result.downcast::<PyTuple>().map_err(|_| {
                    "compute must return a tuple of (ts, vals) pairs".to_string()
                })?;
                tup.iter().collect()
            };

            if pairs.len() != self.n_outputs {
                return Err(format!(
                    "compute returned {} outputs, expected {}",
                    pairs.len(),
                    self.n_outputs
                ));
            }

            let mut batches = Vec::with_capacity(pairs.len());
            for pair in pairs {
                let pair = pair
                    .downcast::<PyTuple>()
                    .map_err(|_| "each output must be a (ts, vals) tuple".to_string())?;
                if pair.len() != 2 {
                    return Err("each output must be a (ts, vals) pair".to_string());
                }
                let ts = extract_i64(&pair.get_item(0).map_err(|e| e.to_string())?)?;
                let vals = extract_f64(&pair.get_item(1).map_err(|e| e.to_string())?)?;
                batches.push(OutputBatch { ts, vals });
            }
            Ok(batches)
        })
    }
}

/// True if the object is a numpy ndarray (used to disambiguate the single-output
/// bare-pair return from a tuple-of-pairs return).
fn is_array(obj: &Bound<'_, PyAny>) -> bool {
    obj.hasattr("dtype").unwrap_or(false) && obj.hasattr("shape").unwrap_or(false)
}

/// Extract an int64 array, coercing a float array via truncation.
fn extract_i64(obj: &Bound<'_, PyAny>) -> Result<Vec<i64>, String> {
    use numpy::{PyArray1, PyArrayMethods};
    if let Ok(a) = obj.downcast::<PyArray1<i64>>() {
        return Ok(a.to_vec().map_err(|e| e.to_string())?);
    }
    if let Ok(a) = obj.downcast::<PyArray1<f64>>() {
        return Ok(a.to_vec().map_err(|e| e.to_string())?.into_iter().map(|v| v as i64).collect());
    }
    Err("output ts must be an int64 or float64 array".to_string())
}

/// Extract a float64 array, widening an int64 array.
fn extract_f64(obj: &Bound<'_, PyAny>) -> Result<Vec<f64>, String> {
    use numpy::{PyArray1, PyArrayMethods};
    if let Ok(a) = obj.downcast::<PyArray1<f64>>() {
        return Ok(a.to_vec().map_err(|e| e.to_string())?);
    }
    if let Ok(a) = obj.downcast::<PyArray1<i64>>() {
        return Ok(a.to_vec().map_err(|e| e.to_string())?.into_iter().map(|v| v as f64).collect());
    }
    Err("output vals must be a float64 or int64 array".to_string())
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

    #[test]
    fn runs_elementwise_and_returns_pairs() {
        if skip_without_numba() {
            return;
        }
        let mut loaded = PyScriptLoader.load(ELEMENTWISE, "elementwise").unwrap();
        let inputs = vec![
            InputWindow { ts: vec![1, 2], vals: vec![10.0, 20.0] },
            InputWindow { ts: vec![1, 2], vals: vec![1.0, 2.0] },
        ];
        let out = loaded.compiled.run(&inputs).unwrap();
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].ts, vec![1, 2]);
        assert_eq!(out[0].vals, vec![11.0, 22.0]);
    }

    const REDUCTION: &str = r#"
import numpy as np
import numba

INPUTS  = ["a"]
OUTPUTS = [{"name": "rms", "type": "float"}]

@numba.njit
def compute(ts, vals):
    v = vals[0]
    return (ts[0][-1:], np.array([np.sqrt(np.mean(v**2))]))
"#;

    #[test]
    fn peek_meta_reads_bindings_without_compile() {
        let src = "INPUTS=[\"load/ch0\"]\nOUTPUTS=[{\"name\":\"{in0.stem}.rms\",\"type\":\"float\",\"unit\":\"g\"}]\n# no compute at all\n";
        let meta = PyScriptLoader.peek_meta(src, "m").unwrap();
        assert_eq!(meta.inputs, vec!["load/ch0".to_string()]);
        assert_eq!(meta.outputs.len(), 1);
        assert_eq!(meta.outputs[0].name, "{in0.stem}.rms"); // template kept verbatim
        assert_eq!(meta.outputs[0].unit, "g");
    }

    #[test]
    fn runs_reduction_single_sample() {
        if skip_without_numba() {
            return;
        }
        let mut loaded = PyScriptLoader.load(REDUCTION, "reduction").unwrap();
        let inputs = vec![InputWindow { ts: vec![1, 2], vals: vec![3.0, 4.0] }];
        let out = loaded.compiled.run(&inputs).unwrap();
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].ts, vec![2]);
        assert_eq!(out[0].vals.len(), 1);
        assert!((out[0].vals[0] - (12.5f64).sqrt()).abs() < 1e-9);
    }
}
