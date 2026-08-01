//! The only file that touches PyO3. Compiled behind the `scripting` feature.

use pyo3::prelude::*;

/// Probe whether the numeric stack is importable. numba is the gate: because it
/// depends on numpy, a successful `import numba` proves the whole stack is
/// present. Returns the Python error text on failure.
pub fn probe_numba() -> Result<(), String> {
    Python::with_gil(|py| {
        py.import_bound("numba").map(|_| ()).map_err(|e| e.to_string())
    })
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
}
