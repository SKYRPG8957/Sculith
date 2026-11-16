use serde::Deserialize;
use std::path::PathBuf;

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PluginConfig {
    pub path: PathBuf,
    #[serde(default)]
    pub args: Vec<String>,
    #[serde(default = "default_timeout")]
    pub timeout_ms: u64,
}

fn default_timeout() -> u64 {
    1000
}

impl PluginConfig {
    pub fn validate(&self) -> anyhow::Result<()> {
        anyhow::ensure!(
            (10..=10_000).contains(&self.timeout_ms),
            "plugin timeout_ms must be between 10 and 10000"
        );
        Ok(())
    }
}
