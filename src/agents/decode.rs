//! Tolerant field decoders for the small structs records and events are decoded into: a
//! field that does not fit never drops the record around it (DESIGN.md §3).

use serde::de::DeserializeOwned;
use serde::{Deserialize, Deserializer};
use serde_json::Value;

/// A field that is `null` or does not fit reads as its default.
pub fn or_default<'de, D: Deserializer<'de>, T: DeserializeOwned + Default>(
    d: D,
) -> Result<T, D::Error> {
    Ok(T::deserialize(Value::deserialize(d)?).unwrap_or_default())
}

/// The entries of a list that decode: one odd entry does not fail the whole list, and a
/// value other than an array has none.
pub fn entries<'de, D: Deserializer<'de>, T: DeserializeOwned>(d: D) -> Result<Vec<T>, D::Error> {
    let Value::Array(items) = Value::deserialize(d)? else {
        return Ok(Vec::new());
    };
    Ok(items
        .into_iter()
        .filter_map(|v| {
            T::deserialize(v)
                .inspect_err(|e| tracing::warn!("skipped a list entry: {e}"))
                .ok()
        })
        .collect())
}
