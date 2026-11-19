// Action form payload verified against Bedrock Dedicated Server 1.26.63.1 (protocol 2225).

use crate::{codec::var_uint::VarUInt, serial::PacketWrite};
use pumpkin_macros::packet;

/// Requests the client to open a server-defined UI form (modal dialog, list menu, or custom form).
#[packet(pumpkin_data::bedrock_packet::MODAL_FORM_REQUEST)]
#[derive(PacketWrite)]
pub struct CModalFormRequest {
    /// Unique identifier assigned by the server to distinguish responses when the form is submitted.
    pub form_id: VarUInt,
    /// JSON-encoded definition describing the layout, controls, and contents of the form window.
    pub form_ui_json: String,
}

impl CModalFormRequest {
    /// The plugin API calls the action list `buttons`. Bedrock 1.26.63 sends it as typed `elements`.
    pub fn encode_plugin_form(mut form: serde_json::Value) -> String {
        use serde_json::Value;
        if let Some(fields) = form.as_object_mut()
            && fields.get("type").and_then(Value::as_str) == Some("form")
            && !fields.contains_key("elements")
            && let Some(Value::Array(mut buttons)) = fields.remove("buttons")
        {
            for button in &mut buttons {
                if let Some(button) = button.as_object_mut() {
                    button.insert("type".into(), Value::from("button"));
                    button.entry("image").or_insert(Value::Null);
                }
            }
            fields.insert("elements".into(), Value::Array(buttons));
        }
        form.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::CModalFormRequest;
    use serde_json::{Value, json};

    #[test]
    fn action_buttons_match_the_dedicated_server_payload() {
        let legacy = json!({"type":"form", "title":"길드 의뢰", "content":"산등성이 조사",
            "buttons":[{"text":"의뢰 수락"}, {"text":"내 의뢰"}, {"text":"닫기"}]});
        // Captured from ActionFormData on Mojang's 1.26.63.1 server.
        let native = json!({"content":"산등성이 조사","elements":[
            {"image":null,"text":"의뢰 수락","type":"button"},
            {"image":null,"text":"내 의뢰","type":"button"},
            {"image":null,"text":"닫기","type":"button"}],"title":"길드 의뢰","type":"form"});
        assert_eq!(
            serde_json::from_str::<Value>(&CModalFormRequest::encode_plugin_form(legacy)).ok(),
            Some(native.clone())
        );
        assert_eq!(
            CModalFormRequest::encode_plugin_form(native.clone()),
            native.to_string()
        );
    }

    #[test]
    fn preserves_native_images_and_other_form_types() {
        let image = json!({"type":"path","data":"textures/items/map"});
        let form = json!({"type":"form","buttons":[{"text":"map","image":image}]});
        let expected =
            json!({"type":"form","elements":[{"type":"button","text":"map","image":image}]});
        assert_eq!(
            CModalFormRequest::encode_plugin_form(form),
            expected.to_string()
        );
        for form in [
            json!({"type":"modal","button1":"yes","button2":"no"}),
            json!({"type":"custom_form","content":[]}),
        ] {
            assert_eq!(
                CModalFormRequest::encode_plugin_form(form.clone()),
                form.to_string()
            );
        }
    }
}
