//! Bounded native response decoding. The owner checks the pending definition.
use anyhow::{Result, bail, ensure};
use sculith_core::{
    FormValue,
    forms::{MAX_FORM_FIELDS, MAX_FORM_RESPONSE_BYTES},
};

#[derive(Debug, PartialEq, Eq)]
pub enum Reply {
    Choice(u32),
    Values(Vec<FormValue>),
    Cancelled,
}

pub fn response(json: Option<&str>, cancelled: bool) -> Result<Reply> {
    let Some(json) = json else {
        return Ok(Reply::Cancelled);
    };
    ensure!(
        !cancelled && json.len() <= MAX_FORM_RESPONSE_BYTES,
        "invalid native form response size or cancellation"
    );
    let value: serde_json::Value = serde_json::from_str(json)?;
    match value {
        serde_json::Value::Null => Ok(Reply::Cancelled),
        serde_json::Value::Number(value) => Ok(Reply::Choice(
            value
                .as_u64()
                .and_then(|n| u32::try_from(n).ok())
                .ok_or_else(|| anyhow::anyhow!("invalid form choice"))?,
        )),
        serde_json::Value::Array(values) => {
            ensure!(values.len() <= MAX_FORM_FIELDS, "too many form values");
            Ok(Reply::Values(
                values
                    .into_iter()
                    .map(|value| match value {
                        serde_json::Value::String(value) => {
                            ensure!(value.len() <= 1024, "form text too long");
                            Ok(FormValue::Text(value))
                        }
                        serde_json::Value::Bool(value) => Ok(FormValue::Toggle(value)),
                        serde_json::Value::Number(value) => FormValue::number(
                            value
                                .as_f64()
                                .ok_or_else(|| anyhow::anyhow!("invalid form number"))?,
                        )
                        .map_err(anyhow::Error::msg),
                        _ => bail!("unsupported form value type"),
                    })
                    .collect::<Result<_>>()?,
            ))
        }
        _ => bail!("unsupported form response"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn native_inputs_are_bounded_typed_and_never_coerce_invalid_values_to_cancellation() {
        assert_eq!(
            response(Some("[\"이름 👋\",false,1,5.0]"), false).unwrap(),
            Reply::Values(vec![
                FormValue::Text("이름 👋".into()),
                FormValue::Toggle(false),
                FormValue::Number(1),
                FormValue::Number(5)
            ])
        );
        assert_eq!(response(None, true).unwrap(), Reply::Cancelled);
        assert_eq!(response(Some("null"), false).unwrap(), Reply::Cancelled);
        assert_eq!(response(Some("1"), false).unwrap(), Reply::Choice(1));
        for json in [
            "{}",
            "[null]",
            "[[1]]",
            "[1.5]",
            "[2147483648]",
            "[1e400]",
            "[true,false,0,0,0,0,0,0,0]",
        ] {
            assert!(response(Some(json), false).is_err(), "{json}");
        }
        assert!(response(Some("[true]"), true).is_err());
        assert!(response(Some(&" ".repeat(MAX_FORM_RESPONSE_BYTES + 1)), false).is_err());
    }
}
