//! A small, strict JSON value, parser and writer (RFC 8259), std only.
//!
//! Numbers keep their source text, so a value the proxy passes through
//! (a request id, a tool argument) leaves byte for byte as it came in.
//! Objects keep their key order.

use std::fmt;

const MAX_DEPTH: usize = 256;

#[derive(Clone, Debug, PartialEq)]
pub enum Value {
    Null,
    Bool(bool),
    /// The number exactly as written in the source (or by `Value::int`).
    Num(String),
    Str(String),
    Array(Vec<Value>),
    Object(Vec<(String, Value)>),
}

impl Value {
    pub fn str(s: impl Into<String>) -> Value {
        Value::Str(s.into())
    }

    pub fn int(n: i64) -> Value {
        Value::Num(n.to_string())
    }

    /// An object from pairs. A repeated key keeps one entry with the last
    /// value, as parsing does, so `get`, `set` and `remove` agree.
    pub fn obj(pairs: Vec<(&str, Value)>) -> Value {
        let mut out: Vec<(String, Value)> = Vec::with_capacity(pairs.len());
        for (k, v) in pairs {
            match out.iter_mut().find(|(e, _)| e == k) {
                Some(slot) => slot.1 = v,
                None => out.push((k.to_string(), v)),
            }
        }
        Value::Object(out)
    }

    pub fn get(&self, key: &str) -> Option<&Value> {
        match self {
            Value::Object(pairs) => pairs.iter().rev().find(|(k, _)| k == key).map(|(_, v)| v),
            _ => None,
        }
    }

    pub fn get_mut(&mut self, key: &str) -> Option<&mut Value> {
        match self {
            Value::Object(pairs) => pairs
                .iter_mut()
                .rev()
                .find(|(k, _)| k == key)
                .map(|(_, v)| v),
            _ => None,
        }
    }

    /// Set `key`, replacing an existing entry in place. No-op on non-objects.
    pub fn set(&mut self, key: &str, value: Value) {
        if let Value::Object(pairs) = self {
            if let Some(slot) = pairs.iter_mut().find(|(k, _)| k == key) {
                slot.1 = value;
            } else {
                pairs.push((key.to_string(), value));
            }
        }
    }

    pub fn remove(&mut self, key: &str) -> Option<Value> {
        if let Value::Object(pairs) = self {
            let i = pairs.iter().position(|(k, _)| k == key)?;
            return Some(pairs.remove(i).1);
        }
        None
    }

    pub fn as_str(&self) -> Option<&str> {
        match self {
            Value::Str(s) => Some(s),
            _ => None,
        }
    }

    pub fn as_bool(&self) -> Option<bool> {
        match self {
            Value::Bool(b) => Some(*b),
            _ => None,
        }
    }

    pub fn as_i64(&self) -> Option<i64> {
        match self {
            Value::Num(n) => n.parse().ok(),
            _ => None,
        }
    }

    pub fn as_array(&self) -> Option<&Vec<Value>> {
        match self {
            Value::Array(a) => Some(a),
            _ => None,
        }
    }

    pub fn as_object(&self) -> Option<&Vec<(String, Value)>> {
        match self {
            Value::Object(o) => Some(o),
            _ => None,
        }
    }

    pub fn is_null(&self) -> bool {
        matches!(self, Value::Null)
    }

    pub fn to_json(&self) -> String {
        let mut out = String::new();
        write_value(self, &mut out);
        out
    }
}

impl fmt::Display for Value {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.to_json())
    }
}

fn write_value(v: &Value, out: &mut String) {
    match v {
        Value::Null => out.push_str("null"),
        Value::Bool(true) => out.push_str("true"),
        Value::Bool(false) => out.push_str("false"),
        Value::Num(n) => out.push_str(n),
        Value::Str(s) => write_string(s, out),
        Value::Array(items) => {
            out.push('[');
            for (i, item) in items.iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                write_value(item, out);
            }
            out.push(']');
        }
        Value::Object(pairs) => {
            out.push('{');
            for (i, (k, val)) in pairs.iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                write_string(k, out);
                out.push(':');
                write_value(val, out);
            }
            out.push('}');
        }
    }
}

pub fn write_string(s: &str, out: &mut String) {
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('"');
}

#[derive(Debug, PartialEq)]
pub struct ParseError {
    pub at: usize,
    pub msg: &'static str,
}

impl fmt::Display for ParseError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{} at byte {}", self.msg, self.at)
    }
}

impl std::error::Error for ParseError {}

/// The top-level `"id"` of a JSON-RPC message that does not parse as a
/// whole (too deep, a bad escape elsewhere), so the error can still be
/// sent for the right request. Only a number or string id is returned; a
/// scan that cannot follow the text returns `None`. Iterative: no depth
/// limit is needed.
pub fn find_id(text: &str) -> Option<Value> {
    let b = text.strip_prefix('\u{feff}').unwrap_or(text).as_bytes();
    let ws = |i: &mut usize| {
        while matches!(b.get(*i), Some(b' ' | b'\t' | b'\r' | b'\n')) {
            *i += 1;
        }
    };
    // From an opening quote, the index just past the closing one.
    let string_end = |mut i: usize| -> Option<usize> {
        i += 1;
        loop {
            match *b.get(i)? {
                b'"' => return Some(i + 1),
                b'\\' => i += 2,
                _ => i += 1,
            }
        }
    };
    let mut i = 0;
    ws(&mut i);
    if b.get(i) != Some(&b'{') {
        return None;
    }
    i += 1;
    loop {
        ws(&mut i);
        if b.get(i) != Some(&b'"') {
            return None;
        }
        let key_end = string_end(i)?;
        let key = &b[i..key_end];
        i = key_end;
        ws(&mut i);
        if b.get(i) != Some(&b':') {
            return None;
        }
        i += 1;
        ws(&mut i);
        let start = i;
        // Skip the value: nested brackets and strings, to the next
        // top-level `,` or `}`.
        let mut depth = 0usize;
        loop {
            match *b.get(i)? {
                b'"' => i = string_end(i)?,
                b'{' | b'[' => {
                    depth += 1;
                    i += 1;
                }
                b'}' | b']' if depth > 0 => {
                    depth -= 1;
                    i += 1;
                }
                b',' | b'}' if depth == 0 => break,
                _ => i += 1,
            }
        }
        if key == b"\"id\"" {
            let v = parse(std::str::from_utf8(&b[start..i]).ok()?.trim()).ok()?;
            return matches!(v, Value::Num(_) | Value::Str(_)).then_some(v);
        }
        if b[i] == b'}' {
            return None;
        }
        i += 1;
    }
}

/// Parse one JSON text. A leading byte order mark is skipped: Windows
/// editors and PowerShell write one. Error offsets count from after it.
pub fn parse(text: &str) -> Result<Value, ParseError> {
    let text = text.strip_prefix('\u{feff}').unwrap_or(text);
    let mut p = Parser {
        b: text.as_bytes(),
        i: 0,
    };
    p.ws();
    let v = p.value(0)?;
    p.ws();
    if p.i != p.b.len() {
        return Err(p.err("trailing characters"));
    }
    Ok(v)
}

struct Parser<'a> {
    b: &'a [u8],
    i: usize,
}

impl Parser<'_> {
    fn err(&self, msg: &'static str) -> ParseError {
        ParseError { at: self.i, msg }
    }

    fn ws(&mut self) {
        while let Some(c) = self.b.get(self.i) {
            if matches!(c, b' ' | b'\t' | b'\n' | b'\r') {
                self.i += 1;
            } else {
                break;
            }
        }
    }

    fn lit(&mut self, word: &[u8], v: Value) -> Result<Value, ParseError> {
        if self.b[self.i..].starts_with(word) {
            self.i += word.len();
            Ok(v)
        } else {
            Err(self.err("unexpected character"))
        }
    }

    fn value(&mut self, depth: usize) -> Result<Value, ParseError> {
        if depth > MAX_DEPTH {
            return Err(self.err("nesting too deep"));
        }
        match self.b.get(self.i) {
            None => Err(self.err("unexpected end")),
            Some(b'n') => self.lit(b"null", Value::Null),
            Some(b't') => self.lit(b"true", Value::Bool(true)),
            Some(b'f') => self.lit(b"false", Value::Bool(false)),
            Some(b'"') => Ok(Value::Str(self.string()?)),
            Some(b'[') => self.array(depth),
            Some(b'{') => self.object(depth),
            Some(c) if *c == b'-' || c.is_ascii_digit() => self.number(),
            Some(_) => Err(self.err("unexpected character")),
        }
    }

    fn number(&mut self) -> Result<Value, ParseError> {
        let start = self.i;
        if self.b.get(self.i) == Some(&b'-') {
            self.i += 1;
        }
        match self.b.get(self.i) {
            Some(b'0') => self.i += 1,
            Some(c) if c.is_ascii_digit() => self.digits(),
            _ => return Err(self.err("bad number")),
        }
        if self.b.get(self.i) == Some(&b'.') {
            self.i += 1;
            if !self.b.get(self.i).is_some_and(u8::is_ascii_digit) {
                return Err(self.err("bad number"));
            }
            self.digits();
        }
        if matches!(self.b.get(self.i), Some(b'e' | b'E')) {
            self.i += 1;
            if matches!(self.b.get(self.i), Some(b'+' | b'-')) {
                self.i += 1;
            }
            if !self.b.get(self.i).is_some_and(u8::is_ascii_digit) {
                return Err(self.err("bad number"));
            }
            self.digits();
        }
        let text =
            std::str::from_utf8(&self.b[start..self.i]).map_err(|_| self.err("bad number"))?;
        Ok(Value::Num(text.to_string()))
    }

    fn digits(&mut self) {
        while self.b.get(self.i).is_some_and(u8::is_ascii_digit) {
            self.i += 1;
        }
    }

    fn hex4(&mut self) -> Result<u32, ParseError> {
        let s = self
            .b
            .get(self.i..self.i + 4)
            .ok_or_else(|| self.err("bad escape"))?;
        let s = std::str::from_utf8(s).map_err(|_| self.err("bad escape"))?;
        if !s.bytes().all(|b| b.is_ascii_hexdigit()) {
            return Err(self.err("bad escape"));
        }
        let n = u32::from_str_radix(s, 16).map_err(|_| self.err("bad escape"))?;
        self.i += 4;
        Ok(n)
    }

    fn string(&mut self) -> Result<String, ParseError> {
        self.i += 1; // opening quote
        let mut out = String::new();
        loop {
            let start = self.i;
            while let Some(&c) = self.b.get(self.i) {
                if c == b'"' || c == b'\\' || c < 0x20 {
                    break;
                }
                self.i += 1;
            }
            out.push_str(
                std::str::from_utf8(&self.b[start..self.i])
                    .map_err(|_| self.err("invalid UTF-8"))?,
            );
            match self.b.get(self.i) {
                None => return Err(self.err("unterminated string")),
                Some(b'"') => {
                    self.i += 1;
                    return Ok(out);
                }
                Some(b'\\') => {
                    self.i += 1;
                    let esc = *self.b.get(self.i).ok_or_else(|| self.err("bad escape"))?;
                    self.i += 1;
                    match esc {
                        b'"' => out.push('"'),
                        b'\\' => out.push('\\'),
                        b'/' => out.push('/'),
                        b'b' => out.push('\u{8}'),
                        b'f' => out.push('\u{c}'),
                        b'n' => out.push('\n'),
                        b'r' => out.push('\r'),
                        b't' => out.push('\t'),
                        b'u' => {
                            let hi = self.hex4()?;
                            // A lone surrogate becomes U+FFFD, as with
                            // JavaScript's TextEncoder: JSON.stringify writes
                            // one for a string cut inside a surrogate pair,
                            // and refusing the whole message would lose it.
                            let code = if (0xD800..0xDC00).contains(&hi) {
                                match self.low_surrogate()? {
                                    Some(lo) => 0x10000 + ((hi - 0xD800) << 10) + (lo - 0xDC00),
                                    None => 0xFFFD,
                                }
                            } else if (0xDC00..0xE000).contains(&hi) {
                                0xFFFD
                            } else {
                                hi
                            };
                            out.push(char::from_u32(code).ok_or_else(|| self.err("bad escape"))?);
                        }
                        _ => return Err(self.err("bad escape")),
                    }
                }
                Some(_) => return Err(self.err("control character in string")),
            }
        }
    }

    /// After a high surrogate: the low surrogate escape that follows it,
    /// consumed, or `None` (nothing consumed) when there is none.
    fn low_surrogate(&mut self) -> Result<Option<u32>, ParseError> {
        if !self.b[self.i..].starts_with(b"\\u") {
            return Ok(None);
        }
        let at = self.i;
        self.i += 2;
        let lo = self.hex4()?;
        if (0xDC00..0xE000).contains(&lo) {
            Ok(Some(lo))
        } else {
            self.i = at;
            Ok(None)
        }
    }

    fn array(&mut self, depth: usize) -> Result<Value, ParseError> {
        self.i += 1;
        let mut items = Vec::new();
        self.ws();
        if self.b.get(self.i) == Some(&b']') {
            self.i += 1;
            return Ok(Value::Array(items));
        }
        loop {
            self.ws();
            items.push(self.value(depth + 1)?);
            self.ws();
            match self.b.get(self.i) {
                Some(b',') => self.i += 1,
                Some(b']') => {
                    self.i += 1;
                    return Ok(Value::Array(items));
                }
                _ => return Err(self.err("expected , or ]")),
            }
        }
    }

    fn object(&mut self, depth: usize) -> Result<Value, ParseError> {
        self.i += 1;
        let mut pairs: Vec<(String, Value)> = Vec::new();
        let mut index: std::collections::HashMap<String, usize> = std::collections::HashMap::new();
        self.ws();
        if self.b.get(self.i) == Some(&b'}') {
            self.i += 1;
            return Ok(Value::Object(pairs));
        }
        loop {
            self.ws();
            if self.b.get(self.i) != Some(&b'"') {
                return Err(self.err("expected a key"));
            }
            let key = self.string()?;
            self.ws();
            if self.b.get(self.i) != Some(&b':') {
                return Err(self.err("expected :"));
            }
            self.i += 1;
            self.ws();
            let v = self.value(depth + 1)?;
            // A repeated key keeps one entry with the last value, so `get`,
            // `set` and `remove` all see the same thing.
            match index.get(&key) {
                Some(&at) => pairs[at].1 = v,
                None => {
                    index.insert(key.clone(), pairs.len());
                    pairs.push((key, v));
                }
            }
            self.ws();
            match self.b.get(self.i) {
                Some(b',') => self.i += 1,
                Some(b'}') => {
                    self.i += 1;
                    return Ok(Value::Object(pairs));
                }
                _ => return Err(self.err("expected , or }")),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn repeated_keys_keep_one_entry_with_the_last_value() {
        let mut v = parse(r#"{"name":"a","x":1,"name":"b"}"#).unwrap();
        assert_eq!(v.to_json(), r#"{"name":"b","x":1}"#);
        v.set("name", Value::str("c"));
        assert_eq!(v.get("name").and_then(Value::as_str), Some("c"));
        v.remove("name");
        assert!(v.get("name").is_none());
    }

    #[test]
    fn obj_with_a_repeated_key_keeps_the_last() {
        let mut v = Value::obj(vec![("a", Value::int(1)), ("a", Value::int(2))]);
        assert_eq!(v.to_json(), r#"{"a":2}"#);
        v.set("a", Value::int(3));
        assert_eq!(v.get("a").and_then(Value::as_i64), Some(3));
    }

    #[test]
    fn unicode_escapes_take_four_hex_digits_only() {
        assert!(parse(r#""\u+041""#).is_err());
        assert!(parse(r#""\u-041""#).is_err());
        assert_eq!(parse(r#""\u0041""#).unwrap(), Value::str("A"));
    }

    #[test]
    fn round_trips_and_keeps_number_text() {
        let src = r#"{"id":12345678901234567890,"f":1.50e+3,"s":"a\"b\\c\n","a":[true,false,null],"o":{}}"#;
        let v = parse(src).unwrap();
        assert_eq!(v.to_json(), src);
        assert_eq!(
            v.get("id").unwrap(),
            &Value::Num("12345678901234567890".into())
        );
    }

    #[test]
    fn decodes_surrogate_pairs() {
        assert_eq!(parse(r#""😀""#).unwrap(), Value::str("😀"));
        assert_eq!(parse(r#""\ud83d""#).unwrap(), Value::str("\u{fffd}"));
        assert_eq!(parse(r#""a\udc00b""#).unwrap(), Value::str("a\u{fffd}b"));
        // A high surrogate followed by an escape that is not a low one.
        assert_eq!(parse(r#""\ud83dA""#).unwrap(), Value::str("\u{fffd}A"));
        assert_eq!(parse(r#""\ud83d😀""#).unwrap(), Value::str("\u{fffd}😀"));
        assert!(parse(r#""\ud83d\u00""#).is_err());
    }

    #[test]
    fn finds_the_id_of_a_message_that_does_not_parse() {
        let deep = format!(
            r#"{{"jsonrpc":"2.0","method":"tools/call","params":{}{},"id":3}}"#,
            "[".repeat(300),
            "]".repeat(300)
        );
        assert!(parse(&deep).is_err());
        assert_eq!(find_id(&deep), Some(Value::int(3)));
        assert_eq!(
            find_id(r#"{"id":"a,}\"b","params":{"x":"\q"}}"#),
            Some(Value::str("a,}\"b"))
        );
        assert_eq!(
            find_id(r#"{"params":{"id":1},"id":7,"#),
            Some(Value::int(7))
        );
        assert_eq!(find_id(r#"{"params":{"id":1}}"#), None);
        assert_eq!(find_id(r#"{"id":null}"#), None);
        assert_eq!(find_id(r#"{"id":{"x":1}}"#), None);
        assert_eq!(find_id("[1]"), None);
        assert_eq!(find_id(r#"{"id":"#), None);
        assert_eq!(find_id("\u{feff}{\"id\":2}"), Some(Value::int(2)));
    }

    #[test]
    fn skips_a_byte_order_mark() {
        assert_eq!(parse("\u{feff}{\"a\":1}").unwrap().to_json(), r#"{"a":1}"#);
        assert!(parse("{}\u{feff}").is_err());
    }

    #[test]
    fn rejects_bad_input() {
        for bad in [
            "",
            "01",
            "1.",
            "-",
            "[1,]",
            "{\"a\"}",
            "\"\u{1}\"",
            "nul",
            "{} x",
            "NaN",
        ] {
            assert!(parse(bad).is_err(), "accepted {bad:?}");
        }
    }

    #[test]
    fn limits_depth() {
        let deep = "[".repeat(300) + &"]".repeat(300);
        assert!(parse(&deep).is_err());
    }

    #[test]
    fn set_replaces_in_place() {
        let mut v = parse(r#"{"a":1,"b":2}"#).unwrap();
        v.set("a", Value::int(3));
        v.set("c", Value::Null);
        assert_eq!(v.to_json(), r#"{"a":3,"b":2,"c":null}"#);
    }
}
