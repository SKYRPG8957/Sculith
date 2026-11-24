//! Shared forms projected to Bedrock forms and vanilla Java dialogs.

use std::{
    io::Cursor,
    sync::Arc,
    time::{Duration, Instant},
};

use pumpkin_nbt::{NbtCompound, deserializer::NbtReadHelperJava, tag::NbtTag};
use sculith_protocol::{
    ConnectionState, IdOr,
    bedrock::client::modal_form_request::CModalFormRequest,
    codec::var_uint::VarUInt,
    java::client::{dialog::DialogNBT, play::CPlayShowDialog},
};
use sculith_util::{
    text::{TextComponent, hover::HoverEvent},
    version::JavaMinecraftVersion,
};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use uuid::Uuid;

use crate::{
    net::ClientPlatform,
    plugin::api::events::player::bedrock_form_response::BedrockFormResponseEvent, server::Server,
};

use super::Player;

pub(crate) const SHARED_FORM_BIT: u32 = 1 << 31;
const ACTION_PREFIX: &str = "sculith:ui/";
const FORM_LIFETIME: Duration = Duration::from_secs(300);
const MAX_FORM_BYTES: usize = 32_768;
const MAX_ELEMENTS: usize = 64;
const MAX_TEXT_LENGTH: usize = 4096;
const MAX_INPUT_LENGTH: usize = 1024;

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(tag = "type", deny_unknown_fields)]
pub enum Form {
    #[serde(rename = "form")]
    Simple {
        title: String,
        content: String,
        buttons: Vec<Button>,
    },
    #[serde(rename = "modal")]
    Modal {
        title: String,
        content: String,
        button1: String,
        button2: String,
    },
    #[serde(rename = "custom_form")]
    Custom {
        title: String,
        content: Vec<Element>,
    },
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Button {
    pub text: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub image: Option<Value>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(tag = "type", deny_unknown_fields)]
pub enum Element {
    #[serde(rename = "label")]
    Label { text: String },
    #[serde(rename = "toggle")]
    Toggle { text: String, default: bool },
    #[serde(rename = "slider")]
    Slider {
        text: String,
        min: f32,
        max: f32,
        step: f32,
        default: f32,
    },
    #[serde(rename = "step_slider")]
    StepSlider {
        text: String,
        steps: Vec<String>,
        default: u32,
    },
    #[serde(rename = "dropdown")]
    Dropdown {
        text: String,
        options: Vec<String>,
        default: u32,
    },
    #[serde(rename = "input")]
    Input {
        text: String,
        placeholder: String,
        default: String,
    },
}

fn invalid(message: &str) -> String {
    message.to_owned()
}

fn check_text(text: &str) -> Result<(), String> {
    if text.len() > MAX_TEXT_LENGTH {
        return Err(invalid("Form text is too long"));
    }
    Ok(())
}

impl Form {
    pub fn from_json(value: Value) -> Result<Self, String> {
        let form: Self = serde_json::from_value(value).map_err(|error| error.to_string())?;
        form.validate()?;
        Ok(form)
    }

    fn validate(&self) -> Result<(), String> {
        let title = match self {
            Self::Simple { title, .. } | Self::Modal { title, .. } | Self::Custom { title, .. } => {
                title
            }
        };
        check_text(title)?;
        match self {
            Self::Simple {
                content, buttons, ..
            } => {
                check_text(content)?;
                if buttons.is_empty() || buttons.len() > MAX_ELEMENTS {
                    return Err(invalid("Forms need between 1 and 64 buttons"));
                }
                for button in buttons {
                    check_text(&button.text)?;
                    if button.image.is_some() {
                        return Err(invalid("Button images are not supported by shared forms"));
                    }
                }
            }
            Self::Modal {
                content,
                button1,
                button2,
                ..
            } => {
                for text in [content, button1, button2] {
                    check_text(text)?;
                }
            }
            Self::Custom { content, .. } => {
                if content.len() > MAX_ELEMENTS {
                    return Err(invalid("Forms support at most 64 elements"));
                }
                for element in content {
                    element.validate()?;
                }
            }
        }
        let json = serde_json::to_vec(self).map_err(|error| error.to_string())?;
        if json.len() > MAX_FORM_BYTES {
            return Err(invalid("Form exceeds 32 KiB"));
        }
        Ok(())
    }

    fn validate_response(&self, response: &Value) -> bool {
        match (self, response) {
            (Self::Simple { buttons, .. }, Value::Number(number)) => number
                .as_u64()
                .is_some_and(|index| index < buttons.len() as u64),
            (Self::Modal { .. }, Value::Bool(_)) => true,
            (Self::Custom { content, .. }, Value::Array(values)) => {
                content.len() == values.len()
                    && content
                        .iter()
                        .zip(values)
                        .all(|(element, value)| element.valid_response(value))
            }
            _ => false,
        }
    }

    fn java_response(&self, action: &str, payload: Option<&[u8]>) -> Result<Option<String>, ()> {
        if action == "cancel" && payload.is_none() {
            return Ok(None);
        }
        let value = match self {
            Self::Simple { .. } if payload.is_none() => {
                Value::from(action.parse::<u32>().map_err(|_| ())?)
            }
            Self::Modal { .. } if payload.is_none() => match action {
                "0" => Value::Bool(true),
                "1" => Value::Bool(false),
                _ => return Err(()),
            },
            Self::Custom { content, .. } if action == "submit" => {
                let payload = payload.ok_or(())?;
                if payload.len() > MAX_FORM_BYTES {
                    return Err(());
                }
                // The packet decoder has already checked tag depth, lengths and exact boundaries.
                let mut reader = NbtReadHelperJava::new(Cursor::new(payload));
                let NbtTag::Compound(compound) =
                    NbtTag::deserialize(&mut reader).map_err(|_| ())?
                else {
                    return Err(());
                };
                if compound.child_tags.len()
                    != content
                        .iter()
                        .filter(|element| !matches!(element, Element::Label { .. }))
                        .count()
                {
                    return Err(());
                }
                let values = content
                    .iter()
                    .enumerate()
                    .map(|(index, element)| element.java_value(compound.get(&format!("f{index}"))))
                    .collect::<Result<Vec<_>, _>>()?;
                Value::Array(values)
            }
            _ => return Err(()),
        };
        if !self.validate_response(&value) {
            return Err(());
        }
        Ok(Some(value.to_string()))
    }

    fn java_dialog(&self, prefix: &str) -> NbtCompound {
        let (title, body, buttons) = match self {
            Self::Simple {
                title,
                content,
                buttons,
            } => (
                title,
                vec![plain_message(content)],
                buttons
                    .iter()
                    .enumerate()
                    .map(|(index, button)| {
                        action_button(&button.text, &format!("{prefix}/{index}"), false)
                    })
                    .collect(),
            ),
            Self::Modal {
                title,
                content,
                button1,
                button2,
            } => (
                title,
                vec![plain_message(content)],
                vec![
                    action_button(button1, &format!("{prefix}/0"), false),
                    action_button(button2, &format!("{prefix}/1"), false),
                ],
            ),
            Self::Custom { title, content } => (
                title,
                content
                    .iter()
                    .filter_map(|element| match element {
                        Element::Label { text } => Some(plain_message(text)),
                        _ => None,
                    })
                    .collect(),
                vec![NbtTag::Compound(action_button_translated(
                    "gui.done",
                    &format!("{prefix}/submit"),
                    true,
                ))],
            ),
        };
        let mut dialog = NbtCompound::new();
        dialog.put_string("type", "minecraft:multi_action".into());
        dialog.put("title", text_tag(title));
        dialog.put_bool("pause", false);
        dialog.put_bool("can_close_with_escape", true);
        dialog.put_string("after_action", "close".into());
        dialog.put_int(
            "columns",
            if matches!(self, Self::Modal { .. }) {
                2
            } else {
                1
            },
        );
        dialog.put("body", NbtTag::List(body));
        dialog.put("actions", NbtTag::List(buttons));
        dialog.put_compound(
            "exit_action",
            action_button_translated("gui.cancel", &format!("{prefix}/cancel"), false),
        );
        if let Self::Custom { content, .. } = self {
            let inputs = content
                .iter()
                .enumerate()
                .filter_map(|(index, element)| element.java_input(index))
                .map(NbtTag::Compound)
                .collect();
            dialog.put("inputs", NbtTag::List(inputs));
        }
        dialog
    }
}

impl Element {
    fn validate(&self) -> Result<(), String> {
        let text = match self {
            Self::Label { text }
            | Self::Toggle { text, .. }
            | Self::Slider { text, .. }
            | Self::StepSlider { text, .. }
            | Self::Dropdown { text, .. }
            | Self::Input { text, .. } => text,
        };
        check_text(text)?;
        match self {
            Self::Slider {
                min,
                max,
                step,
                default,
                ..
            } if ![min, max, step, default]
                .iter()
                .all(|value| value.is_finite())
                || min >= max
                || *step <= 0.0
                || default < min
                || default > max =>
            {
                return Err(invalid("Invalid slider range or default"));
            }
            Self::StepSlider {
                steps: options,
                default,
                ..
            }
            | Self::Dropdown {
                options, default, ..
            } => {
                if options.is_empty()
                    || options.len() > MAX_ELEMENTS
                    || *default as usize >= options.len()
                {
                    return Err(invalid("Invalid form options or default"));
                }
                for option in options {
                    check_text(option)?;
                }
            }
            Self::Input {
                placeholder,
                default,
                ..
            } => {
                check_text(placeholder)?;
                if default.encode_utf16().count() > MAX_INPUT_LENGTH {
                    return Err(invalid("Input default exceeds 1024 characters"));
                }
            }
            _ => {}
        }
        Ok(())
    }

    fn valid_response(&self, value: &Value) -> bool {
        match self {
            Self::Label { .. } => value.is_null(),
            Self::Toggle { .. } => value.is_boolean(),
            Self::Slider { min, max, .. } => value.as_f64().is_some_and(|value| {
                value.is_finite() && value >= f64::from(*min) && value <= f64::from(*max)
            }),
            Self::StepSlider { steps: options, .. } | Self::Dropdown { options, .. } => value
                .as_u64()
                .is_some_and(|value| value < options.len() as u64),
            Self::Input { .. } => value
                .as_str()
                .is_some_and(|value| value.encode_utf16().count() <= MAX_INPUT_LENGTH),
        }
    }

    fn java_value(&self, value: Option<&NbtTag>) -> Result<Value, ()> {
        // InputControlHandlers overrides ValueGetter: checkboxes send bytes,
        // range controls send floats, while text and option IDs send strings.
        match (self, value) {
            (Self::Label { .. }, _) => Ok(Value::Null),
            (Self::Toggle { .. }, Some(NbtTag::Byte(value @ (0 | 1)))) => {
                Ok(Value::Bool(*value == 1))
            }
            (Self::Slider { .. }, Some(NbtTag::Float(value))) => {
                serde_json::Number::from_f64(f64::from(*value))
                    .map(Value::Number)
                    .ok_or(())
            }
            (Self::StepSlider { .. } | Self::Dropdown { .. }, Some(NbtTag::String(value))) => {
                value.parse::<u32>().map(Value::from).map_err(|_| ())
            }
            (Self::Input { .. }, Some(NbtTag::String(value))) => {
                Ok(Value::String(value.to_string()))
            }
            _ => Err(()),
        }
    }

    fn java_input(&self, index: usize) -> Option<NbtCompound> {
        let mut input = NbtCompound::new();
        input.put_string("key", format!("f{index}"));
        let (kind, text) = match self {
            Self::Label { .. } => return None,
            Self::Toggle { text, default } => {
                input.put_bool("initial", *default);
                ("boolean", text)
            }
            Self::Slider {
                text,
                min,
                max,
                step,
                default,
            } => {
                input.put_float("start", *min);
                input.put_float("end", *max);
                input.put_float("step", *step);
                input.put_float("initial", *default);
                ("number_range", text)
            }
            Self::StepSlider {
                text,
                steps: options,
                default,
            }
            | Self::Dropdown {
                text,
                options,
                default,
            } => {
                input.put(
                    "options",
                    NbtTag::List(
                        options
                            .iter()
                            .enumerate()
                            .map(|(index, text)| {
                                let mut option = NbtCompound::new();
                                option.put_string("id", index.to_string());
                                option.put("display", text_tag(text));
                                option.put_bool("initial", index == *default as usize);
                                NbtTag::Compound(option)
                            })
                            .collect(),
                    ),
                );
                ("single_option", text)
            }
            Self::Input {
                text,
                placeholder,
                default,
            } => {
                input.put_string("initial", default.clone());
                input.put_int("max_length", MAX_INPUT_LENGTH as i32);
                if !placeholder.is_empty() {
                    input.put(
                        "label",
                        TextComponent::text(text.to_owned())
                            .hover_event(HoverEvent::show_text(TextComponent::text(
                                placeholder.to_owned(),
                            )))
                            .to_nbt_tag_for_version(&JavaMinecraftVersion::V_26_3),
                    );
                }
                ("text", text)
            }
        };
        input.put_string("type", format!("minecraft:{kind}"));
        if input.get("label").is_none() {
            input.put("label", text_tag(text));
        }
        Some(input)
    }
}

fn text_tag(text: &str) -> NbtTag {
    TextComponent::text(text.to_owned()).to_nbt_tag_for_version(&JavaMinecraftVersion::V_26_3)
}

fn plain_message(text: &str) -> NbtTag {
    let mut message = NbtCompound::new();
    message.put_string("type", "minecraft:plain_message".into());
    message.put("contents", text_tag(text));
    NbtTag::Compound(message)
}

fn action_button(text: &str, id: &str, dynamic: bool) -> NbtTag {
    let mut button = action_button_translated("", id, dynamic);
    button.put("label", text_tag(text));
    NbtTag::Compound(button)
}

fn action_button_translated(key: &str, id: &str, dynamic: bool) -> NbtCompound {
    let mut action = NbtCompound::new();
    action.put_string(
        "type",
        if dynamic {
            "minecraft:dynamic/custom"
        } else {
            "minecraft:custom"
        }
        .into(),
    );
    action.put_string("id", id.to_owned());
    let mut button = NbtCompound::new();
    button.put(
        "label",
        TextComponent::translate(key.to_owned(), vec![])
            .to_nbt_tag_for_version(&JavaMinecraftVersion::V_26_3),
    );
    button.put_compound("action", action);
    button
}

struct PendingForm {
    id: u32,
    prefix: String,
    expires: Instant,
    form: Form,
}

pub(super) struct FormSession {
    next_id: u32,
    pending: Option<PendingForm>,
}

impl Default for FormSession {
    fn default() -> Self {
        Self {
            next_id: SHARED_FORM_BIT,
            pending: None,
        }
    }
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) enum FormReply {
    Unhandled,
    Invalid,
    Valid { id: u32, data: Option<String> },
}

impl FormSession {
    fn open(
        &mut self,
        form: Form,
        java: bool,
        now: Instant,
    ) -> Result<(u32, Option<NbtCompound>), String> {
        if self
            .pending
            .as_ref()
            .is_some_and(|pending| pending.expires > now)
        {
            return Err(invalid("A shared form is already open"));
        }
        let id = self.next_id;
        self.next_id = id
            .checked_add(1)
            .ok_or_else(|| invalid("Form IDs exhausted for this connection"))?;
        let prefix = format!("{ACTION_PREFIX}{id}/{:032x}", Uuid::new_v4().as_u128());
        let dialog = java.then(|| form.java_dialog(&prefix));
        self.pending = Some(PendingForm {
            id,
            prefix,
            expires: now + FORM_LIFETIME,
            form,
        });
        Ok((id, dialog))
    }

    fn abort(&mut self, id: u32) {
        if self
            .pending
            .as_ref()
            .is_some_and(|pending| pending.id == id)
        {
            self.pending = None;
        }
    }

    fn java_response(&mut self, action: &str, payload: Option<&[u8]>, now: Instant) -> FormReply {
        if !action.starts_with(ACTION_PREFIX) {
            return FormReply::Unhandled;
        }
        let Some(form) = self.pending.as_ref() else {
            return FormReply::Invalid;
        };
        let Some(action) = action
            .strip_prefix(&form.prefix)
            .and_then(|action| action.strip_prefix('/'))
        else {
            return FormReply::Invalid;
        };
        if form.expires <= now {
            self.pending = None;
            return FormReply::Invalid;
        }
        let Ok(data) = form.form.java_response(action, payload) else {
            return FormReply::Invalid;
        };
        let reply = FormReply::Valid { id: form.id, data };
        self.pending = None;
        reply
    }

    fn bedrock_response(&mut self, id: u32, data: Option<&str>, now: Instant) -> FormReply {
        if id & SHARED_FORM_BIT == 0 {
            return FormReply::Unhandled;
        }
        let Some(form) = self.pending.as_ref().filter(|form| form.id == id) else {
            return FormReply::Invalid;
        };
        if form.expires <= now {
            self.pending = None;
            return FormReply::Invalid;
        }
        let value = match data {
            Some(data) if data.len() <= MAX_FORM_BYTES => match serde_json::from_str::<Value>(data)
            {
                Ok(value) => value,
                Err(_) => return FormReply::Invalid,
            },
            None => Value::Null,
            _ => return FormReply::Invalid,
        };
        if !value.is_null() && !form.form.validate_response(&value) {
            return FormReply::Invalid;
        }
        let reply = FormReply::Valid {
            id,
            data: (!value.is_null()).then(|| value.to_string()),
        };
        self.pending = None;
        reply
    }
}

impl Player {
    /// Opens one shared form. Replies use `BedrockFormResponseEvent` on both editions.
    pub fn open_form(&self, form: Form) -> Result<u32, String> {
        form.validate()?;
        if let ClientPlatform::Java(client) = self.client.as_ref()
            && (client.version.load() < JavaMinecraftVersion::V_1_21_6
                || client.connection_state.load() != ConnectionState::Play)
        {
            return Err(invalid("Shared forms require Java 1.21.6 or later in play"));
        }
        let native_json = if matches!(self.client.as_ref(), ClientPlatform::Bedrock(_)) {
            let data = serde_json::to_value(&form).map_err(|error| error.to_string())?;
            let encoded = CModalFormRequest::encode_plugin_form(data);
            if encoded.len() > MAX_FORM_BYTES {
                return Err(invalid("Bedrock form payload is too large"));
            }
            encoded
        } else {
            String::new()
        };
        let (id, dialog) = self
            .forms
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .open(
                form,
                matches!(self.client.as_ref(), ClientPlatform::Java(_)),
                Instant::now(),
            )?;
        // Packet observers can call back into the plugin and open another form.
        // Keep the session installed, with its lock released, until delivery succeeds.
        let sent = match self.client.as_ref() {
            ClientPlatform::Java(client) => dialog.as_ref().is_some_and(|dialog| {
                client.try_send_packet(&CPlayShowDialog::new(IdOr::Value(DialogNBT::from_nbt(
                    dialog,
                ))))
            }),
            ClientPlatform::Bedrock(client) => client
                .serialize_packet(&CModalFormRequest {
                    form_id: VarUInt(id),
                    form_ui_json: native_json,
                })
                .is_ok_and(|bytes| client.try_enqueue_packet_data(bytes)),
        };
        if !sent {
            self.forms
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .abort(id);
            return Err(invalid("Form packet was not queued"));
        }
        Ok(id)
    }

    pub(crate) fn java_form_response(&self, action: &str, payload: Option<&[u8]>) -> FormReply {
        self.forms
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .java_response(action, payload, Instant::now())
    }

    pub(crate) fn bedrock_form_response(&self, id: u32, data: Option<&str>) -> FormReply {
        self.forms
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .bedrock_response(id, data, Instant::now())
    }

    pub(crate) fn fire_form_response(
        self: &Arc<Self>,
        server: &Arc<Server>,
        id: u32,
        data: Option<String>,
    ) {
        let mut event = BedrockFormResponseEvent::new(self.clone(), id, data);
        server.plugin_manager.fire_blocking(server, &mut event);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn simple() -> Form {
        Form::from_json(json!({"type":"form", "title":"Title", "content":"Choose", "buttons":[{"text":"First"},{"text":"Second"}]})).unwrap()
    }

    fn custom() -> Form {
        Form::from_json(json!({"type":"custom_form", "title":"Inputs", "content":[
            {"type":"label", "text":"Text"},
            {"type":"toggle", "text":"Toggle", "default":false},
            {"type":"slider", "text":"Range", "min":-1.0, "max":5.0, "step":0.5, "default":0.0},
            {"type":"dropdown", "text":"Select", "options":["A","B"], "default":0},
            {"type":"step_slider", "text":"Step", "steps":["One","Two"], "default":1},
            {"type":"input", "text":"Input", "placeholder":"Hint", "default":""}
        ]}))
        .unwrap()
    }

    #[test]
    fn normalizes_both_editions_to_the_same_custom_response() {
        let form = custom();
        let expected = json!([null, true, 2.5, 1, 0, "가나다😀"]);
        assert!(form.validate_response(&expected));
        let mut payload = NbtCompound::new();
        payload.put_byte("f1", 1);
        payload.put_float("f2", 2.5);
        for (key, value) in [("f3", "1"), ("f4", "0"), ("f5", "가나다😀")] {
            payload.put_string(key, value.into());
        }
        let bytes = pumpkin_nbt::Nbt::from(payload).write_unnamed();
        assert_eq!(
            form.java_response("submit", Some(&bytes)),
            Ok(Some(expected.to_string()))
        );
        for value in [
            json!([null, true, 7.0, 1, 0, "a"]),
            json!([null, "true", 2.5, 1, 0, "a"]),
            json!([null, true, 2.5, 2, 0, "a"]),
            json!([null, true]),
            json!([null, true, 2.5, 1, 0, "a".repeat(1025)]),
        ] {
            assert!(!form.validate_response(&value));
        }
        assert_eq!(form.java_response("submit", None), Err(()));
    }

    #[test]
    fn binds_java_replies_to_one_player_and_one_request() {
        let now = Instant::now();
        let mut session = FormSession::default();
        let (id, dialog) = session.open(simple(), true, now).unwrap();
        assert!(dialog.is_some());
        assert!(session.open(simple(), true, now).is_err());
        let action = format!("{}/0", session.pending.as_ref().unwrap().prefix);
        let mut other = FormSession::default();
        other.open(simple(), true, now).unwrap();
        assert_eq!(other.java_response(&action, None, now), FormReply::Invalid);
        assert_eq!(
            session.java_response("sculith:ui/forged/0", None, now),
            FormReply::Invalid
        );
        assert_eq!(
            session.java_response(&action.replace("/0", "/9"), None, now),
            FormReply::Invalid
        );
        assert_eq!(
            session.java_response(&action, None, now),
            FormReply::Valid {
                id,
                data: Some("0".into())
            }
        );
        assert_eq!(
            session.java_response(&action, None, now),
            FormReply::Invalid
        );
        assert_eq!(
            session.java_response("example:other_dialog", None, now),
            FormReply::Unhandled
        );
        let (new_id, _) = session.open(simple(), true, now).unwrap();
        assert_ne!(new_id, id);
        assert_eq!(
            session.java_response(&action, None, now),
            FormReply::Invalid
        );
    }

    #[test]
    fn rejects_native_replay_wrong_ids_and_invalid_choices() {
        let now = Instant::now();
        let mut session = FormSession::default();
        let (id, dialog) = session.open(simple(), false, now).unwrap();
        assert!(dialog.is_none());
        assert_eq!(
            session.bedrock_response(1, Some("0"), now),
            FormReply::Unhandled
        );
        for (reply_id, data) in [
            (id + 1, "0"),
            (id, "-1"),
            (id, "2"),
            (id, "true"),
            (id, "[0]"),
            (id, "{invalid"),
        ] {
            assert_eq!(
                session.bedrock_response(reply_id, Some(data), now),
                FormReply::Invalid
            );
        }
        assert_eq!(
            session.bedrock_response(id, Some(" 1\n"), now),
            FormReply::Valid {
                id,
                data: Some("1".into())
            }
        );
        assert_eq!(
            session.bedrock_response(id, Some("1"), now),
            FormReply::Invalid
        );
    }

    #[test]
    fn expires_and_closes_sessions_without_reusing_ids() {
        let now = Instant::now();
        let mut session = FormSession::default();
        let (id, _) = session.open(simple(), true, now).unwrap();
        let cancel = format!("{}/cancel", session.pending.as_ref().unwrap().prefix);
        assert_eq!(
            session.java_response(&cancel, None, now),
            FormReply::Valid { id, data: None }
        );
        let (next, _) = session.open(simple(), false, now).unwrap();
        session.abort(id);
        assert!(session.pending.is_some());
        assert_eq!(
            session.bedrock_response(next, None, now + FORM_LIFETIME),
            FormReply::Invalid
        );
        assert!(session.pending.is_none());
        let (last, _) = session.open(simple(), false, now + FORM_LIFETIME).unwrap();
        assert!(last > next);
        assert_eq!(
            session.bedrock_response(last, Some("null\n"), now + FORM_LIFETIME),
            FormReply::Valid {
                id: last,
                data: None
            }
        );
        session.next_id = u32::MAX;
        assert!(session.open(simple(), true, now).is_err());
    }

    #[test]
    fn modal_false_is_distinct_from_dismissal() {
        let form = Form::from_json(json!({"type":"modal", "title":"Question", "content":"Continue?", "button1":"Yes", "button2":"No"})).unwrap();
        assert_eq!(form.java_response("0", None), Ok(Some("true".into())));
        assert_eq!(form.java_response("1", None), Ok(Some("false".into())));
        assert_eq!(form.java_response("cancel", None), Ok(None));
    }

    #[test]
    fn rejects_unsupported_images_and_unbounded_definitions() {
        for definition in [
            json!({"type":"form","title":"","content":"","buttons":[]}),
            json!({"type":"form","title":"","content":"","buttons":[{"text":"Image","image":{"type":"url","data":"https://example.org/a.png"}}]}),
            json!({"type":"custom_form","title":"","content":[{"type":"slider","text":"","min":0.0,"max":0.0,"step":1.0,"default":0.0}]}),
            json!({"type":"custom_form","title":"","content":[{"type":"dropdown","text":"","options":["A"],"default":1}]}),
            json!({"type":"form","title":"x".repeat(MAX_TEXT_LENGTH+1),"content":"","buttons":[{"text":"A"}]}),
        ] {
            assert!(Form::from_json(definition).is_err());
        }
        let mut form = simple();
        if let Form::Simple { buttons, .. } = &mut form {
            *buttons = vec![
                Button {
                    text: "x".repeat(1024),
                    image: None
                };
                MAX_ELEMENTS
            ];
        }
        assert!(form.validate().is_err());
    }
}
