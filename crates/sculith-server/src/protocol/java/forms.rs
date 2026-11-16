//! Vanilla multi-action dialog: the simple-form fallback, not arbitrary JSON UI.
mod custom;
pub use custom::{response as custom_response, show as show_custom};

use super::codec::{self, Fields};
use anyhow::{Result, ensure};
use sculith_core::SimpleForm;

fn compound<const N: usize>(fields: [(&str, nbtx::Value); N]) -> nbtx::Compound {
    fields
        .into_iter()
        .map(|(name, value)| (name.as_bytes().to_vec().into(), value))
        .collect()
}

fn button(label: &str, action: &str) -> Result<nbtx::Compound> {
    use nbtx::Value as V;
    Ok(compound([
        ("label", codec::nbt_string(label)?),
        (
            "action",
            V::Compound(compound([
                ("type", codec::nbt_string("minecraft:custom")?),
                ("id", codec::nbt_string(action)?),
            ])),
        ),
    ]))
}

pub fn show(id: u32, form: &SimpleForm) -> Result<Vec<u8>> {
    use nbtx::{Value as V, ValueList as L};
    form.validate().map_err(anyhow::Error::msg)?;
    let actions = form
        .buttons
        .iter()
        .enumerate()
        .map(|(index, label)| button(label, &format!("sculith:form/{id}/{index}")))
        .collect::<Result<Vec<_>>>()?;
    let dialog = V::Compound(compound([
        ("type", codec::nbt_string("minecraft:multi_action")?),
        ("title", codec::nbt_string(&form.title)?),
        ("pause", V::Byte(0)),
        ("can_close_with_escape", V::Byte(1)),
        ("after_action", codec::nbt_string("close")?),
        ("columns", V::Int(1)),
        (
            "body",
            V::List(L::Compound(vec![compound([
                ("type", codec::nbt_string("minecraft:plain_message")?),
                ("contents", codec::nbt_string(&form.body)?),
                ("width", V::Int(300)),
            ])])),
        ),
        ("actions", V::List(L::Compound(actions))),
        (
            "exit_action",
            V::Compound(button("닫기", &format!("sculith:form/{id}/cancel"))?),
        ),
    ]));
    let mut packet = codec::packet(0x8f);
    packet.push(0); // Direct registry holder followed by network NBT.
    codec::nbt(&dialog, &mut packet)?;
    Ok(packet)
}

pub fn response(mut fields: Fields<'_>) -> Result<Option<(u32, Option<u32>)>> {
    let action = fields.string(128)?;
    let Some(identifier) = action.strip_prefix("sculith:form/") else {
        return Ok(None);
    };
    let Some((form, choice)) = identifier.split_once('/') else {
        return Ok(None);
    };
    let Ok(form_id) = form.parse::<u32>() else {
        return Ok(None);
    };
    let selection = if choice == "cancel" {
        None
    } else {
        let Ok(index) = choice.parse::<u32>() else {
            return Ok(None);
        };
        Some(index)
    };
    // Protocol 777 adds a length-prefixed optional NBT payload. Our static
    // custom actions have no payload: one TAG_End byte, prefixed with length 1.
    ensure!(
        fields.int()? == 1 && fields.byte()? == 0,
        "unexpected form action payload"
    );
    fields.end()?;
    Ok(Some((form_id, selection)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dialog_uses_inline_holder_and_modified_utf8_for_nested_unicode() {
        let packet = show(
            41,
            &SimpleForm {
                title: "안내 👋".into(),
                body: "한 줄\n두 줄".into(),
                buttons: vec!["선택".into()],
            },
        )
        .unwrap();
        let mut fields = Fields(&packet);
        assert_eq!(fields.int().unwrap(), 143);
        assert_eq!(fields.int().unwrap(), 0);
        assert_eq!(fields.byte().unwrap(), 10);
        assert!(
            fields
                .0
                .windows(6)
                .any(|part| part == [0xed, 0xa0, 0xbd, 0xed, 0xb1, 0x8b])
        );
        assert!(
            fields
                .0
                .windows(17)
                .any(|part| part == b"sculith:form/41/0")
        );
    }

    #[test]
    fn custom_click_parses_prefixed_optional_nbt_without_general_nbt_input() {
        let mut payload = Vec::new();
        super::super::write_string("sculith:form/41/0", &mut payload);
        payload.extend_from_slice(&[1, 0]);
        assert_eq!(response(Fields(&payload)).unwrap(), Some((41, Some(0))));
        payload.pop();
        assert!(response(Fields(&payload)).is_err());
    }
}
