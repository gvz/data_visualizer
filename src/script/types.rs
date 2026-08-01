use crate::types::SampleType;

/// Parse an output channel's declared type. Feature-independent so the engine
/// (not behind `scripting`) can resolve instance output overrides. Text outputs
/// are rejected — scripts publish numeric channels only.
pub fn parse_sample_type(ty: &str) -> Result<crate::types::SampleType, String> {
    use crate::types::SampleType;
    match ty {
        "float" => Ok(SampleType::Float),
        "int" => Ok(SampleType::Int),
        "bool" => Ok(SampleType::Bool),
        other => Err(format!("output type '{other}' is not one of float/int/bool")),
    }
}

/// Expand `{inN}` / `{inN.stem}` placeholders in an output-name template against
/// an instance's resolved input channel names. `{inN}` yields the Nth input's
/// full name; `{inN.stem}` its last `/`-separated segment. Any other `{...}`
/// form, or an out-of-range index, is an error.
pub fn expand_output_name(template: &str, inputs: &[String]) -> Result<String, String> {
    let mut out = String::with_capacity(template.len());
    let mut rest = template;
    while let Some(open) = rest.find('{') {
        out.push_str(&rest[..open]);
        let close = rest[open..]
            .find('}')
            .ok_or_else(|| format!("unterminated placeholder in '{template}'"))?
            + open;
        let token = &rest[open + 1..close]; // between the braces
        out.push_str(&expand_token(token, inputs)?);
        rest = &rest[close + 1..];
    }
    out.push_str(rest);
    Ok(out)
}

fn expand_token(token: &str, inputs: &[String]) -> Result<String, String> {
    let (idx_part, want_stem) = match token.strip_suffix(".stem") {
        Some(prefix) => (prefix, true),
        None => (token, false),
    };
    let idx_str = idx_part
        .strip_prefix("in")
        .ok_or_else(|| format!("unknown placeholder '{{{token}}}'"))?;
    let idx: usize = idx_str
        .parse()
        .map_err(|_| format!("unknown placeholder '{{{token}}}'"))?;
    let name = inputs
        .get(idx)
        .ok_or_else(|| format!("placeholder '{{{token}}}' has no input {idx}"))?;
    Ok(if want_stem {
        name.rsplit('/').next().unwrap_or(name).to_string()
    } else {
        name.clone()
    })
}

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

    /// Read a script's declared `INPUTS`/`OUTPUTS` without compiling `compute`.
    /// Output names are returned as their raw templates. Used by the GUI editor
    /// to prefill an instance's fields when a script is chosen.
    fn peek_meta(&self, source: &str, name: &str) -> Result<ScriptMeta, String>;
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

    #[test]
    fn parse_sample_type_accepts_numeric_and_rejects_text() {
        use crate::types::SampleType;
        assert_eq!(parse_sample_type("float").unwrap(), SampleType::Float);
        assert_eq!(parse_sample_type("int").unwrap(), SampleType::Int);
        assert_eq!(parse_sample_type("bool").unwrap(), SampleType::Bool);
        assert!(parse_sample_type("text").unwrap_err().contains("float/int/bool"));
        assert!(parse_sample_type("nope").is_err());
    }

    #[test]
    fn expand_output_name_literal_passthrough() {
        let inputs = vec!["load/ch0".to_string()];
        assert_eq!(expand_output_name("scripts.ch0_rms", &inputs).unwrap(), "scripts.ch0_rms");
    }

    #[test]
    fn expand_output_name_full_and_stem() {
        let inputs = vec!["load/ch0".to_string()];
        assert_eq!(expand_output_name("{in0}", &inputs).unwrap(), "load/ch0");
        assert_eq!(expand_output_name("{in0.stem}.rms", &inputs).unwrap(), "ch0.rms");
    }

    #[test]
    fn expand_output_name_multi_input_indices() {
        let inputs = vec!["a/x".to_string(), "b/y".to_string()];
        assert_eq!(expand_output_name("{in1.stem}-{in0.stem}", &inputs).unwrap(), "y-x");
    }

    #[test]
    fn expand_output_name_unknown_placeholder_errors() {
        let inputs = vec!["a".to_string()];
        assert!(expand_output_name("{in5}", &inputs).is_err());       // index out of range
        assert!(expand_output_name("{bogus}", &inputs).is_err());     // unrecognized form
        assert!(expand_output_name("{in0.foo}", &inputs).is_err());   // unknown modifier
    }

    #[test]
    fn expand_output_name_stem_of_unslashed_is_whole() {
        let inputs = vec!["plain".to_string()];
        assert_eq!(expand_output_name("{in0.stem}", &inputs).unwrap(), "plain");
    }
}
