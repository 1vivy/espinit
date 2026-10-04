// SPDX-License-Identifier: Apache-2.0
//! Bounded LVM configuration grammar, with LVM's literal (not C) escapes.
use crate::{Error, MAX_DEPTH, MAX_NODES, MAX_TEXT_BYTES, MAX_TOKEN_BYTES};
use std::{borrow::Cow, collections::BTreeMap};

pub type Section<'a> = BTreeMap<Cow<'a, str>, Value<'a>>;
#[derive(Debug, PartialEq, Eq)]
pub enum Value<'a> {
    Section(Section<'a>),
    String(Cow<'a, str>),
    Integer(i128),
    Array(Vec<Value<'a>>),
}
impl<'a> Value<'a> {
    pub fn section(&self) -> Result<&Section<'a>, Error> {
        match self {
            Self::Section(v) => Ok(v),
            _ => Err(Error::Invalid("expected section")),
        }
    }
    pub fn string(&self) -> Result<&str, Error> {
        match self {
            Self::String(v) => Ok(v),
            _ => Err(Error::Invalid("expected string")),
        }
    }
    pub fn unsigned(&self) -> Result<u64, Error> {
        match self {
            Self::Integer(v) => u64::try_from(*v).map_err(|_| Error::Invalid("unsigned integer")),
            _ => Err(Error::Invalid("expected integer")),
        }
    }
    pub fn array(&self) -> Result<&[Value<'a>], Error> {
        match self {
            Self::Array(v) => Ok(v),
            _ => Err(Error::Invalid("expected array")),
        }
    }
}

pub fn parse(input: &str) -> Result<Section<'_>, Error> {
    if input.len() > MAX_TEXT_BYTES {
        return Err(Error::Limit("text bytes"));
    }
    if input.as_bytes().contains(&0) {
        return Err(Error::Invalid("embedded NUL"));
    }
    Parser {
        input,
        cursor: 0,
        nodes: 0,
    }
    .section(0, false)
}

struct Parser<'a> {
    input: &'a str,
    cursor: usize,
    nodes: usize,
}
impl<'a> Parser<'a> {
    fn peek(&self) -> Option<u8> {
        self.input.as_bytes().get(self.cursor).copied()
    }
    fn space(&mut self) {
        loop {
            while self.peek().is_some_and(|b| b.is_ascii_whitespace()) {
                self.cursor += 1;
            }
            if self.peek() != Some(b'#') {
                return;
            }
            while self.peek().is_some_and(|b| b != b'\n') {
                self.cursor += 1;
            }
        }
    }
    fn expect(&mut self, byte: u8) -> Result<(), Error> {
        self.space();
        if self.peek() != Some(byte) {
            return Err(Error::Invalid("text punctuation"));
        }
        self.cursor += 1;
        Ok(())
    }
    fn node(&mut self, depth: usize) -> Result<(), Error> {
        if depth > MAX_DEPTH {
            return Err(Error::Limit("nesting depth"));
        }
        self.nodes += 1;
        if self.nodes > MAX_NODES {
            return Err(Error::Limit("text nodes"));
        }
        Ok(())
    }
    fn token(&mut self) -> Result<&'a str, Error> {
        let start = self.cursor;
        while self
            .peek()
            .is_some_and(|b| !b.is_ascii_whitespace() && !b"#={}[],\"'".contains(&b))
        {
            self.cursor += 1;
        }
        if self.cursor == start {
            return Err(Error::Invalid("text token"));
        }
        if self.cursor - start > MAX_TOKEN_BYTES {
            return Err(Error::Limit("token bytes"));
        }
        Ok(&self.input[start..self.cursor])
    }
    fn section(&mut self, depth: usize, nested: bool) -> Result<Section<'a>, Error> {
        self.node(depth)?;
        let mut entries = BTreeMap::new();
        loop {
            self.space();
            match self.peek() {
                None if !nested => return Ok(entries),
                Some(b'}') if nested => {
                    self.cursor += 1;
                    return Ok(entries);
                }
                None | Some(b'}') => return Err(Error::Invalid("unbalanced section")),
                _ => {}
            }
            self.node(depth)?;
            let key = if matches!(self.peek(), Some(b'"' | b'\'')) {
                self.string()?
            } else {
                Cow::Borrowed(self.token()?)
            };
            self.space();
            let value = if self.peek() == Some(b'{') {
                self.cursor += 1;
                Value::Section(self.section(depth + 1, true)?)
            } else {
                self.expect(b'=')?;
                self.value(depth + 1)?
            };
            if entries.insert(key, value).is_some() {
                return Err(Error::Invalid("duplicate key"));
            }
        }
    }
    fn value(&mut self, depth: usize) -> Result<Value<'a>, Error> {
        self.node(depth)?;
        self.space();
        match self.peek() {
            Some(b'"' | b'\'') => self.string().map(Value::String),
            Some(b'[') => {
                self.cursor += 1;
                let mut values = Vec::new();
                self.space();
                if self.peek() == Some(b']') {
                    self.cursor += 1;
                    return Ok(Value::Array(values));
                }
                loop {
                    values.push(self.value(depth + 1)?);
                    self.space();
                    if self.peek() == Some(b']') {
                        self.cursor += 1;
                        return Ok(Value::Array(values));
                    }
                    self.expect(b',')?;
                    self.space();
                    if self.peek() == Some(b']') {
                        self.cursor += 1;
                        return Ok(Value::Array(values));
                    }
                }
            }
            Some(_) => {
                let token = self.token()?;
                if !token.starts_with(|c: char| c.is_ascii_digit() || "+-.".contains(c)) {
                    return Ok(Value::String(Cow::Borrowed(token)));
                }
                let (negative, unsigned) = if let Some(s) = token.strip_prefix('-') {
                    (true, s)
                } else {
                    (false, token.strip_prefix('+').unwrap_or(token))
                };
                let integer = if unsigned.starts_with('0') && unsigned.len() > 1 {
                    i128::from_str_radix(unsigned, 8)
                } else {
                    unsigned.parse::<i128>()
                }
                .map_err(|_| Error::Invalid("integer"))?;
                let integer = if negative { -integer } else { integer };
                if integer < i128::from(i64::MIN) || integer > i128::from(u64::MAX) {
                    return Err(Error::Invalid("integer range"));
                }
                Ok(Value::Integer(integer))
            }
            None => Err(Error::Invalid("missing value")),
        }
    }
    fn string(&mut self) -> Result<Cow<'a, str>, Error> {
        let quote = self.peek().ok_or(Error::Invalid("string"))?;
        self.cursor += 1;
        let start = self.cursor;
        let mut part = start;
        let mut decoded: Option<String> = None;
        loop {
            if self.cursor - start > MAX_TOKEN_BYTES {
                return Err(Error::Limit("string bytes"));
            }
            match self.peek() {
                Some(b) if b == quote => {
                    let last = &self.input[part..self.cursor];
                    self.cursor += 1;
                    return Ok(match decoded {
                        Some(mut s) => {
                            s.push_str(last);
                            Cow::Owned(s)
                        }
                        None => Cow::Borrowed(last),
                    });
                }
                Some(b'\\') if quote == b'"' => {
                    let next = self.input.as_bytes().get(self.cursor + 1).copied();
                    if let Some(b'\\' | b'"') = next {
                        let s = decoded.get_or_insert_with(String::new);
                        s.push_str(&self.input[part..self.cursor]);
                        s.push(char::from(next.expect("matched escape byte")));
                        self.cursor += 2;
                        part = self.cursor;
                    } else {
                        // LVM only unescapes double quotes and backslashes.
                        self.cursor += 1;
                    }
                }
                Some(_) => self.cursor += 1,
                None => return Err(Error::Invalid("unterminated string")),
            }
        }
    }
}
