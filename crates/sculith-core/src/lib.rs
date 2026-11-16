//! Edition-independent server state. Bedrock semantics will be authoritative.

use serde::{Deserialize, Serialize};

/// Edition-independent simple choice form. No executable commands or asset IDs.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SimpleForm {
    pub title: String,
    pub body: String,
    pub buttons: Vec<String>,
}

impl SimpleForm {
    pub fn validate(&self) -> Result<(), String> {
        let label = |text: &str| {
            !text.trim().is_empty() && text.len() <= 128 && !text.chars().any(char::is_control)
        };
        if !label(&self.title)
            || self.body.len() > 2048
            || self.body.chars().any(|c| c.is_control() && c != '\n')
            || self.buttons.is_empty()
            || self.buttons.len() > 8
            || !self.buttons.iter().all(|text| label(text))
        {
            return Err("form requires a 1..128-byte title, body up to 2048 bytes and 1..8 bounded button labels".into());
        }
        Ok(())
    }
}

/// Canonical custom sound: a bounded Java-style resource identifier.
pub fn validate_sound_id(id: &str) -> Result<(), String> {
    let valid = || {
        let (namespace, path) = id.split_once(':')?;
        if id.len() > 128
            || namespace.is_empty()
            || namespace == "."
            || namespace == ".."
            || !namespace
                .bytes()
                .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b"_.-".contains(&b))
            || !path
                .bytes()
                .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b"_/.-".contains(&b))
            || path
                .split('/')
                .any(|p| p.is_empty() || p == "." || p == "..")
        {
            return None;
        }
        Some(())
    };
    valid().ok_or_else(|| {
        "sound ID requires namespace:path, at most 128 bytes without traversal".into()
    })
}

pub fn validate_sound_parameters(volume: f32, pitch: f32) -> Result<(), String> {
    if !volume.is_finite()
        || !(0.0..=1.0).contains(&volume)
        || !pitch.is_finite()
        || !(0.5..=2.0).contains(&pitch)
    {
        return Err("sound requires finite volume 0..1 and pitch 0.5..2".into());
    }
    Ok(())
}

/// Default configuration stays discovery-only; edition previews are explicit.
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

/// Bounded plain system text, serialized as text rather than executable JSON.
pub fn validate_system_message(message: &str) -> Result<(), String> {
    if message.trim().is_empty() || message.len() > 512 || message.chars().any(char::is_control) {
        return Err(
            "system message must contain 1..=512 UTF-8 bytes without control characters".into(),
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sound_identifiers_and_parameters_are_bounded() {
        validate_sound_id("sculith:demo/tone").unwrap();
        for id in ["bad", ":x", "..:x", ".:x", "a:../x", "a:x//y", "a:X", "a:"] {
            assert!(validate_sound_id(id).is_err(), "{id}");
        }
        assert!(validate_sound_id(&format!("a:{}", "x".repeat(128))).is_err());
        validate_sound_parameters(0.7, 1.0).unwrap();
        for (volume, pitch) in [
            (f32::NAN, 1.0),
            (1.0, f32::INFINITY),
            (-0.1, 1.0),
            (1.1, 1.0),
            (0.5, 0.49),
            (0.5, 2.01),
        ] {
            assert!(validate_sound_parameters(volume, pitch).is_err());
        }
    }

    #[test]
    fn shared_motd_rejects_bedrock_field_injection_and_preserves_old_value() {
        let mut status = ServerStatus::new("안녕하세요".into(), 20).unwrap();
        for invalid in ["", " ", "name;999", "name\nline", "name\0"] {
            assert!(status.set_motd(invalid.into()).is_err());
            assert_eq!(status.motd, "안녕하세요");
        }
        assert!(validate_motd(&"가".repeat(65)).is_err());
    }

    #[test]
    fn system_text_is_bounded_utf8_without_control_characters() {
        assert!(validate_system_message("환영합니다 👋").is_ok());
        for invalid in ["", " ", "injected\nline", "bad\0text"] {
            assert!(validate_system_message(invalid).is_err());
        }
        assert!(validate_system_message(&"가".repeat(171)).is_err());
    }

    #[test]
    fn simple_forms_bound_multiline_text_labels_and_choice_count() {
        let mut form = SimpleForm {
            title: "환영합니다 👋".into(),
            body: "첫 줄\n다음 줄".into(),
            buttons: vec!["선택".into()],
        };
        form.validate().unwrap();
        form.buttons = vec!["x".into(); 9];
        assert!(form.validate().is_err());
        form.buttons.clear();
        assert!(form.validate().is_err());
        form.buttons = vec!["\n".into()];
        assert!(form.validate().is_err());
        form.buttons = vec!["확인".into()];
        form.body = "bad\0text".into();
        assert!(form.validate().is_err());
        form.body = "가".repeat(683);
        assert!(form.validate().is_err());
    }
}
