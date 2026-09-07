//! JavaScript primitive operations observable through pinned Pi's tool results.

pub(crate) fn number_string(value: f64) -> String {
    ryu_js::Buffer::new().format(value).to_owned()
}

/// JSON.stringify(value, null, 2), as used in Pi's validation diagnostics.
/// Preserve JavaScript number rendering and integer-key enumeration even when
/// the original JSON used integer precision or ordering not available in JS.
pub(crate) fn pretty_json(value: &serde_json::Value) -> String {
    fn write(value: &serde_json::Value, output: &mut String, depth: usize) {
        use crate::lossless_json::{self, JsString};
        use serde_json::Value;
        if JsString::from_value(value).is_some() {
            output.push_str(&lossless_json::to_string(value).unwrap());
            return;
        }
        let (open, close, entries): (_, _, Vec<(Option<JsString>, &Value)>) = match value {
            Value::Number(number) => {
                output.push_str(&number_string(number.as_f64().unwrap()));
                return;
            }
            Value::Array(array) => ('[', ']', array.iter().map(|value| (None, value)).collect()),
            Value::Object(_) => (
                '{',
                '}',
                lossless_json::object_entries(value)
                    .unwrap()
                    .into_iter()
                    .map(|(key, value)| (Some(key), value))
                    .collect(),
            ),
            _ => {
                output.push_str(&serde_json::to_string(value).unwrap());
                return;
            }
        };
        output.push(open);
        for (i, (key, value)) in entries.iter().enumerate() {
            if i > 0 {
                output.push(',');
            }
            output.push('\n');
            output.push_str(&"  ".repeat(depth + 1));
            if let Some(key) = key {
                output.push_str(&lossless_json::to_string(key).unwrap());
                output.push_str(": ");
            }
            write(value, output, depth + 1);
        }
        if !entries.is_empty() {
            output.push('\n');
            output.push_str(&"  ".repeat(depth));
        }
        output.push(close);
    }
    let mut output = String::new();
    write(value, &mut output, 0);
    output
}

// ECMAScript WhiteSpace + LineTerminator (also used by String.trimEnd).
pub(crate) fn whitespace(value: char) -> bool {
    matches!(value, '\u{0009}'..='\u{000d}' | '\u{0020}' | '\u{00a0}' |
        '\u{1680}' | '\u{2000}'..='\u{200a}' | '\u{2028}' | '\u{2029}' |
        '\u{202f}' | '\u{205f}' | '\u{3000}' | '\u{feff}')
}

/// Node's default util.inspect string rendering, used by ERR_INVALID_ARG_VALUE.
/// The pinned Pi tools propagate these native argument errors unchanged.
pub(crate) fn inspect_argument_string(
    value: &crate::lossless_json::JsString,
) -> crate::lossless_json::JsString {
    use crate::lossless_json::JsString;
    fn quote(value: &[u16]) -> String {
        let has = |text: &str| {
            value
                .windows(text.encode_utf16().count())
                .any(|window| window.iter().copied().eq(text.encode_utf16()))
        };
        let delimiter = if has("'") && !has("\"") {
            '"'
        } else if has("'") && has("\"") && !has("`") && !has("${") {
            '`'
        } else {
            '\''
        };
        let mut output = String::from(delimiter);
        for decoded in char::decode_utf16(value.iter().copied()) {
            let character = match decoded {
                Ok(character) => character,
                Err(error) => {
                    output.push_str(&format!("\\u{:04x}", error.unpaired_surrogate()));
                    continue;
                }
            };
            match character {
                '\\' => output.push_str("\\\\"),
                '\'' if delimiter == '\'' => output.push_str("\\'"),
                '\u{0008}' => output.push_str("\\b"),
                '\t' => output.push_str("\\t"),
                '\n' => output.push_str("\\n"),
                '\u{000c}' => output.push_str("\\f"),
                '\r' => output.push_str("\\r"),
                '\u{0000}'..='\u{001f}' | '\u{007f}'..='\u{009f}' => {
                    output.push_str(&format!("\\x{:02X}", character as u32))
                }
                _ => output.push(character),
            }
        }
        output.push(delimiter);
        output
    }
    let inspected = if value.len() > 76 {
        value
            .units()
            .split_inclusive(|unit| *unit == 10)
            .map(quote)
            .collect::<Vec<_>>()
            .join(" +\n  ")
    } else {
        quote(value.units())
    };
    let mut inspected = JsString::from(inspected);
    if inspected.len() > 128 {
        inspected = JsString::from_units(inspected.units()[..128].to_vec());
        inspected.push_str("...");
    }
    inspected
}

/// Number(string)'s unsigned binary/octal/hex forms, rounded once to binary64.
/// Accumulating digits into an f64 would introduce intermediate rounding, and
/// parsing a u64 would reject valid JavaScript inputs larger than 64 bits.
pub(crate) fn radix_number(digits: &str, radix: u32) -> Option<f64> {
    if digits.is_empty() {
        return None;
    }
    let width = radix.ilog2();
    let mut bits = 0usize;
    let mut significand = 0u64;
    let mut round = false;
    let mut sticky = false;
    for digit in digits.chars() {
        let digit = digit.to_digit(radix)?;
        for shift in (0..width).rev() {
            let bit = (digit >> shift) & 1;
            if bits == 0 && bit == 0 {
                continue;
            }
            if bits < 53 {
                significand = (significand << 1) | u64::from(bit);
            } else if bits == 53 {
                round = bit != 0;
            } else {
                sticky |= bit != 0;
            }
            bits += 1;
        }
    }
    if bits <= 53 {
        return Some(significand as f64);
    }
    if bits > 1024 {
        return None;
    }
    if round && (sticky || significand & 1 != 0) {
        significand += 1;
    }
    let result = significand as f64 * 2.0f64.powi((bits - 53) as i32);
    result.is_finite().then_some(result)
}
