//! Native port of pinned Pi utils/json-parse.ts and partial-json 0.1.7's
//! default Allow.ALL parser. See THIRD_PARTY_NOTICES.md. No runtime dependency.
use serde_json::{Map, Value, json};

pub fn repair_json(input: &str) -> String {
    let chars: Vec<_> = input.chars().collect();
    let mut output = String::new();
    let mut inside = false;
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i];
        if !inside {
            output.push(c);
            inside = c == '"';
        } else if c == '"' {
            output.push(c);
            inside = false;
        } else if c == '\\' {
            if let Some(&next) = chars.get(i + 1) {
                if next == 'u'
                    && chars
                        .get(i + 2..i + 6)
                        .is_some_and(|v| v.iter().all(char::is_ascii_hexdigit))
                {
                    output.extend(&chars[i..i + 6]);
                    i += 5;
                } else if ['"', '\\', '/', 'b', 'f', 'n', 'r', 't', 'u'].contains(&next) {
                    output.push(c);
                    output.push(next);
                    i += 1;
                } else {
                    output.push_str("\\\\");
                }
            } else {
                output.push_str("\\\\");
            }
        } else if c <= '\u{1f}' {
            let escaped = serde_json::to_string(&c.to_string()).unwrap();
            output.push_str(&escaped[1..escaped.len() - 1]);
        } else {
            output.push(c);
        }
        i += 1;
    }
    output
}

pub fn parse_streaming_json(input: &str) -> Value {
    if input.trim().is_empty() {
        return json!({});
    }
    if let Ok(value) = serde_json::from_str(input) {
        return value;
    }
    let repaired = repair_json(input);
    if repaired != input
        && let Ok(value) = serde_json::from_str(&repaired)
    {
        return value;
    }
    for input in [input, repaired.as_str()] {
        let mut parser = Partial {
            text: input.trim(),
            index: 0,
        };
        if let Ok(value) = parser.any() {
            return if value.is_null() { json!({}) } else { value };
        }
    }
    json!({})
}
struct Partial<'a> {
    text: &'a str,
    index: usize,
}
impl Partial<'_> {
    fn byte(&self) -> Option<u8> {
        self.text.as_bytes().get(self.index).copied()
    }
    fn blank(&mut self) {
        while self.byte().is_some_and(|b| b" \n\r\t".contains(&b)) {
            self.index += 1;
        }
    }
    fn any(&mut self) -> Result<Value, ()> {
        self.blank();
        let Some(b) = self.byte() else {
            return Err(());
        };
        match b {
            b'"' => return self.string().map(Value::String),
            b'{' => return Ok(self.object()),
            b'[' => return Ok(self.array()),
            _ => {}
        }
        let rest = &self.text[self.index..];
        for (literal, value) in [
            ("null", Value::Null),
            ("true", json!(true)),
            ("false", json!(false)),
            ("Infinity", Value::Null),
            ("-Infinity", Value::Null),
            ("NaN", Value::Null),
        ] {
            if rest.starts_with(literal)
                || (literal.starts_with(rest) && (literal != "-Infinity" || rest.len() > 1))
            {
                self.index += literal.len();
                return Ok(value);
            }
        }
        self.number()
    }
    fn string(&mut self) -> Result<String, ()> {
        let start = self.index;
        self.index += 1;
        let mut escape = false;
        while let Some(b) = self.byte() {
            if b == b'"' && !(escape && self.text.as_bytes().get(self.index - 1) == Some(&b'\\')) {
                break;
            }
            escape = if b == b'\\' { !escape } else { false };
            self.index += 1;
        }
        if self.byte() == Some(b'"') {
            self.index += 1;
            return serde_json::from_str(&self.text[start..self.index - usize::from(escape)])
                .map_err(|_| ());
        }
        let end = self.index - usize::from(escape);
        let candidate = format!("{}\"", &self.text[start..end]);
        if let Ok(value) = serde_json::from_str(&candidate) {
            return Ok(value);
        }
        let end = self.text.rfind('\\').unwrap_or(0).max(start);
        serde_json::from_str(&format!("{}\"", &self.text[start..end])).map_err(|_| ())
    }
    fn object(&mut self) -> Value {
        self.index += 1;
        self.blank();
        let mut map = Map::new();
        while self.byte() != Some(b'}') {
            self.blank();
            if self.index >= self.text.len() {
                return Value::Object(map);
            }
            let Ok(key) = self.string() else {
                return Value::Object(map);
            };
            self.blank();
            self.index += 1;
            let Ok(value) = self.any() else {
                return Value::Object(map);
            };
            // JavaScript's object prototype setter does not create an own key.
            if key != "__proto__" {
                map.insert(key, value);
            }
            self.blank();
            if self.byte() == Some(b',') {
                self.index += 1;
            }
        }
        self.index += 1;
        Value::Object(map)
    }
    fn array(&mut self) -> Value {
        self.index += 1;
        let mut list = Vec::new();
        while self.byte() != Some(b']') {
            let Ok(value) = self.any() else {
                return Value::Array(list);
            };
            list.push(value);
            self.blank();
            if self.byte() == Some(b',') {
                self.index += 1;
            }
        }
        self.index += 1;
        Value::Array(list)
    }
    fn number(&mut self) -> Result<Value, ()> {
        let start = self.index;
        if start == 0 {
            self.index = self.text.len();
        } else {
            while self.byte().is_some_and(|b| !b",]}".contains(&b)) {
                self.index += 1;
            }
        }
        if let Ok(value) = serde_json::from_str(&self.text[start..self.index]) {
            return Ok(value);
        }
        let end = self.text.rfind('e').unwrap_or(start).max(start);
        serde_json::from_str(&self.text[start..end]).map_err(|_| ())
    }
}
