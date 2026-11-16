//! Vanilla dialog controls and a flat, bounded custom-action value decoder.
use super::{button, compound};
use crate::protocol::java::codec::{self, Fields};
use anyhow::{Result, bail, ensure};
use nbtx::{Value as V, ValueList as L};
use sculith_core::{
    CustomForm, FormField, FormValue,
    forms::{MAX_FORM_FIELDS, MAX_FORM_RESPONSE_BYTES},
};

pub fn show(id: u32, form: &CustomForm) -> Result<Vec<u8>> {
    form.validate().map_err(anyhow::Error::msg)?;
    let inputs = form
        .fields
        .iter()
        .enumerate()
        .map(|(index, field)| {
            let (kind, key, mut control) = match field {
                FormField::Text {
                    default,
                    max_length,
                    ..
                } => (
                    "text",
                    "text",
                    compound([
                        ("initial", codec::nbt_string(default)?),
                        ("max_length", V::Int(i32::from(*max_length))),
                    ]),
                ),
                FormField::Toggle { default, .. } => (
                    "boolean",
                    "toggle",
                    compound([("initial", V::Byte(i8::from(*default)))]),
                ),
                FormField::Dropdown {
                    options, default, ..
                } => {
                    let entries = options
                        .iter()
                        .enumerate()
                        .map(|(i, label)| {
                            Ok(compound([
                                ("id", codec::nbt_string(&i.to_string())?),
                                ("display", codec::nbt_string(label)?),
                                ("initial", V::Byte(i8::from(i == *default as usize))),
                            ]))
                        })
                        .collect::<Result<Vec<_>>>()?;
                    (
                        "single_option",
                        "number",
                        compound([("options", V::List(L::Compound(entries)))]),
                    )
                }
                FormField::Slider {
                    min,
                    max,
                    step,
                    default,
                    ..
                } => (
                    "number_range",
                    "number",
                    compound([
                        ("start", V::Float(*min as f32)),
                        ("end", V::Float(*max as f32)),
                        ("step", V::Float(*step as f32)),
                        ("initial", V::Float(*default as f32)),
                    ]),
                ),
            };
            for (name, value) in [
                ("type", codec::nbt_string(&format!("minecraft:{kind}"))?),
                ("key", codec::nbt_string(&format!("{key}_{index}"))?),
                ("label", codec::nbt_string(field.label())?),
            ] {
                control.insert(name.as_bytes().to_vec().into(), value);
            }
            Ok(control)
        })
        .collect::<Result<Vec<_>>>()?;
    let mut submit = button("확인", &format!("sculith:input/{id}/submit"))?;
    submit.insert(
        b"action".to_vec().into(),
        V::Compound(compound([
            ("type", codec::nbt_string("minecraft:dynamic/custom")?),
            (
                "id",
                codec::nbt_string(&format!("sculith:input/{id}/submit"))?,
            ),
        ])),
    );
    let dialog = V::Compound(compound([
        ("type", codec::nbt_string("minecraft:multi_action")?),
        ("title", codec::nbt_string(&form.title)?),
        ("pause", V::Byte(0)),
        ("can_close_with_escape", V::Byte(1)),
        ("after_action", codec::nbt_string("close")?),
        ("columns", V::Int(1)),
        ("inputs", V::List(L::Compound(inputs))),
        ("actions", V::List(L::Compound(vec![submit]))),
        (
            "exit_action",
            V::Compound(button("닫기", &format!("sculith:input/{id}/cancel"))?),
        ),
    ]));
    let mut packet = codec::packet(143);
    packet.push(0);
    codec::nbt(&dialog, &mut packet)?;
    Ok(packet)
}

/// Accept only the string-valued flat compound emitted by vanilla's dynamic
/// custom action. No lists, nested tags, arbitrary keys or duplicate indexes.
pub fn response(mut fields: Fields<'_>) -> Result<Option<(u32, Option<Vec<FormValue>>)>> {
    let action = fields.string(128)?;
    let Some(action) = action.strip_prefix("sculith:input/") else {
        return Ok(None);
    };
    let (id, action) = action
        .split_once('/')
        .ok_or_else(|| anyhow::anyhow!("invalid input form action"))?;
    let id = id.parse::<u32>()?;
    let payload = fields.byte_array(MAX_FORM_RESPONSE_BYTES)?;
    fields.end()?;
    if action == "cancel" {
        ensure!(payload == [0], "unexpected cancel payload");
        return Ok(Some((id, None)));
    }
    ensure!(action == "submit", "invalid input form action");
    let mut data = Fields(payload);
    ensure!(data.byte()? == 10, "input payload must be a compound");
    let mut values = std::collections::BTreeMap::new();
    loop {
        let tag = data.byte()?;
        if tag == 0 {
            break;
        }
        ensure!(
            tag == 8 && values.len() < MAX_FORM_FIELDS,
            "invalid input payload tag or field count"
        );
        let key = nbt_string(&mut data, 32)?;
        let (kind, index) = key
            .split_once('_')
            .ok_or_else(|| anyhow::anyhow!("invalid input key"))?;
        let index = index.parse::<usize>()?;
        ensure!(
            index < MAX_FORM_FIELDS && key == format!("{kind}_{index}"),
            "invalid input index"
        );
        let value = nbt_string(&mut data, 1024)?;
        let value = match kind {
            "text" => FormValue::Text(value),
            "toggle" => FormValue::Toggle(match value.as_str() {
                "true" => true,
                "false" => false,
                _ => bail!("invalid toggle value"),
            }),
            "number" => FormValue::number(value.parse::<f64>()?).map_err(anyhow::Error::msg)?,
            _ => bail!("unknown input key"),
        };
        ensure!(
            values.insert(index, value).is_none(),
            "duplicate input index"
        );
    }
    data.end()?;
    ensure!(
        values.keys().copied().eq(0..values.len()),
        "missing input index"
    );
    Ok(Some((id, Some(values.into_values().collect()))))
}

fn nbt_string(data: &mut Fields<'_>, limit: usize) -> Result<String> {
    let count = u16::from_be_bytes(data.take(2)?.try_into()?) as usize;
    ensure!(count <= limit * 3, "input NBT string exceeds bound");
    let raw = data.take(count)?;
    let mut units = Vec::with_capacity(count);
    let mut cursor = 0;
    while cursor < raw.len() {
        let first = raw[cursor];
        cursor += 1;
        let (mut value, tail, minimum) = match first {
            1..=0x7f => (u16::from(first), 0, 0),
            0xc0..=0xdf => (u16::from(first & 0x1f), 1, 0x80),
            0xe0..=0xef => (u16::from(first & 0x0f), 2, 0x800),
            _ => bail!("invalid modified UTF-8 lead byte"),
        };
        ensure!(cursor + tail <= raw.len(), "truncated modified UTF-8");
        for byte in &raw[cursor..cursor + tail] {
            ensure!(byte & 0xc0 == 0x80, "invalid modified UTF-8 continuation");
            value = (value << 6) | u16::from(byte & 0x3f);
        }
        cursor += tail;
        ensure!(
            value >= minimum || (tail == 1 && value == 0),
            "overlong modified UTF-8"
        );
        units.push(value);
    }
    let text = String::from_utf16(&units)?;
    ensure!(text.len() <= limit, "input NBT text exceeds bound");
    Ok(text)
}

#[cfg(test)]
mod tests {
    use super::*;
    fn packet(entries: &[(&str, &str)]) -> Vec<u8> {
        let mut fields = codec::packet(0);
        fields.clear();
        super::super::super::write_string("sculith:input/41/submit", &mut fields);
        let mut payload = vec![10];
        for (key, value) in entries {
            payload.push(8);
            for text in [key, value] {
                // Reuse only the production string encoder; adversarial frames
                // below alter the independently assembled flat tag structure.
                let mut encoded = Vec::new();
                codec::nbt(&codec::nbt_string(text).unwrap(), &mut encoded).unwrap();
                payload.extend_from_slice(&encoded[1..]);
            }
        }
        payload.push(0);
        super::super::super::write_varint(payload.len() as i32, &mut fields);
        fields.extend_from_slice(&payload);
        fields
    }
    #[test]
    fn input_payload_is_ordered_without_recursion_and_decodes_surrogate_pairs() {
        let packet = packet(&[
            ("number_3", "5.0"),
            ("toggle_1", "false"),
            ("text_0", "이름 👋"),
            ("number_2", "1"),
        ]);
        assert_eq!(
            response(Fields(&packet)).unwrap(),
            Some((
                41,
                Some(vec![
                    FormValue::Text("이름 👋".into()),
                    FormValue::Toggle(false),
                    FormValue::Number(1),
                    FormValue::Number(5)
                ])
            ))
        );
        for entries in [
            vec![("text_0", "ok"), ("text_0", "duplicate")],
            vec![("text_1", "missing")],
            vec![("other_0", "unknown")],
            vec![("toggle_0", "1")],
            vec![("number_0", "NaN")],
            vec![("number_0", "1.1")],
            vec![("text_00", "alias")],
        ] {
            assert!(response(Fields(&self::packet(&entries))).is_err());
        }
        for end in 0..packet.len() {
            assert!(response(Fields(&packet[..end])).is_err());
        }
    }
    #[test]
    fn mutf8_rejects_overlong_unpaired_surrogates_and_four_byte_utf8() {
        for raw in [
            &b"\xc1\x81"[..],
            &b"\xed\xa0\x80"[..],
            &b"\xf0\x9f\x91\x8b"[..],
            &b"\0"[..],
            &b"\xe0\x81\x81"[..],
        ] {
            let mut data = (raw.len() as u16).to_be_bytes().to_vec();
            data.extend_from_slice(raw);
            assert!(nbt_string(&mut Fields(&data), 1024).is_err());
        }
    }
}
