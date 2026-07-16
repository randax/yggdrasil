//! Stable machine-readable CLI output.

/// Serialize a typed response with recursively sorted object keys.
pub(crate) fn canonical_json<T: serde::Serialize>(body: &T) -> anyhow::Result<String> {
    fn sort_keys(value: &mut serde_json::Value) {
        match value {
            serde_json::Value::Object(map) => {
                map.sort_keys();
                map.values_mut().for_each(sort_keys);
            }
            serde_json::Value::Array(items) => items.iter_mut().for_each(sort_keys),
            _ => {}
        }
    }

    let mut value = serde_json::to_value(body)?;
    sort_keys(&mut value);
    Ok(serde_json::to_string(&value)?)
}

pub(crate) fn print_json<T: serde::Serialize>(body: &T) -> anyhow::Result<()> {
    println!("{}", canonical_json(body)?);
    Ok(())
}
