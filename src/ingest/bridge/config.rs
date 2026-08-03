use anyhow::Context;
use serde::Deserialize;

/// One `[[sources.bridge]]` entry: an external adapter datavis spawns.
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BridgeConfig {
    /// Human-facing name shown in the status bar and logs.
    pub name: String,
    /// Path or PATH-resolvable name of the org's proprietary executable.
    pub command: String,
    /// Arguments passed to the executable; defaults to empty.
    #[serde(default)]
    pub args: Vec<String>,
}

// `[sources]` may grow other keys later, so this wrapper does not deny unknown
// fields; and the top-level doc ignores every section except `sources`.
#[derive(Deserialize)]
struct DocWrapper {
    sources: Option<RawSources>,
}

#[derive(Deserialize)]
struct RawSources {
    #[serde(default)]
    bridge: Vec<BridgeConfig>,
}

impl BridgeConfig {
    /// Extract the `[[sources.bridge]]` array from a full config.toml. An absent
    /// section yields an empty list.
    pub fn list_from_toml_str(s: &str) -> anyhow::Result<Vec<BridgeConfig>> {
        let doc: DocWrapper = toml::from_str(s).context("parsing [[sources.bridge]]")?;
        Ok(doc.sources.map(|s| s.bridge).unwrap_or_default())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn absent_section_yields_empty() {
        let v = BridgeConfig::list_from_toml_str("default_window_s = 5.0\n").unwrap();
        assert!(v.is_empty());
    }

    #[test]
    fn parses_bridges_with_default_args() {
        let v = BridgeConfig::list_from_toml_str(
            "[[sources.bridge]]\nname = \"vendor-x\"\ncommand = \"/opt/x/adapter\"\n\
             args = [\"--device\", \"/dev/tty0\"]\n\n\
             [[sources.bridge]]\nname = \"vendor-y\"\ncommand = \"adapter-y\"\n",
        )
        .unwrap();
        assert_eq!(v.len(), 2);
        assert_eq!(v[0].name, "vendor-x");
        assert_eq!(v[0].command, "/opt/x/adapter");
        assert_eq!(v[0].args, vec!["--device", "/dev/tty0"]);
        assert_eq!(v[1].name, "vendor-y");
        assert!(v[1].args.is_empty()); // default
    }

    #[test]
    fn ignores_other_sections() {
        let v = BridgeConfig::list_from_toml_str(
            "[channels.\"a\"]\ntype = \"float\"\n\n\
             [[sources.bridge]]\nname = \"b\"\ncommand = \"c\"\n",
        )
        .unwrap();
        assert_eq!(v.len(), 1);
    }
}
