//! Edition-independent server state. Bedrock semantics will be authoritative.

use serde::{Deserialize, Serialize};

/// Discovery is implemented; a game session cannot be created yet.
pub const DEVELOPMENT_STAGE: &str = "discovery-only";

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ServerStatus {
    pub motd: String,
    pub online_players: u32,
    pub max_players: u32,
    pub stage: String,
}

impl ServerStatus {
    pub fn new(motd: String, max_players: u32) -> Result<Self, String> {
        validate_motd(&motd)?;
        if !(1..=100_000).contains(&max_players) {
            return Err("max_players must be between 1 and 100000".into());
        }
        Ok(Self {
            motd,
            online_players: 0,
            max_players,
            stage: DEVELOPMENT_STAGE.into(),
        })
    }

    pub fn set_motd(&mut self, motd: String) -> Result<(), String> {
        validate_motd(&motd)?;
        self.motd = motd;
        Ok(())
    }
}

/// One MOTD must fit both editions' discovery formats, including RakNet delimiters.
pub fn validate_motd(motd: &str) -> Result<(), String> {
    if motd.trim().is_empty() || motd.len() > 192 {
        return Err("motd must contain 1..=192 UTF-8 bytes".into());
    }
    if motd.chars().any(|c| c.is_control() || c == ';') {
        return Err("motd cannot contain control characters or ';'".into());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shared_motd_rejects_bedrock_field_injection_and_preserves_old_value() {
        let mut status = ServerStatus::new("안녕하세요".into(), 20).unwrap();
        for invalid in ["", " ", "name;999", "name\nline", "name\0"] {
            assert!(status.set_motd(invalid.into()).is_err());
            assert_eq!(status.motd, "안녕하세요");
        }
        assert!(validate_motd(&"가".repeat(65)).is_err());
    }
}
