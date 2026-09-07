//! Native UTF-16 strings at Pi's logical JSON boundaries.
//!
//! JSON permits isolated surrogate escapes, whereas Rust `String` does not.
//! `JsString` owns the original code units. Its serde representation uses private
//! tagged values only inside Rust; the functions in this module emit ordinary
//! JSON strings. Incoming objects with reserved keys are escaped as entry lists,
//! so no user string or object can be mistaken for an internal string carrier.

use serde::{Deserialize, Deserializer, Serialize, Serializer, de::DeserializeOwned};
use serde_json::{Map, Value};
use std::{fmt, io::Write, ops::Deref};

const STRING: &str = "$bashkitten.internal.utf16";
const OBJECT: &str = "$bashkitten.internal.object";

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct JsString {
    units: Vec<u16>,
    display: String,
}

impl JsString {
    pub fn from_units(units: Vec<u16>) -> Self {
        Self {
            display: String::from_utf16_lossy(&units),
            units,
        }
    }
    pub fn units(&self) -> &[u16] {
        &self.units
    }
    pub fn len(&self) -> usize {
        self.units.len()
    }
    pub fn is_empty(&self) -> bool {
        self.units.is_empty()
    }
    /// Display-only UTF-8 view. Provider conversion must use `sanitized`.
    pub fn as_str(&self) -> &str {
        &self.display
    }
    /// Pi's sanitizeSurrogates: remove lone units, retaining every valid pair.
    pub fn sanitized(&self) -> String {
        char::decode_utf16(self.units.iter().copied())
            .filter_map(Result::ok)
            .collect()
    }
    pub fn push_str(&mut self, value: &str) {
        self.units.extend(value.encode_utf16());
        self.display = String::from_utf16_lossy(&self.units);
    }
    pub fn push_js(&mut self, value: &Self) {
        self.units.extend_from_slice(&value.units);
        self.display = String::from_utf16_lossy(&self.units);
    }
    pub fn prefixed(&self, prefix: &str) -> Self {
        let mut result = Self::from(prefix);
        result.push_js(self);
        result
    }
    pub fn join(values: &[Self], separator: &str) -> Self {
        let mut result = Self::default();
        for (i, value) in values.iter().enumerate() {
            if i > 0 {
                result.push_str(separator);
            }
            result.push_js(value);
        }
        result
    }
    pub fn to_value(&self) -> Value {
        match String::from_utf16(&self.units) {
            Ok(text) => Value::String(text),
            Err(_) => Value::Object(Map::from_iter([(
                STRING.into(),
                serde_json::json!(self.units),
            )])),
        }
    }
    /// An injective alphabet for algorithms implemented using Rust strings:
    /// non-surrogate BMP units stay unchanged; each surrogate unit gets one
    /// supplementary scalar. All original astral scalars were split first,
    /// so these algorithm-only scalars cannot collide with input characters.
    pub(crate) fn algorithm_string(&self) -> String {
        self.units
            .iter()
            .map(|unit| {
                char::from_u32(if (0xd800..=0xdfff).contains(unit) {
                    0xf0000 + u32::from(*unit - 0xd800)
                } else {
                    u32::from(*unit)
                })
                .expect("algorithm scalar")
            })
            .collect()
    }
    pub(crate) fn from_algorithm_string(value: &str) -> Self {
        let mut units = Vec::new();
        for ch in value.chars() {
            if (0xf0000..=0xf07ff).contains(&(ch as u32)) {
                units.push((ch as u32 - 0xf0000) as u16 + 0xd800);
            } else {
                let mut buffer = [0; 2];
                units.extend_from_slice(ch.encode_utf16(&mut buffer));
            }
        }
        Self::from_units(units)
    }
    pub fn from_value(value: &Value) -> Option<Self> {
        if let Some(text) = value.as_str() {
            return Some(Self::from(text));
        }
        let object = value.as_object()?;
        if object.len() != 1 {
            return None;
        }
        let units = object
            .get(STRING)?
            .as_array()?
            .iter()
            .map(|unit| u16::try_from(unit.as_u64()?).ok())
            .collect::<Option<Vec<_>>>()?;
        Some(Self::from_units(units))
    }
}
impl From<&str> for JsString {
    fn from(value: &str) -> Self {
        Self::from_units(value.encode_utf16().collect())
    }
}
impl From<String> for JsString {
    fn from(value: String) -> Self {
        Self::from(value.as_str())
    }
}
impl From<&String> for JsString {
    fn from(value: &String) -> Self {
        Self::from(value.as_str())
    }
}
impl From<&JsString> for JsString {
    fn from(value: &JsString) -> Self {
        value.clone()
    }
}
impl Deref for JsString {
    type Target = str;
    fn deref(&self) -> &str {
        self.as_str()
    }
}
impl fmt::Display for JsString {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.display.fmt(f)
    }
}
impl PartialEq<&str> for JsString {
    fn eq(&self, other: &&str) -> bool {
        self.units.iter().copied().eq(other.encode_utf16())
    }
}
impl PartialEq<String> for JsString {
    fn eq(&self, other: &String) -> bool {
        self == &other.as_str()
    }
}
impl Serialize for JsString {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        self.to_value().serialize(serializer)
    }
}
impl<'de> Deserialize<'de> for JsString {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let value = Value::deserialize(deserializer)?;
        Self::from_value(&value).ok_or_else(|| serde::de::Error::custom("expected a JSON string"))
    }
}

fn error(message: &str) -> serde_json::Error {
    <serde_json::Error as serde::de::Error>::custom(message)
}

/// Parse JSON strings into UTF-16 before serde sees them. Replacing *all* string
/// tokens with indices makes the temporary representation collision-free.
pub fn from_str<T: DeserializeOwned>(input: &str) -> Result<T, serde_json::Error> {
    from_js_str(&JsString::from(input))
}

pub fn from_js_str<T: DeserializeOwned>(input: &JsString) -> Result<T, serde_json::Error> {
    let mut transformed = String::with_capacity(input.len());
    let mut strings = Vec::new();
    let mut chars = input.units().iter().copied().peekable();
    while let Some(unit) = chars.next() {
        let ch = char::from_u32(u32::from(unit)).unwrap_or(char::REPLACEMENT_CHARACTER);
        if ch != '"' {
            transformed.push(ch);
            continue;
        }
        let mut units = Vec::new();
        loop {
            let unit = chars
                .next()
                .ok_or_else(|| error("EOF while parsing a string"))?;
            let ch = char::from_u32(u32::from(unit)).unwrap_or(char::REPLACEMENT_CHARACTER);
            match ch {
                '"' => break,
                '\\' => {
                    let escaped = chars
                        .next()
                        .ok_or_else(|| error("EOF while parsing a string"))?;
                    let escaped =
                        char::from_u32(u32::from(escaped)).unwrap_or(char::REPLACEMENT_CHARACTER);
                    units.push(match escaped {
                        '"' => 34,
                        '\\' => 92,
                        '/' => 47,
                        'b' => 8,
                        'f' => 12,
                        'n' => 10,
                        'r' => 13,
                        't' => 9,
                        'u' => {
                            let mut unit = 0u16;
                            for _ in 0..4 {
                                let digit = chars
                                    .next()
                                    .and_then(|c| char::from_u32(u32::from(c)))
                                    .and_then(|c| c.to_digit(16))
                                    .ok_or_else(|| error("invalid escape"))?;
                                unit = unit * 16 + digit as u16;
                            }
                            unit
                        }
                        _ => return Err(error("invalid escape")),
                    });
                }
                '\0'..='\u{001f}' => return Err(error("control character while parsing a string")),
                _ => units.push(unit),
            }
        }
        transformed.push('"');
        transformed.push_str(&strings.len().to_string());
        transformed.push('"');
        strings.push(JsString::from_units(units));
    }
    fn restore(value: Value, strings: &[JsString]) -> Value {
        match value {
            Value::String(index) => {
                strings[index.parse::<usize>().expect("token index")].to_value()
            }
            Value::Array(values) => {
                Value::Array(values.into_iter().map(|v| restore(v, strings)).collect())
            }
            Value::Object(values) => {
                let mut entries: Vec<(JsString, Value)> = Vec::new();
                for (key, value) in values {
                    let key = strings[key.parse::<usize>().expect("key token index")].clone();
                    let value = restore(value, strings);
                    // JSON.parse keeps the last value at the original key's
                    // enumeration position, including lone-surrogate keys.
                    if let Some((_, previous)) = entries.iter_mut().find(|(old, _)| old == &key) {
                        *previous = value;
                    } else {
                        entries.push((key, value));
                    }
                }
                if entries
                    .iter()
                    .any(|(key, _)| String::from_utf16(key.units()).is_err())
                    || (entries.len() == 1
                        && entries
                            .iter()
                            .any(|(key, _)| key == &STRING || key == &OBJECT))
                {
                    Value::Object(Map::from_iter([(
                        OBJECT.into(),
                        Value::Array(
                            entries
                                .into_iter()
                                .map(|(key, value)| Value::Array(vec![key.to_value(), value]))
                                .collect(),
                        ),
                    )]))
                } else {
                    Value::Object(
                        entries
                            .into_iter()
                            .map(|(key, value)| (key.display, value))
                            .collect(),
                    )
                }
            }
            value => value,
        }
    }
    let value = restore(serde_json::from_str(&transformed)?, &strings);
    serde_json::from_value(value)
}

pub fn from_slice<T: DeserializeOwned>(input: &[u8]) -> Result<T, serde_json::Error> {
    from_str(std::str::from_utf8(input).map_err(|_| error("invalid UTF-8"))?)
}

fn write_string(value: &JsString, output: &mut String) {
    if let Ok(text) = String::from_utf16(value.units()) {
        output.push_str(&serde_json::to_string(&text).expect("quote string"));
        return;
    }
    output.push('"');
    for decoded in char::decode_utf16(value.units.iter().copied()) {
        match decoded {
            Ok(ch) => {
                let mut bytes = [0; 4];
                let quoted = serde_json::to_string(ch.encode_utf8(&mut bytes)).expect("quote char");
                output.push_str(&quoted[1..quoted.len() - 1]);
            }
            Err(error) => output.push_str(&format!("\\u{:04x}", error.unpaired_surrogate())),
        }
    }
    output.push('"');
}

pub fn object_entries(value: &Value) -> Option<Vec<(JsString, &Value)>> {
    let object = value.as_object()?;
    if JsString::from_value(value).is_some() {
        return None;
    }
    let mut entries = if object.len() == 1
        && let Some(entries) = object.get(OBJECT).and_then(Value::as_array)
    {
        entries
            .iter()
            .map(|entry| {
                (
                    JsString::from_value(&entry[0]).expect("object key"),
                    &entry[1],
                )
            })
            .collect::<Vec<_>>()
    } else {
        object
            .iter()
            .map(|(key, value)| (JsString::from(key), value))
            .collect::<Vec<_>>()
    };
    entries.sort_by_key(|(key, _)| {
        key.as_str()
            .parse::<u32>()
            .ok()
            .filter(|n| *n != u32::MAX && n.to_string() == key.as_str())
            .map_or((1, 0), |n| (0, n))
    });
    Some(entries)
}

pub fn object_get<'a>(value: &'a Value, key: &str) -> Option<&'a Value> {
    if let Some(entries) = value
        .as_object()
        .filter(|v| v.len() == 1)
        .and_then(|v| v.get(OBJECT))
        .and_then(Value::as_array)
    {
        return entries
            .iter()
            .find(|entry| JsString::from_value(&entry[0]).is_some_and(|name| name == key))
            .map(|entry| &entry[1]);
    }
    value.get(key)
}

pub fn object_get_mut<'a>(value: &'a mut Value, key: &str) -> Option<&'a mut Value> {
    if value
        .as_object()
        .is_some_and(|v| v.len() == 1 && v.contains_key(OBJECT))
    {
        return value
            .get_mut(OBJECT)?
            .as_array_mut()?
            .iter_mut()
            .find(|entry| JsString::from_value(&entry[0]).is_some_and(|name| name == key))
            .map(|entry| &mut entry[1]);
    }
    value.get_mut(key)
}

pub fn object_remove(value: &mut Value, key: &str) -> Option<Value> {
    if value
        .as_object()
        .is_some_and(|v| v.len() == 1 && v.contains_key(OBJECT))
    {
        let entries = value.get_mut(OBJECT)?.as_array_mut()?;
        let index = entries
            .iter()
            .position(|entry| JsString::from_value(&entry[0]).is_some_and(|name| name == key))?;
        return Some(entries.remove(index)[1].take());
    }
    value.as_object_mut()?.remove(key)
}

pub fn object_insert(value: &mut Value, key: &str, entry: Value) {
    if let Some(previous) = object_get_mut(value, key) {
        *previous = entry;
        return;
    }
    if value
        .as_object()
        .is_some_and(|v| v.len() == 1 && v.contains_key(OBJECT))
    {
        value[OBJECT]
            .as_array_mut()
            .expect("object entries")
            .push(serde_json::json!([key, entry]));
    } else if let Some(object) = value.as_object_mut() {
        object.insert(key.into(), entry);
    }
}

/// Fixed Rust tool schemas have only ordinary named properties. Keep native
/// string carriers, while making those properties visible to serde even when
/// an object also contains unknown keys with unpaired code units.
pub(crate) fn tool_schema_value(value: &Value) -> Value {
    if JsString::from_value(value).is_some() {
        return value.clone();
    }
    if let Some(entries) = object_entries(value) {
        return Value::Object(
            entries
                .into_iter()
                .map(|(key, value)| (key.display, tool_schema_value(value)))
                .collect(),
        );
    }
    if let Some(values) = value.as_array() {
        return Value::Array(values.iter().map(tool_schema_value).collect());
    }
    value.clone()
}

fn write_value(value: &Value, output: &mut String) {
    if let Some(text) = JsString::from_value(value) {
        write_string(&text, output);
        return;
    }
    match value {
        Value::Object(_) => {
            output.push('{');
            for (index, (key, value)) in object_entries(value).expect("object").iter().enumerate() {
                if index > 0 {
                    output.push(',');
                }
                write_string(key, output);
                output.push(':');
                write_value(value, output);
            }
            output.push('}');
        }
        Value::Array(array) => {
            output.push('[');
            for (index, value) in array.iter().enumerate() {
                if index > 0 {
                    output.push(',');
                }
                write_value(value, output);
            }
            output.push(']');
        }
        value => output.push_str(&serde_json::to_string(value).expect("serialize primitive")),
    }
}

pub fn to_string<T: Serialize + ?Sized>(value: &T) -> Result<String, serde_json::Error> {
    let mut output = String::new();
    write_value(&serde_json::to_value(value)?, &mut output);
    Ok(output)
}
pub fn to_vec<T: Serialize + ?Sized>(value: &T) -> Result<Vec<u8>, serde_json::Error> {
    Ok(to_string(value)?.into_bytes())
}
pub fn to_writer<W: Write, T: Serialize + ?Sized>(
    mut writer: W,
    value: &T,
) -> Result<(), serde_json::Error> {
    writer
        .write_all(&to_vec(value)?)
        .map_err(serde_json::Error::io)
}

pub struct Wire<T>(T);
pub fn wire<T: Serialize>(value: T) -> Wire<T> {
    Wire(value)
}
impl<T: Serialize> Serialize for Wire<T> {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let json = to_string(&self.0).map_err(serde::ser::Error::custom)?;
        let raw =
            serde_json::value::RawValue::from_string(json).map_err(serde::ser::Error::custom)?;
        raw.serialize(serializer)
    }
}

/// Convert only where pinned Pi sanitizes provider-bound text. The original
/// logical value is retained for persistence, live output, replay and forks.
pub fn sanitize(value: &Value) -> Value {
    if let Some(text) = JsString::from_value(value) {
        return Value::String(text.sanitized());
    }
    match value {
        Value::Array(values) => Value::Array(values.iter().map(sanitize).collect()),
        Value::Object(values) => {
            if values.len() == 1
                && let Some(entries) = values.get(OBJECT).and_then(Value::as_array)
            {
                return Value::Object(
                    entries
                        .iter()
                        .map(|entry| {
                            (
                                JsString::from_value(&entry[0])
                                    .expect("object key")
                                    .sanitized(),
                                sanitize(&entry[1]),
                            )
                        })
                        .collect(),
                );
            }
            Value::Object(
                values
                    .iter()
                    .map(|(key, value)| (key.clone(), sanitize(value)))
                    .collect(),
            )
        }
        value => value.clone(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn utf16_wire_round_trip_and_collision_escaping() {
        for json in [
            r#"["\ud800x\udc00", "🙂", "\ud83d\ude42"]"#,
            r#"{"$bashkitten.internal.utf16":[55296],"nested":{"$bashkitten.internal.object":[["x",3]]}}"#,
            r#"{"\ud800":"\udc00","normal":{"$bashkitten.internal.utf16":[55296]}}"#,
        ] {
            let value: Value = from_str(json).unwrap();
            let serialized = to_string(&value).unwrap();
            assert_eq!(from_str::<Value>(&serialized).unwrap(), value);
            assert_eq!(serde_json::to_string(&wire(&value)).unwrap(), serialized);
        }
        let value: JsString = from_str(r#""a\ud800🙂\udc00z""#).unwrap();
        assert_eq!(value.units(), &[97, 0xd800, 0xd83d, 0xde42, 0xdc00, 122]);
        assert_eq!(value.sanitized(), "a🙂z");
        assert_eq!(to_string(&value).unwrap(), r#""a\ud800🙂\udc00z""#);
    }
}
