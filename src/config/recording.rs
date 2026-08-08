use anyhow::Context;
use serde::Deserialize;

/// The `[recording]` section of config.toml. Controls size-based auto-split of
/// live recordings. Absent section / absent key / `0` all mean "single file".
#[derive(Debug, Clone, PartialEq, Default)]
pub struct RecordingConfig {
    pub max_file_mb: Option<u64>,
}

#[derive(Deserialize)]
struct DocWrapper {
    recording: Option<RawRecording>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawRecording {
    max_file_mb: Option<u64>,
}

impl RecordingConfig {
    /// Parse the `[recording]` table out of a full config.toml. An absent
    /// section or absent key yields `max_file_mb: None`.
    pub fn from_toml_str(s: &str) -> anyhow::Result<Self> {
        let doc: DocWrapper = toml::from_str(s).context("parsing [recording]")?;
        Ok(match doc.recording {
            None => RecordingConfig::default(),
            Some(raw) => RecordingConfig { max_file_mb: raw.max_file_mb },
        })
    }

    /// Size limit in bytes, or `None` for no split. `Some(0)` is treated as
    /// `None` so a user can disable splitting without deleting the key.
    pub fn max_bytes(&self) -> Option<u64> {
        match self.max_file_mb {
            Some(mb) if mb > 0 => Some(mb * 1024 * 1024),
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn absent_section_means_no_split() {
        let c = RecordingConfig::from_toml_str("[defaults]\nmax_rate = 100\n").unwrap();
        assert_eq!(c.max_file_mb, None);
        assert_eq!(c.max_bytes(), None);
    }

    #[test]
    fn reads_max_file_mb() {
        let c = RecordingConfig::from_toml_str("[recording]\nmax_file_mb = 512\n").unwrap();
        assert_eq!(c.max_file_mb, Some(512));
        assert_eq!(c.max_bytes(), Some(512 * 1024 * 1024));
    }

    #[test]
    fn zero_means_no_split() {
        let c = RecordingConfig::from_toml_str("[recording]\nmax_file_mb = 0\n").unwrap();
        assert_eq!(c.max_bytes(), None);
    }
}
