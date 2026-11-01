//! Tokens of the OData expression syntax.

use crate::UriError;
use odatabase_edm::{PrimitiveKind, Value, parse_duration};
use rust_decimal::Decimal;
use std::str::FromStr;

#[derive(Clone, Debug, PartialEq)]
pub enum Token {
    /// A name, possibly dotted. `$`-prefixed names such as `$it` and `$count` are included.
    Ident(String),
    /// `@name` parameter alias.
    Alias(String),
    /// Self-typed literal.
    Literal(Value),
    /// `Namespace.Type'Member'`, resolved against the model by the parser.
    TypedLiteral(String, String),
    LParen,
    RParen,
    Comma,
    Slash,
    Colon,
    Equals,
    Semicolon,
    Minus,
    Star,
}

pub fn tokenize(input: &str) -> Result<Vec<Token>, UriError> {
    Lexer {
        s: input.as_bytes(),
        text: input,
        i: 0,
        out: Vec::new(),
    }
    .run()
}

struct Lexer<'a> {
    s: &'a [u8],
    text: &'a str,
    i: usize,
    out: Vec<Token>,
}

fn is_name_start(c: u8) -> bool {
    c.is_ascii_alphabetic() || c == b'_' || c == b'$' || c >= 0x80
}

fn is_name_char(c: u8) -> bool {
    c.is_ascii_alphanumeric() || c == b'_' || c == b'.' || c >= 0x80
}

impl Lexer<'_> {
    fn peek(&self, offset: usize) -> Option<u8> {
        self.s.get(self.i + offset).copied()
    }

    fn run(mut self) -> Result<Vec<Token>, UriError> {
        while let Some(c) = self.peek(0) {
            match c {
                b' ' | b'\t' => self.i += 1,
                // Form encoders write spaces as `+`. Signs are handled before this arm.
                b'+' if !self.number_follows() => self.i += 1,
                b'(' => self.single(Token::LParen),
                b')' => self.single(Token::RParen),
                b',' => self.single(Token::Comma),
                b'/' => self.single(Token::Slash),
                b':' => self.single(Token::Colon),
                b'=' => self.single(Token::Equals),
                b';' => self.single(Token::Semicolon),
                b'*' => self.single(Token::Star),
                b'\'' => {
                    let v = self.quoted()?;
                    self.out.push(Token::Literal(Value::String(v)));
                }
                b'@' => {
                    self.i += 1;
                    let name = self.name();
                    if name.is_empty() {
                        return Err(UriError::bad_request("expected alias name after @"));
                    }
                    self.out.push(Token::Alias(name));
                }
                b'-' | b'+' if self.number_follows() => self.number()?,
                b'-' if self.keyword_float() => self.number()?,
                b'-' => self.single(Token::Minus),
                b'0'..=b'9' => {
                    if self.guid().is_none() {
                        self.date_or_number()?;
                    }
                }
                c if is_name_start(c) => {
                    if self.guid().is_none() {
                        self.word()?;
                    }
                }
                c => {
                    return Err(UriError::bad_request(format!(
                        "unexpected character '{}'",
                        c as char
                    )));
                }
            }
        }
        Ok(self.out)
    }

    fn single(&mut self, t: Token) {
        self.i += 1;
        self.out.push(t);
    }

    fn number_follows(&self) -> bool {
        self.peek(1).is_some_and(|c| c.is_ascii_digit())
    }

    fn keyword_float(&self) -> bool {
        self.text[self.i + 1..].starts_with("INF")
    }

    fn name(&mut self) -> String {
        let start = self.i;
        while self.peek(0).is_some_and(is_name_char) {
            self.i += 1;
        }
        self.text[start..self.i].to_owned()
    }

    fn quoted(&mut self) -> Result<String, UriError> {
        self.i += 1;
        let mut out = String::new();
        let mut start = self.i;
        loop {
            match self.peek(0) {
                None => return Err(UriError::bad_request("unterminated string")),
                Some(b'\'') if self.peek(1) == Some(b'\'') => {
                    out.push_str(&self.text[start..=self.i]);
                    self.i += 2;
                    start = self.i;
                }
                Some(b'\'') => {
                    out.push_str(&self.text[start..self.i]);
                    self.i += 1;
                    return Ok(out);
                }
                Some(_) => self.i += 1,
            }
        }
    }

    fn word(&mut self) -> Result<(), UriError> {
        let name = {
            let start = self.i;
            self.i += 1;
            while self.peek(0).is_some_and(is_name_char) {
                self.i += 1;
            }
            self.text[start..self.i].to_owned()
        };
        if self.peek(0) == Some(b'\'') {
            let body = self.quoted()?;
            let token = match name.as_str() {
                "duration" | "Edm.Duration" => Token::Literal(Value::Duration(
                    parse_duration(&body)
                        .ok_or_else(|| UriError::bad_request("invalid duration"))?,
                )),
                "binary" | "Edm.Binary" => Token::Literal(
                    Value::parse_text(PrimitiveKind::Binary, &body)
                        .map_err(|e| UriError::bad_request(e.to_string()))?,
                ),
                _ => Token::TypedLiteral(name, body),
            };
            self.out.push(token);
            return Ok(());
        }
        let token = match name.as_str() {
            "true" => Token::Literal(Value::Boolean(true)),
            "false" => Token::Literal(Value::Boolean(false)),
            "null" => Token::Literal(Value::Null),
            "NaN" => Token::Literal(Value::Double(f64::NAN)),
            "INF" => Token::Literal(Value::Double(f64::INFINITY)),
            _ => Token::Ident(name),
        };
        self.out.push(token);
        Ok(())
    }

    /// Consumes a GUID at the current position, recognised by its 8-4-4-4-12 shape.
    fn guid(&mut self) -> Option<()> {
        const GROUPS: [usize; 5] = [8, 4, 4, 4, 12];
        let mut at = self.i;
        for (i, len) in GROUPS.iter().enumerate() {
            let group = self.s.get(at..at + len)?;
            if !group.iter().all(u8::is_ascii_hexdigit) {
                return None;
            }
            at += len;
            if i < 4 {
                if self.s.get(at) != Some(&b'-') {
                    return None;
                }
                at += 1;
            }
        }
        if self.s.get(at).copied().is_some_and(is_name_char) {
            return None;
        }
        let value = Value::parse_text(PrimitiveKind::Guid, &self.text[self.i..at]).ok()?;
        self.i = at;
        self.out.push(Token::Literal(value));
        Some(())
    }

    fn date_or_number(&mut self) -> Result<(), UriError> {
        let rest = &self.s[self.i..];
        let digits = rest.iter().take_while(|c| c.is_ascii_digit()).count();
        if digits == 4 && rest.get(4) == Some(&b'-') {
            return self.temporal();
        }
        if digits == 2 && rest.get(2) == Some(&b':') {
            let len = rest
                .iter()
                .take_while(|c| c.is_ascii_digit() || **c == b':' || **c == b'.')
                .count();
            let text = &self.text[self.i..self.i + len];
            let v = Value::parse_text(PrimitiveKind::TimeOfDay, text)
                .map_err(|e| UriError::bad_request(e.to_string()))?;
            self.i += len;
            self.out.push(Token::Literal(v));
            return Ok(());
        }
        self.number()
    }

    fn temporal(&mut self) -> Result<(), UriError> {
        let rest = &self.s[self.i..];
        let len = rest
            .iter()
            .take_while(|c| {
                c.is_ascii_digit() || matches!(**c, b'-' | b':' | b'.' | b'T' | b'Z' | b'+')
            })
            .count();
        let text = &self.text[self.i..self.i + len];
        let kind = if text.contains('T') {
            PrimitiveKind::DateTimeOffset
        } else {
            PrimitiveKind::Date
        };
        let v = Value::parse_text(kind, text).map_err(|e| UriError::bad_request(e.to_string()))?;
        self.i += len;
        self.out.push(Token::Literal(v));
        Ok(())
    }

    fn number(&mut self) -> Result<(), UriError> {
        let start = self.i;
        if matches!(self.peek(0), Some(b'-' | b'+')) {
            self.i += 1;
        }
        if self.text[self.i..].starts_with("INF") {
            self.i += 3;
            let negative = self.s[start] == b'-';
            let v = if negative {
                f64::NEG_INFINITY
            } else {
                f64::INFINITY
            };
            self.out.push(Token::Literal(Value::Double(v)));
            return Ok(());
        }
        while self.peek(0).is_some_and(|c| c.is_ascii_digit()) {
            self.i += 1;
        }
        let mut is_int = true;
        if self.peek(0) == Some(b'.') && self.peek(1).is_some_and(|c| c.is_ascii_digit()) {
            is_int = false;
            self.i += 1;
            while self.peek(0).is_some_and(|c| c.is_ascii_digit()) {
                self.i += 1;
            }
        }
        let mut is_float = false;
        if matches!(self.peek(0), Some(b'e' | b'E')) {
            is_float = true;
            self.i += 1;
            if matches!(self.peek(0), Some(b'-' | b'+')) {
                self.i += 1;
            }
            while self.peek(0).is_some_and(|c| c.is_ascii_digit()) {
                self.i += 1;
            }
        }
        let text = self.text[start..self.i].trim_start_matches('+');
        let bad = || UriError::bad_request(format!("invalid number {text}"));
        let value = if is_float {
            Value::Double(text.parse().map_err(|_| bad())?)
        } else if is_int {
            match text.parse::<i64>() {
                Ok(v) => i32::try_from(v).map_or(Value::Int64(v), Value::Int32),
                Err(_) => Value::Decimal(Decimal::from_str(text).map_err(|_| bad())?),
            }
        } else {
            Value::Decimal(Decimal::from_str(text).map_err(|_| bad())?)
        };
        self.out.push(Token::Literal(value));
        Ok(())
    }
}
