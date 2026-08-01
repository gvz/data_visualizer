use crate::types::SampleType;

/// One channel a script publishes.
#[derive(Debug, Clone, PartialEq)]
pub struct OutputSpec {
    pub name: String,
    pub sample_type: SampleType,
    pub unit: String,
}

/// A script's self-declared bindings, read from its `INPUTS`/`OUTPUTS` globals.
#[derive(Debug, Clone, PartialEq)]
pub struct ScriptMeta {
    pub inputs: Vec<String>,
    pub outputs: Vec<OutputSpec>,
}

/// One input channel's window: parallel timestamp and value arrays.
#[derive(Debug, Clone, PartialEq)]
pub struct InputWindow {
    pub ts: Vec<i64>,
    pub vals: Vec<f64>,
}

/// One output channel's samples for a tick: parallel timestamp and value arrays.
#[derive(Debug, Clone, PartialEq)]
pub struct OutputBatch {
    pub ts: Vec<i64>,
    pub vals: Vec<f64>,
}

/// A loaded, compiled script's callable. Abstracted so the scheduler is
/// testable without a Python interpreter.
pub trait CompiledScript: Send {
    /// Run one tick. `inputs` is in `INPUTS` order; the return is in `OUTPUTS`
    /// order, one batch per declared output.
    fn run(&mut self, inputs: &[InputWindow]) -> Result<Vec<OutputBatch>, String>;
}

/// A script's metadata plus its compiled callable.
pub struct LoadedScript {
    pub meta: ScriptMeta,
    pub compiled: Box<dyn CompiledScript>,
}

/// Loads a script's source into metadata + a compiled callable. Implemented by
/// the PyO3 layer; faked in tests.
pub trait ScriptLoader: Send {
    fn load(&self, source: &str, name: &str) -> Result<LoadedScript, String>;
}

/// Validate a script's declared bindings before registering its channels.
/// `channel_exists` reports whether a name is already a channel in the registry
/// (used to reject output-name collisions with non-script channels).
pub fn validate_meta(
    meta: &ScriptMeta,
    channel_exists: impl Fn(&str) -> bool,
) -> Result<(), String> {
    if meta.inputs.is_empty() {
        return Err("INPUTS must list at least one channel".to_string());
    }
    if meta.outputs.is_empty() {
        return Err("OUTPUTS must declare at least one channel".to_string());
    }
    let mut seen = std::collections::HashSet::new();
    for out in &meta.outputs {
        if !seen.insert(out.name.as_str()) {
            return Err(format!("duplicate output channel '{}'", out.name));
        }
        if channel_exists(&out.name) {
            return Err(format!(
                "output '{}' collides with an existing channel",
                out.name
            ));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn spec(name: &str) -> OutputSpec {
        OutputSpec { name: name.into(), sample_type: SampleType::Float, unit: String::new() }
    }

    #[test]
    fn accepts_valid_meta() {
        let meta = ScriptMeta { inputs: vec!["a".into()], outputs: vec![spec("out")] };
        assert!(validate_meta(&meta, |_| false).is_ok());
    }

    #[test]
    fn rejects_empty_inputs() {
        let meta = ScriptMeta { inputs: vec![], outputs: vec![spec("out")] };
        assert!(validate_meta(&meta, |_| false).is_err());
    }

    #[test]
    fn rejects_empty_outputs() {
        let meta = ScriptMeta { inputs: vec!["a".into()], outputs: vec![] };
        assert!(validate_meta(&meta, |_| false).is_err());
    }

    #[test]
    fn rejects_duplicate_output_names() {
        let meta =
            ScriptMeta { inputs: vec!["a".into()], outputs: vec![spec("o"), spec("o")] };
        assert!(validate_meta(&meta, |_| false).is_err());
    }

    #[test]
    fn rejects_collision_with_existing_channel() {
        let meta = ScriptMeta { inputs: vec!["a".into()], outputs: vec![spec("taken")] };
        let err = validate_meta(&meta, |n| n == "taken").unwrap_err();
        assert!(err.contains("collides"));
    }
}
