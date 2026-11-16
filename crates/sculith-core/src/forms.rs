//! Bounded input forms shared by native menus and stock Java dialogs.
use serde::{Deserialize, Serialize};

pub const MAX_FORM_FIELDS: usize = 8;
pub const MAX_FORM_RESPONSE_BYTES: usize = 16_384;
pub const MAX_FORM_TEXT_UNITS: u16 = 256;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CustomForm {
    pub title: String,
    pub fields: Vec<FormField>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum FormField {
    Text {
        label: String,
        default: String,
        /// UTF-16 code units, matching the vanilla Java input widget.
        max_length: u16,
    },
    Toggle {
        label: String,
        default: bool,
    },
    Dropdown {
        label: String,
        options: Vec<String>,
        default: u32,
    },
    Slider {
        label: String,
        min: i32,
        max: i32,
        step: u32,
        default: i32,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(
    tag = "type",
    content = "value",
    rename_all = "snake_case",
    deny_unknown_fields
)]
pub enum FormValue {
    Text(String),
    Toggle(bool),
    /// Dropdown option index or integer slider value, in field order.
    Number(i32),
}

impl FormValue {
    /// Input controls share exact integer numeric values across editions.
    pub fn number(value: f64) -> Result<Self, String> {
        if !value.is_finite()
            || value.fract() != 0.0
            || value < f64::from(i32::MIN)
            || value > f64::from(i32::MAX)
        {
            return Err("form number requires an exact bounded integer".into());
        }
        Ok(Self::Number(value as i32))
    }
}

fn label(value: &str) -> bool {
    !value.trim().is_empty() && value.len() <= 128 && !value.chars().any(char::is_control)
}

fn text(value: &str, limit: u16) -> bool {
    value.encode_utf16().count() <= usize::from(limit) && !value.chars().any(char::is_control)
}

fn slider(value: i32, min: i32, max: i32, step: u32) -> bool {
    value >= min
        && value <= max
        && step != 0
        && (i64::from(value) - i64::from(min)) % i64::from(step) == 0
}

impl FormField {
    pub fn label(&self) -> &str {
        match self {
            Self::Text { label, .. }
            | Self::Toggle { label, .. }
            | Self::Dropdown { label, .. }
            | Self::Slider { label, .. } => label,
        }
    }
}

impl CustomForm {
    pub fn validate(&self) -> Result<(), String> {
        if !label(&self.title) || self.fields.is_empty() || self.fields.len() > MAX_FORM_FIELDS {
            return Err("input form requires a bounded title and 1..8 fields".into());
        }
        for field in &self.fields {
            if !label(field.label()) {
                return Err("input labels require 1..128 bytes without control characters".into());
            }
            let valid = match field {
                FormField::Text {
                    default,
                    max_length,
                    ..
                } => (1..=MAX_FORM_TEXT_UNITS).contains(max_length) && text(default, *max_length),
                FormField::Toggle { .. } => true,
                FormField::Dropdown {
                    options, default, ..
                } => {
                    !options.is_empty()
                        && options.len() <= 8
                        && (*default as usize) < options.len()
                        && options.iter().all(|value| label(value))
                }
                FormField::Slider {
                    min,
                    max,
                    step,
                    default,
                    ..
                } => {
                    *min >= -32_768
                        && *max <= 32_767
                        && min < max
                        && *step != 0
                        && *step <= 65_535
                        && (i64::from(*max) - i64::from(*min)) % i64::from(*step) == 0
                        && slider(*default, *min, *max, *step)
                }
            };
            if !valid {
                return Err(
                    "invalid input default, text limit, options or integer slider range".into(),
                );
            }
        }
        Ok(())
    }

    /// Validate the complete submission before the pending request is consumed.
    pub fn validate_values(&self, values: &[FormValue]) -> Result<(), String> {
        if values.len() != self.fields.len() {
            return Err("input value count does not match the form".into());
        }
        for (field, value) in self.fields.iter().zip(values) {
            let valid = match (field, value) {
                (FormField::Text { max_length, .. }, FormValue::Text(value)) => {
                    text(value, *max_length)
                }
                (FormField::Toggle { .. }, FormValue::Toggle(_)) => true,
                (FormField::Dropdown { options, .. }, FormValue::Number(value)) => {
                    *value >= 0 && (*value as usize) < options.len()
                }
                (FormField::Slider { min, max, step, .. }, FormValue::Number(value)) => {
                    slider(*value, *min, *max, *step)
                }
                _ => false,
            };
            if !valid {
                return Err(
                    "input value has an invalid type, length, option or slider step".into(),
                );
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    pub fn fixture() -> CustomForm {
        CustomForm {
            title: "설정 👋".into(),
            fields: vec![
                FormField::Text {
                    label: "이름".into(),
                    default: "".into(),
                    max_length: 8,
                },
                FormField::Toggle {
                    label: "알림".into(),
                    default: true,
                },
                FormField::Dropdown {
                    label: "목록".into(),
                    options: vec!["가".into(), "나".into()],
                    default: 0,
                },
                FormField::Slider {
                    label: "크기".into(),
                    min: -10,
                    max: 10,
                    step: 5,
                    default: 0,
                },
            ],
        }
    }

    #[test]
    fn values_are_typed_complete_bounded_and_use_utf16_length() {
        let form = fixture();
        form.validate().unwrap();
        let values = vec![
            FormValue::Text("이름 👋".into()),
            FormValue::Toggle(false),
            FormValue::Number(1),
            FormValue::Number(5),
        ];
        form.validate_values(&values).unwrap();
        assert!(form.validate_values(&values[..3]).is_err());
        for (index, value) in [
            (0, FormValue::Text("👋".repeat(5))),
            (0, FormValue::Text("line\nbreak".into())),
            (1, FormValue::Text("true".into())),
            (2, FormValue::Number(-1)),
            (2, FormValue::Number(2)),
            (3, FormValue::Number(1)),
            (3, FormValue::Number(15)),
        ] {
            let mut invalid = values.clone();
            invalid[index] = value;
            assert!(form.validate_values(&invalid).is_err());
        }
    }

    #[test]
    fn definitions_reject_invalid_limits_defaults_and_ranges() {
        let mut form = fixture();
        form.fields.push(FormField::Text {
            label: "text".into(),
            default: "".into(),
            max_length: 0,
        });
        assert!(form.validate().is_err());
        for (min, max, step, default) in [
            (0, 10, 0, 0),
            (0, 10, 3, 0),
            (0, 10, 2, 1),
            (0, 10, 2, 12),
            (i32::MIN, i32::MAX, 1, 0),
        ] {
            form.fields = vec![FormField::Slider {
                label: "value".into(),
                min,
                max,
                step,
                default,
            }];
            assert!(form.validate().is_err());
        }
        form.fields = vec![FormField::Dropdown {
            label: "option".into(),
            options: vec![],
            default: 0,
        }];
        assert!(form.validate().is_err());
        form.fields = vec![FormField::Text {
            label: "text".into(),
            default: "👋".into(),
            max_length: 1,
        }];
        assert!(form.validate().is_err());
        form.fields = vec![FormField::Toggle {
            label: "\n".into(),
            default: false,
        }];
        assert!(form.validate().is_err());
    }
}
