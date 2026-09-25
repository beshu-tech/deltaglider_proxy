// SPDX-License-Identifier: BUSL-1.1

//! Typed deserialization of a config value tree that carries expanded
//! `${env:NAME}` values.
//!
//! The expander splices a value that is a WHOLE scalar as `!envref "value"`
//! (see `expansion.rs`), never as plain text: plain text is re-typed by YAML,
//! so an all-digit AES key read as a number and a secret `true` as a bool,
//! and the config failed to load. An `!envref` node is a string, and it
//! becomes whatever the target field asks for: a string field takes it as
//! is, a bool, number or `Option` field parses it (so
//! `force_path_style: ${env:X:-false}` still works).
//!
//! A subtree without an `!envref` node is handed to `serde_yaml` unchanged,
//! so every other document parses exactly as before.

use serde::de::{self, DeserializeOwned, IntoDeserializer, Visitor};
use serde_yaml::{Error, Mapping, Value};

/// `deserialize_with` for a `bool` field inside an internally tagged or
/// untagged enum (`BackendConfig`, `BackendEncryptionConfig`). serde
/// buffers such an enum before it knows the variant, so an `!envref` value
/// reaches the field as a string; this accepts `true`/`false` in either
/// form (a JSON section PUT sends a real bool). A source test keeps every
/// such field on it.
pub(crate) fn bool_or_string<'de, D: de::Deserializer<'de>>(d: D) -> Result<bool, D::Error> {
    #[derive(serde::Deserialize)]
    #[serde(untagged)]
    enum Wire {
        Bool(bool),
        Str(String),
    }
    match <Wire as serde::Deserialize>::deserialize(d)? {
        Wire::Bool(b) => Ok(b),
        Wire::Str(s) => parse_bool(s.trim())
            .ok_or_else(|| de::Error::invalid_value(de::Unexpected::Str(&s), &"`true` or `false`")),
    }
}

/// The tag the expander puts on a whole-scalar env value.
pub(crate) const ENV_REF_TAG: &str = "!envref";

fn is_env_ref_node(v: &Value) -> Option<&str> {
    match v {
        Value::Tagged(t) if t.tag == serde_yaml::value::Tag::new(ENV_REF_TAG) => match &t.value {
            Value::String(s) => Some(s.as_str()),
            _ => None,
        },
        _ => None,
    }
}

fn contains_env_ref(v: &Value) -> bool {
    match v {
        Value::Sequence(s) => s.iter().any(contains_env_ref),
        Value::Mapping(m) => m
            .iter()
            .any(|(k, v)| contains_env_ref(k) || contains_env_ref(v)),
        Value::Tagged(t) => {
            t.tag == serde_yaml::value::Tag::new(ENV_REF_TAG) || contains_env_ref(&t.value)
        }
        _ => false,
    }
}

/// `serde_yaml::from_value`, with `!envref` nodes typed by their target.
pub(crate) fn from_value<T: DeserializeOwned>(v: Value) -> Result<T, Error> {
    if !contains_env_ref(&v) {
        return serde_yaml::from_value(v);
    }
    T::deserialize(Lenient(v))
}

struct Lenient(Value);

fn is_null_word(s: &str) -> bool {
    matches!(s, "" | "~" | "null" | "Null" | "NULL")
}

fn parse_bool(s: &str) -> Option<bool> {
    match s {
        "true" | "True" | "TRUE" => Some(true),
        "false" | "False" | "FALSE" => Some(false),
        _ => None,
    }
}

macro_rules! parse_num {
    ($name:ident, $visit:ident, $ty:ty) => {
        fn $name<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value, Error> {
            if let Some(s) = is_env_ref_node(&self.0) {
                return match s.trim().parse::<$ty>() {
                    Ok(n) => visitor.$visit(n),
                    Err(_) => Err(de::Error::invalid_value(
                        de::Unexpected::Str(s),
                        &concat!("an env value that parses as ", stringify!($ty)),
                    )),
                };
            }
            self.plain(|v| v.$name(visitor))
        }
    };
}

impl Lenient {
    /// Not an `!envref` node: serde_yaml decides, as without this module.
    fn plain<T>(self, f: impl FnOnce(Value) -> Result<T, Error>) -> Result<T, Error> {
        f(self.0)
    }
}

impl<'de> de::Deserializer<'de> for Lenient {
    type Error = Error;

    fn deserialize_any<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value, Error> {
        if let Some(s) = is_env_ref_node(&self.0) {
            // A buffered target (a tagged enum) cannot say its type. A null
            // word stays null, as the plain splice made it (`${env:X:-null}`,
            // `${env:X:-}`); everything else is a string.
            if is_null_word(s) {
                return visitor.visit_unit();
            }
            return visitor.visit_string(s.to_string());
        }
        if !contains_env_ref(&self.0) {
            return de::Deserializer::deserialize_any(self.0, visitor);
        }
        match self.0 {
            Value::Mapping(m) => visitor.visit_map(MapAccess::new(m)),
            Value::Sequence(s) => visitor.visit_seq(SeqAccess(s.into_iter())),
            Value::Tagged(t) => Lenient(t.value).deserialize_any(visitor),
            other => de::Deserializer::deserialize_any(other, visitor),
        }
    }

    fn deserialize_bool<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value, Error> {
        if let Some(s) = is_env_ref_node(&self.0) {
            return match parse_bool(s.trim()) {
                Some(b) => visitor.visit_bool(b),
                None => Err(de::Error::invalid_value(
                    de::Unexpected::Str(s),
                    &"an env value `true` or `false`",
                )),
            };
        }
        self.plain(|v| v.deserialize_bool(visitor))
    }

    parse_num!(deserialize_i8, visit_i8, i8);
    parse_num!(deserialize_i16, visit_i16, i16);
    parse_num!(deserialize_i32, visit_i32, i32);
    parse_num!(deserialize_i64, visit_i64, i64);
    parse_num!(deserialize_u8, visit_u8, u8);
    parse_num!(deserialize_u16, visit_u16, u16);
    parse_num!(deserialize_u32, visit_u32, u32);
    parse_num!(deserialize_u64, visit_u64, u64);
    parse_num!(deserialize_f32, visit_f32, f32);
    parse_num!(deserialize_f64, visit_f64, f64);

    fn deserialize_option<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value, Error> {
        match &self.0 {
            Value::Null => visitor.visit_none(),
            v if is_env_ref_node(v).is_some_and(is_null_word) => visitor.visit_none(),
            v if contains_env_ref(v) => visitor.visit_some(self),
            _ => self.plain(|v| v.deserialize_option(visitor)),
        }
    }

    fn deserialize_unit<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value, Error> {
        if is_env_ref_node(&self.0).is_some_and(is_null_word) {
            return visitor.visit_unit();
        }
        self.plain(|v| v.deserialize_unit(visitor))
    }

    fn deserialize_newtype_struct<V: Visitor<'de>>(
        self,
        name: &'static str,
        visitor: V,
    ) -> Result<V::Value, Error> {
        if contains_env_ref(&self.0) {
            return visitor.visit_newtype_struct(self);
        }
        self.plain(|v| v.deserialize_newtype_struct(name, visitor))
    }

    fn deserialize_enum<V: Visitor<'de>>(
        self,
        name: &'static str,
        variants: &'static [&'static str],
        visitor: V,
    ) -> Result<V::Value, Error> {
        if let Some(s) = is_env_ref_node(&self.0) {
            let d: de::value::StringDeserializer<Error> = s.to_string().into_deserializer();
            return visitor.visit_enum(d);
        }
        match self.0 {
            // Externally tagged `{ Variant: value }` with an env value inside.
            Value::Mapping(m) if m.len() == 1 && contains_env_ref(&Value::Mapping(m.clone())) => {
                let (k, v) = m.into_iter().next().expect("one entry");
                visitor.visit_enum(EnumAccess(k, v))
            }
            other => other.deserialize_enum(name, variants, visitor),
        }
    }

    fn deserialize_str<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value, Error> {
        match is_env_ref_node(&self.0) {
            Some(s) => visitor.visit_string(s.to_string()),
            None => self.deserialize_any(visitor),
        }
    }
    fn deserialize_string<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value, Error> {
        self.deserialize_str(visitor)
    }
    fn deserialize_char<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value, Error> {
        self.deserialize_any(visitor)
    }
    fn deserialize_identifier<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value, Error> {
        self.deserialize_any(visitor)
    }
    fn deserialize_bytes<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value, Error> {
        self.deserialize_any(visitor)
    }
    fn deserialize_byte_buf<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value, Error> {
        self.deserialize_any(visitor)
    }
    fn deserialize_seq<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value, Error> {
        self.deserialize_any(visitor)
    }
    fn deserialize_tuple<V: Visitor<'de>>(
        self,
        _len: usize,
        visitor: V,
    ) -> Result<V::Value, Error> {
        self.deserialize_any(visitor)
    }
    fn deserialize_tuple_struct<V: Visitor<'de>>(
        self,
        _name: &'static str,
        _len: usize,
        visitor: V,
    ) -> Result<V::Value, Error> {
        self.deserialize_any(visitor)
    }
    fn deserialize_map<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value, Error> {
        self.deserialize_any(visitor)
    }
    fn deserialize_struct<V: Visitor<'de>>(
        self,
        name: &'static str,
        fields: &'static [&'static str],
        visitor: V,
    ) -> Result<V::Value, Error> {
        if !contains_env_ref(&self.0) {
            return self.0.deserialize_struct(name, fields, visitor);
        }
        self.deserialize_any(visitor)
    }
    fn deserialize_unit_struct<V: Visitor<'de>>(
        self,
        _name: &'static str,
        visitor: V,
    ) -> Result<V::Value, Error> {
        self.deserialize_unit(visitor)
    }
    fn deserialize_ignored_any<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value, Error> {
        visitor.visit_unit()
    }
}

struct SeqAccess(std::vec::IntoIter<Value>);

impl<'de> de::SeqAccess<'de> for SeqAccess {
    type Error = Error;
    fn next_element_seed<T: de::DeserializeSeed<'de>>(
        &mut self,
        seed: T,
    ) -> Result<Option<T::Value>, Error> {
        self.0
            .next()
            .map(|v| seed.deserialize(Lenient(v)))
            .transpose()
    }
    fn size_hint(&self) -> Option<usize> {
        Some(self.0.len())
    }
}

struct MapAccess {
    iter: <Mapping as IntoIterator>::IntoIter,
    value: Option<Value>,
}

impl MapAccess {
    fn new(m: Mapping) -> Self {
        Self {
            iter: m.into_iter(),
            value: None,
        }
    }
}

impl<'de> de::MapAccess<'de> for MapAccess {
    type Error = Error;
    fn next_key_seed<K: de::DeserializeSeed<'de>>(
        &mut self,
        seed: K,
    ) -> Result<Option<K::Value>, Error> {
        match self.iter.next() {
            Some((k, v)) => {
                self.value = Some(v);
                seed.deserialize(Lenient(k)).map(Some)
            }
            None => Ok(None),
        }
    }
    fn next_value_seed<V: de::DeserializeSeed<'de>>(&mut self, seed: V) -> Result<V::Value, Error> {
        let v = self
            .value
            .take()
            .ok_or_else(|| <Error as de::Error>::custom("map value missing"))?;
        seed.deserialize(Lenient(v))
    }
}

struct EnumAccess(Value, Value);

impl<'de> de::EnumAccess<'de> for EnumAccess {
    type Error = Error;
    type Variant = VariantAccess;
    fn variant_seed<V: de::DeserializeSeed<'de>>(
        self,
        seed: V,
    ) -> Result<(V::Value, VariantAccess), Error> {
        let variant = seed.deserialize(Lenient(self.0))?;
        Ok((variant, VariantAccess(self.1)))
    }
}

struct VariantAccess(Value);

impl<'de> de::VariantAccess<'de> for VariantAccess {
    type Error = Error;
    fn unit_variant(self) -> Result<(), Error> {
        de::Deserialize::deserialize(Lenient(self.0))
    }
    fn newtype_variant_seed<T: de::DeserializeSeed<'de>>(self, seed: T) -> Result<T::Value, Error> {
        seed.deserialize(Lenient(self.0))
    }
    fn tuple_variant<V: Visitor<'de>>(self, _len: usize, visitor: V) -> Result<V::Value, Error> {
        de::Deserializer::deserialize_any(Lenient(self.0), visitor)
    }
    fn struct_variant<V: Visitor<'de>>(
        self,
        _fields: &'static [&'static str],
        visitor: V,
    ) -> Result<V::Value, Error> {
        de::Deserializer::deserialize_any(Lenient(self.0), visitor)
    }
}

#[cfg(test)]
mod tests {
    /// serde buffers an internally tagged enum before it knows the variant,
    /// so a `!envref` reaches its fields as a string. Every `bool` field in
    /// such an enum must parse one (`bool_or_string`), or
    /// `force_path_style: ${env:X}` fails to load.
    #[test]
    fn bool_fields_of_tagged_enums_accept_env_strings() {
        let mut offenders = Vec::new();
        for file in ["src/config/mod.rs", "src/config_sections.rs"] {
            let text = std::fs::read_to_string(
                concat!(env!("CARGO_MANIFEST_DIR"), "/").to_string() + file,
            )
            .unwrap();
            let text = text.split("\n#[cfg(test)]\nmod ").next().unwrap();
            for part in text
                .split("#[serde(tag")
                .skip(1)
                .chain(text.split("#[serde(untagged").skip(1))
            {
                // The enum body: from its first `{` to the matching `}`.
                let Some(open) = part.find('{') else { continue };
                let mut depth = 0;
                let mut end = part.len();
                for (i, c) in part[open..].char_indices() {
                    match c {
                        '{' => depth += 1,
                        '}' => {
                            depth -= 1;
                            if depth == 0 {
                                end = open + i;
                                break;
                            }
                        }
                        _ => {}
                    }
                }
                let body = &part[open..end];
                let lines: Vec<&str> = body.lines().collect();
                for (i, l) in lines.iter().enumerate() {
                    if l.trim_end().ends_with(": bool,") {
                        let attrs = lines[..i]
                            .iter()
                            .rev()
                            .take_while(|p| {
                                // Back to the previous field or the variant start.
                                let t = p.trim();
                                !(t.ends_with('{')
                                    || (t.ends_with(',')
                                        && t.contains(": ")
                                        && !t.starts_with("///")))
                            })
                            .copied()
                            .collect::<Vec<_>>()
                            .join(" ");
                        if !attrs.contains("bool_or_string") {
                            offenders.push(format!("{file}: {}", l.trim()));
                        }
                    }
                }
            }
        }
        assert!(
            offenders.is_empty(),
            "add deserialize_with = \"lenient::bool_or_string\": {offenders:?}"
        );
    }
}
