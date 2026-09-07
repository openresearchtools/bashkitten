//! JSON.parse syntax diagnostics used by the pinned Pi provider runtime (V8).
//!
//! Serde remains the value parser. This grammar walk runs only for rejected
//! input, preserving V8's token, UTF-16 position and line/column diagnostics.
use crate::lossless_json::JsString;

struct Parser {
    input: Vec<u16>,
    position: usize,
}
impl Parser {
    fn peek(&self) -> Option<u16> {
        self.input.get(self.position).copied()
    }
    fn whitespace(&mut self) {
        while matches!(self.peek(), Some(0x20 | 0x09 | 0x0a | 0x0d)) {
            self.position += 1;
        }
    }
    fn at(&self, message: &str) -> String {
        let (mut line, mut column) = (1, 1);
        let mut previous_cr = false;
        for &unit in &self.input[..self.position] {
            match unit {
                13 => {
                    line += 1;
                    column = 1;
                }
                10 => {
                    if !previous_cr {
                        line += 1;
                    }
                    column = 1;
                }
                _ => column += 1,
            }
            previous_cr = unit == 13;
        }
        let suffix = if message.ends_with("JSON") {
            ""
        } else {
            " in JSON"
        };
        format!(
            "{message}{suffix} at position {} (line {line} column {column})",
            self.position
        )
    }
    fn unexpected(&self) -> String {
        let Some(unit) = self.peek() else {
            return "Unexpected end of JSON input".into();
        };
        let raw = JsString::from_units(self.input.clone()).algorithm_string();
        if matches!(
            raw.as_str(),
            "undefined" | "NaN" | "Infinity" | "[object Object]"
        ) {
            return format!("\"{raw}\" is not valid JSON");
        }
        let token = JsString::from_units(vec![unit]).algorithm_string();
        let (prefix, suffix, snippet) = if self.input.len() <= 20 {
            ("", "", raw)
        } else {
            let begin = self.position.saturating_sub(10);
            let end = (self.position + 10).min(self.input.len());
            (
                if self.position >= 10 { "..." } else { "" },
                if end < self.input.len() { "..." } else { "" },
                JsString::from_units(self.input[begin..end].to_vec()).algorithm_string(),
            )
        };
        format!("Unexpected token '{token}', {prefix}\"{snippet}\"{suffix} is not valid JSON")
    }
    fn string(&mut self) -> Result<(), String> {
        self.position += 1;
        loop {
            match self.peek() {
                None => return Err(self.at("Unterminated string")),
                Some(34) => {
                    self.position += 1;
                    return Ok(());
                }
                Some(0..=31) => return Err(self.at("Bad control character in string literal")),
                Some(92) => {
                    self.position += 1;
                    match self.peek() {
                        None => return Err(self.unexpected()),
                        Some(34 | 92 | 47 | 98 | 102 | 110 | 114 | 116) => self.position += 1,
                        Some(117) => {
                            self.position += 1;
                            for _ in 0..4 {
                                if !matches!(self.peek(), Some(48..=57 | 65..=70 | 97..=102)) {
                                    return Err(self.at("Bad Unicode escape"));
                                }
                                self.position += 1;
                            }
                        }
                        _ => return Err(self.at("Bad escaped character")),
                    }
                }
                _ => self.position += 1,
            }
        }
    }
    fn number(&mut self) -> Result<(), String> {
        if self.peek() == Some(45) {
            self.position += 1;
            if !matches!(self.peek(), Some(48..=57)) {
                return Err(self.at("No number after minus sign"));
            }
        }
        if self.peek() == Some(48) {
            self.position += 1;
            if matches!(self.peek(), Some(48..=57)) {
                return Err(self.at("Unexpected number"));
            }
        } else {
            while matches!(self.peek(), Some(48..=57)) {
                self.position += 1;
            }
        }
        if self.peek() == Some(46) {
            self.position += 1;
            if !matches!(self.peek(), Some(48..=57)) {
                return Err(self.at("Unterminated fractional number"));
            }
            while matches!(self.peek(), Some(48..=57)) {
                self.position += 1;
            }
        }
        if matches!(self.peek(), Some(101 | 69)) {
            self.position += 1;
            if matches!(self.peek(), Some(43 | 45)) {
                self.position += 1;
            }
            if !matches!(self.peek(), Some(48..=57)) {
                return Err(self.at("Exponent part is missing a number"));
            }
            while matches!(self.peek(), Some(48..=57)) {
                self.position += 1;
            }
        }
        Ok(())
    }
    fn literal(&mut self, expected: &str) -> Result<(), String> {
        for byte in expected.bytes() {
            if self.peek() != Some(byte as u16) {
                return Err(self.unexpected());
            }
            self.position += 1;
        }
        Ok(())
    }
    fn value(&mut self) -> Result<(), String> {
        enum Frame {
            Value,
            ArrayFirst,
            ArrayAfter,
            ObjectKey(bool),
            ObjectAfter,
        }
        let mut frames = vec![Frame::Value];
        while let Some(frame) = frames.pop() {
            self.whitespace();
            match frame {
                Frame::Value => match self.peek() {
                    Some(34) => self.string()?,
                    Some(45 | 48..=57) => self.number()?,
                    Some(116) => self.literal("true")?,
                    Some(102) => self.literal("false")?,
                    Some(110) => self.literal("null")?,
                    Some(91) => {
                        self.position += 1;
                        frames.push(Frame::ArrayFirst);
                    }
                    Some(123) => {
                        self.position += 1;
                        frames.push(Frame::ObjectKey(true));
                    }
                    _ => return Err(self.unexpected()),
                },
                Frame::ArrayFirst => {
                    if self.peek() == Some(93) {
                        self.position += 1;
                    } else {
                        frames.push(Frame::ArrayAfter);
                        frames.push(Frame::Value);
                    }
                }
                Frame::ArrayAfter => match self.peek() {
                    Some(93) => self.position += 1,
                    Some(44) => {
                        self.position += 1;
                        frames.push(Frame::ArrayAfter);
                        frames.push(Frame::Value);
                    }
                    _ => return Err(self.at("Expected ',' or ']' after array element")),
                },
                Frame::ObjectKey(first) => {
                    if first && self.peek() == Some(125) {
                        self.position += 1;
                        continue;
                    }
                    if self.peek() != Some(34) {
                        return Err(self.at(if first {
                            "Expected property name or '}'"
                        } else {
                            "Expected double-quoted property name"
                        }));
                    }
                    self.string()?;
                    self.whitespace();
                    if self.peek() != Some(58) {
                        return Err(self.at("Expected ':' after property name"));
                    }
                    self.position += 1;
                    frames.push(Frame::ObjectAfter);
                    frames.push(Frame::Value);
                }
                Frame::ObjectAfter => match self.peek() {
                    Some(125) => self.position += 1,
                    Some(44) => {
                        self.position += 1;
                        frames.push(Frame::ObjectKey(false));
                    }
                    _ => return Err(self.at("Expected ',' or '}' after property value")),
                },
            }
        }
        Ok(())
    }
}

fn encoded_error_message(input: &str) -> String {
    let mut parser = Parser {
        input: input.encode_utf16().collect(),
        position: 0,
    };
    if let Err(error) = parser.value() {
        return error;
    }
    parser.whitespace();
    if parser.peek().is_some() {
        parser.at("Unexpected non-whitespace character after JSON")
    } else {
        // Valid JSON rejected by the value parser (e.g. its depth bound).
        // Normal provider inputs use the lossless parser before this path.
        "Unexpected end of JSON input".into()
    }
}

/// V8 may include a lone surrogate in the unexpected-token field itself.
/// Keep that code unit through error redaction, assembly and session JSON.
#[derive(Clone, Debug)]
pub struct ProviderError(pub JsString);
impl std::fmt::Display for ProviderError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.0.fmt(f)
    }
}
impl std::error::Error for ProviderError {}

pub fn error_text(input: &str) -> JsString {
    JsString::from_algorithm_string(&encoded_error_message(input))
}
pub fn error_message(input: &str) -> String {
    error_text(input).to_string()
}
pub fn malformed(prefix: &str, input: &str) -> anyhow::Error {
    ProviderError(error_text(input).prefixed(prefix)).into()
}
pub fn exception_message(error: &anyhow::Error) -> JsString {
    error
        .downcast_ref::<ProviderError>()
        .map(|error| error.0.clone())
        .or_else(|| crate::codex_websocket::exception_message(error))
        .unwrap_or_else(|| error.to_string().into())
}
pub(crate) fn redact(error: anyhow::Error, secrets: &[String]) -> anyhow::Error {
    let text = exception_message(&error);
    let secrets = secrets
        .iter()
        .map(|secret| JsString::from(secret).algorithm_string())
        .collect::<Vec<_>>();
    ProviderError(JsString::from_algorithm_string(
        &crate::provider_http::safe_error_body(&text.algorithm_string(), &secrets),
    ))
    .into()
}
