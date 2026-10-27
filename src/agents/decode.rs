//! Tolerant field decoders for the small structs records and events are decoded into: a
//! field that does not fit never drops the record around it (DESIGN.md §3).

use serde::de::DeserializeOwned;
use serde::{Deserialize, Deserializer};
use serde_json::Value;

/// Message content: a string, blocks, or something this version cannot read whole.
#[derive(Debug)]
pub enum Content<B> {
    Text(String),
    /// Blocks, every one read.
    Blocks(Vec<B>),
    /// A shape this version does not know, or blocks it could not all read, with the
    /// blocks it could. The rest may hold anything, tool calls included, so such content
    /// never starts or ends a turn.
    Unknown(Vec<B>),
}

impl<B> Content<B> {
    /// The blocks read, for display; a string has none.
    pub fn blocks(&self) -> &[B] {
        match self {
            Content::Text(_) => &[],
            Content::Blocks(b) | Content::Unknown(b) => b,
        }
    }
}

impl<B> Default for Content<B> {
    fn default() -> Self {
        Content::Blocks(Vec::new())
    }
}

impl<'de, B: DeserializeOwned> Deserialize<'de> for Content<B> {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        Ok(match Value::deserialize(d)? {
            Value::String(s) => Content::Text(s),
            v @ Value::Array(_) => match read_entries(v) {
                (blocks, true) => Content::Blocks(blocks),
                (blocks, false) => Content::Unknown(blocks),
            },
            _ => Content::Unknown(Vec::new()),
        })
    }
}

/// A field that is `null` or does not fit reads as its default.
pub fn or_default<'de, D: Deserializer<'de>, T: DeserializeOwned + Default>(
    d: D,
) -> Result<T, D::Error> {
    Ok(T::deserialize(Value::deserialize(d)?).unwrap_or_default())
}

/// The entries of a list that decode: one odd entry does not fail the whole list, and a
/// value other than an array has none.
pub fn entries<'de, D: Deserializer<'de>, T: DeserializeOwned>(d: D) -> Result<Vec<T>, D::Error> {
    Ok(read_entries(Value::deserialize(d)?).0)
}

/// The entries of a list that decode, and whether that is every one of them (`false` for
/// a value other than an array too).
pub fn read_entries<T: DeserializeOwned>(v: Value) -> (Vec<T>, bool) {
    let Value::Array(items) = v else {
        return (Vec::new(), false);
    };
    let mut out = Vec::with_capacity(items.len());
    let mut whole = true;
    for v in items {
        match T::deserialize(v) {
            Ok(t) => out.push(t),
            Err(e) => {
                tracing::warn!("skipped a list entry: {e}");
                whole = false;
            }
        }
    }
    (out, whole)
}
