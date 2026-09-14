//! Strict parsing for untrusted JSON (provider output, Stage 9 frames).
//!
//! `serde_json::Value` silently keeps the last of duplicate object keys, so two
//! consumers of the same bytes could disagree about a field. Untrusted input is
//! therefore rejected on any duplicate key, with an explicit nesting bound that is
//! tighter than serde_json's own recursion guard.
use serde::de::{DeserializeSeed, Error as _, MapAccess, SeqAccess, Visitor};
use serde_json::{Map, Number, Value};

pub const MAX_DEPTH: usize = 64;

pub fn from_slice(bytes: &[u8]) -> serde_json::Result<Value> {
    let mut deserializer = serde_json::Deserializer::from_slice(bytes);
    let value = Strict(0).deserialize(&mut deserializer)?;
    deserializer.end()?;
    Ok(value)
}

pub fn from_str(text: &str) -> serde_json::Result<Value> {
    from_slice(text.as_bytes())
}

struct Strict(usize);

impl<'de> DeserializeSeed<'de> for Strict {
    type Value = Value;
    fn deserialize<D: serde::Deserializer<'de>>(self, d: D) -> Result<Value, D::Error> {
        d.deserialize_any(self)
    }
}

impl<'de> Visitor<'de> for Strict {
    type Value = Value;
    fn expecting(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        f.write_str("JSON value")
    }
    fn visit_bool<E>(self, v: bool) -> Result<Value, E> {
        Ok(Value::Bool(v))
    }
    fn visit_i64<E>(self, v: i64) -> Result<Value, E> {
        Ok(Value::Number(v.into()))
    }
    fn visit_u64<E>(self, v: u64) -> Result<Value, E> {
        Ok(Value::Number(v.into()))
    }
    fn visit_f64<E: serde::de::Error>(self, v: f64) -> Result<Value, E> {
        Number::from_f64(v)
            .map(Value::Number)
            .ok_or_else(|| E::custom("non-finite JSON number"))
    }
    fn visit_str<E>(self, v: &str) -> Result<Value, E> {
        Ok(Value::String(v.to_owned()))
    }
    fn visit_string<E>(self, v: String) -> Result<Value, E> {
        Ok(Value::String(v))
    }
    fn visit_unit<E>(self) -> Result<Value, E> {
        Ok(Value::Null)
    }
    fn visit_none<E>(self) -> Result<Value, E> {
        Ok(Value::Null)
    }
    fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> Result<Value, A::Error> {
        if self.0 >= MAX_DEPTH {
            return Err(A::Error::custom("JSON nesting exceeds limit"));
        }
        let mut values = vec![];
        while let Some(value) = seq.next_element_seed(Strict(self.0 + 1))? {
            values.push(value);
        }
        Ok(Value::Array(values))
    }
    fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<Value, A::Error> {
        if self.0 >= MAX_DEPTH {
            return Err(A::Error::custom("JSON nesting exceeds limit"));
        }
        let mut object = Map::new();
        while let Some(key) = map.next_key::<String>()? {
            if object.contains_key(&key) {
                return Err(A::Error::custom(format!("duplicate JSON key {key:?}")));
            }
            let value = map.next_value_seed(Strict(self.0 + 1))?;
            object.insert(key, value);
        }
        Ok(Value::Object(object))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strict_json_rejects_duplicate_keys_and_deep_nesting() {
        assert_eq!(
            from_str(r#"{"a":[1,{"b":null}],"c":"d"}"#).unwrap(),
            serde_json::json!({"a":[1,{"b":null}],"c":"d"})
        );
        let error = from_str(r#"{"status":"FAILED","status":"SUCCEEDED"}"#).unwrap_err();
        assert!(error.to_string().contains("duplicate JSON key"));
        assert!(from_str(r#"{"a":{"x":1,"x":2}}"#).is_err());
        let deep = format!("{}{}", "[".repeat(MAX_DEPTH + 1), "]".repeat(MAX_DEPTH + 1));
        assert!(from_str(&deep).is_err());
        let ok = format!("{}{}", "[".repeat(MAX_DEPTH), "]".repeat(MAX_DEPTH));
        assert!(from_str(&ok).is_ok());
        assert!(from_str("{} trailing").is_err());
    }
}
