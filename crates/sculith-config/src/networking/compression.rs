use serde::{Deserialize, Serialize};

/// Configuration for packet compression.
///
/// Controls whether network packet compression is enabled and the compression parameters.
#[derive(Deserialize, Serialize, Clone)]
#[serde(default)]
pub struct CompressionConfig {
    /// Whether compression is enabled.
    pub enabled: bool,
    /// Detailed compression settings.
    #[serde(flatten)]
    pub info: CompressionInfo,
}

impl CompressionConfig {
    /// Bedrock advertises an unsigned 16-bit threshold; zero disables compression.
    #[must_use]
    pub fn bedrock_settings(&self) -> CompressionInfo {
        CompressionInfo {
            threshold: if self.enabled {
                self.info.threshold.min(u32::from(u16::MAX))
            } else {
                0
            },
            level: self.info.level,
        }
    }
}

impl Default for CompressionConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            info: CompressionInfo::default(),
        }
    }
}

/// Detailed information for packet compression settings.
///
/// Can also be used independently of the config.
#[derive(Deserialize, Serialize, Clone)]
#[serde(default)]
pub struct CompressionInfo {
    /// The compression threshold in bytes.
    /// Packets smaller than this will not be compressed.
    pub threshold: u32,
    /// Compression level, between `0..9`.
    /// `1` = optimize for speed, `9` = optimize for size.
    pub level: u32,
}

impl Default for CompressionInfo {
    fn default() -> Self {
        Self {
            threshold: 256,
            level: 4,
        }
    }
}

#[cfg(test)]
mod bedrock_tests {
    use super::*;
    #[test]
    fn wire_threshold_and_encoder_setting_agree() {
        let mut config = CompressionConfig::default();
        config.info.threshold = 65_536;
        assert_eq!(config.bedrock_settings().threshold, 65_535);
        config.enabled = false;
        assert_eq!(config.bedrock_settings().threshold, 0);
    }
}
