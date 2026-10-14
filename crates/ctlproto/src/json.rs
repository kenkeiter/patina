//! Minimal, allocation-free JSON reader.
//!
//! [`parse`] validates a complete JSON text and returns a borrowed
//! [`Value`]. Containers are kept as validated raw slices and walked lazily
//! ([`Object::get`], [`Object::iter`], [`Array::iter`]); strings are kept
//! raw and decoded on demand ([`Str::chars`], [`Str::decode_into`],
//! [`Str::eq_str`]). Numbers are kept raw ([`Number::as_i64`],
//! [`Number::as_u64`]).
//!
//! Nesting depth is capped at [`MAX_DEPTH`] so hostile input cannot
//! exhaust the (small, kernel) stack. Nothing in this module panics on any
//! input.

/// Maximum container nesting accepted by [`parse`].
pub const MAX_DEPTH: usize = 32;

/// Parse error kinds. `pos` in [`Error`] is a byte offset into the input.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ErrorKind {
    /// Input is not valid UTF-8.
    InvalidUtf8,
    /// Input ended in the middle of a value.
    UnexpectedEof,
    /// A byte that cannot start/continue the current construct.
    UnexpectedByte(u8),
    /// Bad `\` escape or `\u` sequence (including unpaired surrogates).
    BadEscape,
    /// Raw control character inside a string.
    ControlInString,
    /// Malformed number.
    BadNumber,
    /// Nesting deeper than [`MAX_DEPTH`].
    TooDeep,
    /// Non-whitespace after the top-level value.
    TrailingData,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Error {
    pub kind: ErrorKind,
    pub pos: usize,
}

impl core::fmt::Display for Error {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(f, "{:?} at byte {}", self.kind, self.pos)
    }
}

/// A JSON value borrowing from the input.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Value<'a> {
    Null,
    Bool(bool),
    Number(Number<'a>),
    String(Str<'a>),
    Array(Array<'a>),
    Object(Object<'a>),
}

impl<'a> Value<'a> {
    pub fn as_object(&self) -> Option<Object<'a>> {
        match *self {
            Value::Object(o) => Some(o),
            _ => None,
        }
    }
    pub fn as_str(&self) -> Option<Str<'a>> {
        match *self {
            Value::String(s) => Some(s),
            _ => None,
        }
    }
    pub fn as_i64(&self) -> Option<i64> {
        match self {
            Value::Number(n) => n.as_i64(),
            _ => None,
        }
    }
    pub fn as_u64(&self) -> Option<u64> {
        match self {
            Value::Number(n) => n.as_u64(),
            _ => None,
        }
    }
    pub fn as_bool(&self) -> Option<bool> {
        match *self {
            Value::Bool(b) => Some(b),
            _ => None,
        }
    }
    pub fn is_null(&self) -> bool {
        matches!(self, Value::Null)
    }
}

/// A validated JSON number, kept as its source text.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Number<'a>(&'a str);

impl<'a> Number<'a> {
    pub fn raw(&self) -> &'a str {
        self.0
    }
    fn is_integer(&self) -> bool {
        !self.0.bytes().any(|b| matches!(b, b'.' | b'e' | b'E'))
    }
    /// The value if it is an integer literal that fits in `i64`.
    pub fn as_i64(&self) -> Option<i64> {
        if self.is_integer() {
            self.0.parse().ok()
        } else {
            None
        }
    }
    /// The value if it is a non-negative integer literal that fits in `u64`.
    pub fn as_u64(&self) -> Option<u64> {
        if self.is_integer() && !self.0.starts_with('-') {
            self.0.parse().ok()
        } else {
            None
        }
    }
}

/// A validated JSON string, kept as the raw text between the quotes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Str<'a>(&'a str);

impl<'a> Str<'a> {
    /// Raw (still escaped) contents.
    pub fn raw(&self) -> &'a str {
        self.0
    }

    /// The contents without allocation when no escapes are present.
    pub fn as_unescaped(&self) -> Option<&'a str> {
        if self.0.contains('\\') {
            None
        } else {
            Some(self.0)
        }
    }

    /// Iterate the decoded characters.
    pub fn chars(&self) -> StrChars<'a> {
        StrChars {
            s: self.0.as_bytes(),
            raw: self.0,
            pos: 0,
        }
    }

    /// Compare the decoded contents with `s`.
    pub fn eq_str(&self, s: &str) -> bool {
        if let Some(u) = self.as_unescaped() {
            return u == s;
        }
        self.chars().eq(s.chars())
    }

    /// Decode into `buf`, returning the decoded `&str`, or `None` if it
    /// does not fit.
    pub fn decode_into<'b>(&self, buf: &'b mut [u8]) -> Option<&'b str> {
        let mut n = 0;
        for c in self.chars() {
            let l = c.len_utf8();
            if n + l > buf.len() {
                return None;
            }
            c.encode_utf8(&mut buf[n..n + l]);
            n += l;
        }
        core::str::from_utf8(&buf[..n]).ok()
    }
}

/// Decoded characters of a [`Str`].
#[derive(Clone, Debug)]
pub struct StrChars<'a> {
    s: &'a [u8],
    raw: &'a str,
    pos: usize,
}

fn hex4(b: &[u8]) -> Option<u16> {
    if b.len() < 4 {
        return None;
    }
    let mut v = 0u16;
    for &c in &b[..4] {
        let d = match c {
            b'0'..=b'9' => c - b'0',
            b'a'..=b'f' => c - b'a' + 10,
            b'A'..=b'F' => c - b'A' + 10,
            _ => return None,
        };
        v = (v << 4) | d as u16;
    }
    Some(v)
}

/// Decode one escape sequence starting just after the backslash. Returns
/// the char and the number of bytes consumed after the backslash.
fn decode_escape(b: &[u8]) -> Option<(char, usize)> {
    let c = *b.first()?;
    let simple = match c {
        b'"' => '"',
        b'\\' => '\\',
        b'/' => '/',
        b'b' => '\u{8}',
        b'f' => '\u{c}',
        b'n' => '\n',
        b'r' => '\r',
        b't' => '\t',
        b'u' => {
            let hi = hex4(&b[1..])?;
            if (0xD800..0xDC00).contains(&hi) {
                // High surrogate: must be followed by \uDC00..DFFF.
                if b.len() < 11 || b[5] != b'\\' || b[6] != b'u' {
                    return None;
                }
                let lo = hex4(&b[7..])?;
                if !(0xDC00..0xE000).contains(&lo) {
                    return None;
                }
                let cp = 0x10000 + (((hi as u32) - 0xD800) << 10) + ((lo as u32) - 0xDC00);
                return char::from_u32(cp).map(|ch| (ch, 11));
            }
            if (0xDC00..0xE000).contains(&hi) {
                return None;
            }
            return char::from_u32(hi as u32).map(|ch| (ch, 5));
        }
        _ => return None,
    };
    Some((simple, 1))
}

impl Iterator for StrChars<'_> {
    type Item = char;

    fn next(&mut self) -> Option<char> {
        let b = *self.s.get(self.pos)?;
        if b == b'\\' {
            // Validated at parse time; on corruption, stop.
            let (c, n) = decode_escape(&self.s[self.pos + 1..])?;
            self.pos += 1 + n;
            Some(c)
        } else {
            let c = self.raw.get(self.pos..)?.chars().next()?;
            self.pos += c.len_utf8();
            Some(c)
        }
    }
}

/// A validated JSON array (raw text including brackets).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Array<'a>(&'a str);

impl<'a> Array<'a> {
    pub fn raw(&self) -> &'a str {
        self.0
    }
    pub fn iter(&self) -> ArrayIter<'a> {
        ArrayIter {
            p: Parser::new(self.0),
            started: false,
            done: false,
        }
    }
}

/// Iterator over array elements.
#[derive(Clone, Debug)]
pub struct ArrayIter<'a> {
    p: Parser<'a>,
    started: bool,
    done: bool,
}

impl<'a> Iterator for ArrayIter<'a> {
    type Item = Value<'a>;

    fn next(&mut self) -> Option<Value<'a>> {
        if self.done {
            return None;
        }
        let r = (|| {
            if !self.started {
                self.started = true;
                self.p.expect(b'[')?;
                self.p.ws();
                if self.p.peek() == Some(b']') {
                    return Ok(None);
                }
            } else {
                self.p.ws();
                match self.p.bump()? {
                    b',' => {}
                    b']' => return Ok(None),
                    c => return Err(self.p.err(ErrorKind::UnexpectedByte(c))),
                }
            }
            self.p.value(0).map(Some)
        })();
        match r {
            Ok(Some(v)) => Some(v),
            _ => {
                self.done = true;
                None
            }
        }
    }
}

/// A validated JSON object (raw text including braces).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Object<'a>(&'a str);

impl<'a> Object<'a> {
    pub fn raw(&self) -> &'a str {
        self.0
    }

    pub fn iter(&self) -> ObjectIter<'a> {
        ObjectIter {
            p: Parser::new(self.0),
            started: false,
            done: false,
        }
    }

    /// Value of the first member whose (decoded) key equals `key`.
    pub fn get(&self, key: &str) -> Option<Value<'a>> {
        self.iter().find(|(k, _)| k.eq_str(key)).map(|(_, v)| v)
    }
}

/// Iterator over object members in source order.
#[derive(Clone, Debug)]
pub struct ObjectIter<'a> {
    p: Parser<'a>,
    started: bool,
    done: bool,
}

impl<'a> Iterator for ObjectIter<'a> {
    type Item = (Str<'a>, Value<'a>);

    fn next(&mut self) -> Option<(Str<'a>, Value<'a>)> {
        if self.done {
            return None;
        }
        let r = (|| {
            if !self.started {
                self.started = true;
                self.p.expect(b'{')?;
                self.p.ws();
                if self.p.peek() == Some(b'}') {
                    return Ok(None);
                }
            } else {
                self.p.ws();
                match self.p.bump()? {
                    b',' => {}
                    b'}' => return Ok(None),
                    c => return Err(self.p.err(ErrorKind::UnexpectedByte(c))),
                }
            }
            self.p.ws();
            let k = self.p.string()?;
            self.p.ws();
            self.p.expect(b':')?;
            let v = self.p.value(0)?;
            Ok(Some((k, v)))
        })();
        match r {
            Ok(Some(kv)) => Some(kv),
            _ => {
                self.done = true;
                None
            }
        }
    }
}

/// Parse a complete JSON text (surrounding whitespace allowed).
pub fn parse(input: &[u8]) -> Result<Value<'_>, Error> {
    let s = core::str::from_utf8(input).map_err(|e| Error {
        kind: ErrorKind::InvalidUtf8,
        pos: e.valid_up_to(),
    })?;
    parse_str(s)
}

/// [`parse`] for input already known to be UTF-8.
pub fn parse_str(s: &str) -> Result<Value<'_>, Error> {
    let mut p = Parser::new(s);
    let v = p.value(0)?;
    p.ws();
    if p.pos != p.b.len() {
        return Err(p.err(ErrorKind::TrailingData));
    }
    Ok(v)
}

#[derive(Clone, Debug)]
struct Parser<'a> {
    s: &'a str,
    b: &'a [u8],
    pos: usize,
}

impl<'a> Parser<'a> {
    fn new(s: &'a str) -> Self {
        Parser {
            s,
            b: s.as_bytes(),
            pos: 0,
        }
    }

    fn err(&self, kind: ErrorKind) -> Error {
        Error {
            kind,
            pos: self.pos,
        }
    }

    fn peek(&self) -> Option<u8> {
        self.b.get(self.pos).copied()
    }

    fn bump(&mut self) -> Result<u8, Error> {
        let c = self.peek().ok_or(self.err(ErrorKind::UnexpectedEof))?;
        self.pos += 1;
        Ok(c)
    }

    fn expect(&mut self, want: u8) -> Result<(), Error> {
        match self.peek() {
            Some(c) if c == want => {
                self.pos += 1;
                Ok(())
            }
            Some(c) => Err(self.err(ErrorKind::UnexpectedByte(c))),
            None => Err(self.err(ErrorKind::UnexpectedEof)),
        }
    }

    fn ws(&mut self) {
        while let Some(b' ' | b'\t' | b'\n' | b'\r') = self.peek() {
            self.pos += 1;
        }
    }

    fn literal(&mut self, lit: &[u8]) -> Result<(), Error> {
        for &c in lit {
            self.expect(c)?;
        }
        Ok(())
    }

    fn value(&mut self, depth: usize) -> Result<Value<'a>, Error> {
        self.ws();
        let c = self.peek().ok_or(self.err(ErrorKind::UnexpectedEof))?;
        match c {
            b'n' => self.literal(b"null").map(|_| Value::Null),
            b't' => self.literal(b"true").map(|_| Value::Bool(true)),
            b'f' => self.literal(b"false").map(|_| Value::Bool(false)),
            b'"' => self.string().map(Value::String),
            b'-' | b'0'..=b'9' => self.number().map(Value::Number),
            b'[' | b'{' => {
                if depth >= MAX_DEPTH {
                    return Err(self.err(ErrorKind::TooDeep));
                }
                let start = self.pos;
                if c == b'[' {
                    self.array_body(depth)?;
                    Ok(Value::Array(Array(&self.s[start..self.pos])))
                } else {
                    self.object_body(depth)?;
                    Ok(Value::Object(Object(&self.s[start..self.pos])))
                }
            }
            c => Err(self.err(ErrorKind::UnexpectedByte(c))),
        }
    }

    fn array_body(&mut self, depth: usize) -> Result<(), Error> {
        self.expect(b'[')?;
        self.ws();
        if self.peek() == Some(b']') {
            self.pos += 1;
            return Ok(());
        }
        loop {
            self.value(depth + 1)?;
            self.ws();
            match self.bump()? {
                b',' => {}
                b']' => return Ok(()),
                c => {
                    self.pos -= 1;
                    return Err(self.err(ErrorKind::UnexpectedByte(c)));
                }
            }
        }
    }

    fn object_body(&mut self, depth: usize) -> Result<(), Error> {
        self.expect(b'{')?;
        self.ws();
        if self.peek() == Some(b'}') {
            self.pos += 1;
            return Ok(());
        }
        loop {
            self.ws();
            self.string()?;
            self.ws();
            self.expect(b':')?;
            self.value(depth + 1)?;
            self.ws();
            match self.bump()? {
                b',' => {}
                b'}' => return Ok(()),
                c => {
                    self.pos -= 1;
                    return Err(self.err(ErrorKind::UnexpectedByte(c)));
                }
            }
        }
    }

    fn string(&mut self) -> Result<Str<'a>, Error> {
        self.expect(b'"')?;
        let start = self.pos;
        loop {
            let c = self.peek().ok_or(self.err(ErrorKind::UnexpectedEof))?;
            match c {
                b'"' => {
                    let s = Str(&self.s[start..self.pos]);
                    self.pos += 1;
                    return Ok(s);
                }
                b'\\' => {
                    let rest = &self.b[self.pos + 1..];
                    match decode_escape(rest) {
                        Some((_, n)) => self.pos += 1 + n,
                        None => {
                            return Err(if rest.is_empty() {
                                self.err(ErrorKind::UnexpectedEof)
                            } else {
                                self.err(ErrorKind::BadEscape)
                            })
                        }
                    }
                }
                0x00..=0x1f => return Err(self.err(ErrorKind::ControlInString)),
                _ => self.pos += 1,
            }
        }
    }

    fn digits(&mut self) -> usize {
        let start = self.pos;
        while let Some(b'0'..=b'9') = self.peek() {
            self.pos += 1;
        }
        self.pos - start
    }

    fn number(&mut self) -> Result<Number<'a>, Error> {
        let start = self.pos;
        if self.peek() == Some(b'-') {
            self.pos += 1;
        }
        match self.peek() {
            Some(b'0') => self.pos += 1,
            Some(b'1'..=b'9') => {
                self.digits();
            }
            _ => return Err(self.err(ErrorKind::BadNumber)),
        }
        if self.peek() == Some(b'.') {
            self.pos += 1;
            if self.digits() == 0 {
                return Err(self.err(ErrorKind::BadNumber));
            }
        }
        if let Some(b'e' | b'E') = self.peek() {
            self.pos += 1;
            if let Some(b'+' | b'-') = self.peek() {
                self.pos += 1;
            }
            if self.digits() == 0 {
                return Err(self.err(ErrorKind::BadNumber));
            }
        }
        Ok(Number(&self.s[start..self.pos]))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn p(s: &str) -> Result<Value<'_>, Error> {
        parse(s.as_bytes())
    }

    #[test]
    fn scalars() {
        assert_eq!(p("null"), Ok(Value::Null));
        assert_eq!(p(" true "), Ok(Value::Bool(true)));
        assert_eq!(p("false"), Ok(Value::Bool(false)));
        assert_eq!(p("0").unwrap().as_i64(), Some(0));
        assert_eq!(p("-17").unwrap().as_i64(), Some(-17));
        assert_eq!(p("-17").unwrap().as_u64(), None);
        assert_eq!(p("18446744073709551615").unwrap().as_u64(), Some(u64::MAX));
        assert_eq!(p("18446744073709551616").unwrap().as_u64(), None);
        assert_eq!(p("9223372036854775807").unwrap().as_i64(), Some(i64::MAX));
        assert_eq!(p("1.5").unwrap().as_i64(), None);
        assert_eq!(p("1e3").unwrap().as_i64(), None);
        assert!(matches!(p("1.5e-3"), Ok(Value::Number(_))));
    }

    #[test]
    fn bad_numbers() {
        for s in [
            "01", "-", "1.", ".5", "1e", "1e+", "+1", "--1", "0x10", "1.e5",
        ] {
            assert!(p(s).is_err(), "{s:?} should fail");
        }
    }

    #[test]
    fn strings() {
        let v = p(r#""a\"b\\c\/d\b\f\n\r\t""#).unwrap().as_str().unwrap();
        let mut buf = [0u8; 32];
        assert_eq!(v.decode_into(&mut buf), Some("a\"b\\c/d\u{8}\u{c}\n\r\t"));
        let v = p(r#""Aé€😀""#).unwrap().as_str().unwrap();
        assert_eq!(v.decode_into(&mut buf), Some("Aé€😀"));
        assert!(v.eq_str("Aé€😀"));
        assert!(!v.eq_str("Aé€"));
        assert_eq!(v.decode_into(&mut buf[..3]), None);
        let v = p("\"héllo\"").unwrap().as_str().unwrap();
        assert_eq!(v.as_unescaped(), Some("héllo"));
        assert!(v.eq_str("héllo"));
    }

    #[test]
    fn bad_strings() {
        for s in [
            r#""abc"#,
            r#""\x""#,
            r#""\u12""#,
            r#""\u12g4""#,
            r#""\ud83d""#,
            r#""\ud83dA""#,
            r#""\ude00""#,
            "\"a\nb\"",
            "\"a\u{1}b\"",
            r#""\"#,
        ] {
            assert!(p(s).is_err(), "{s:?} should fail");
        }
        assert_eq!(parse(b"\"\xff\"").unwrap_err().kind, ErrorKind::InvalidUtf8);
    }

    #[test]
    fn containers() {
        let v = p(r#" { "a" : [1, {"b": null}, "x"], "c": {"d": {"e": []}}, "a": 2 } "#).unwrap();
        let o = v.as_object().unwrap();
        // First match wins for duplicate keys.
        let a = o.get("a").unwrap();
        let items: [Option<Value>; 4] = {
            let mut it = match a {
                Value::Array(arr) => arr.iter(),
                _ => panic!(),
            };
            [it.next(), it.next(), it.next(), it.next()]
        };
        assert_eq!(items[0].unwrap().as_i64(), Some(1));
        assert!(items[1]
            .unwrap()
            .as_object()
            .unwrap()
            .get("b")
            .unwrap()
            .is_null());
        assert!(items[2].unwrap().as_str().unwrap().eq_str("x"));
        assert!(items[3].is_none());
        assert_eq!(o.iter().count(), 3);
        assert!(o.get("zz").is_none());
        assert_eq!(p("{}").unwrap().as_object().unwrap().iter().count(), 0);
        let e = p("[]").unwrap();
        assert!(matches!(e, Value::Array(a) if a.iter().next().is_none()));
        // Escaped key lookup.
        let o = p(r#"{"op":"ping"}"#).unwrap().as_object().unwrap();
        assert!(o.get("op").unwrap().as_str().unwrap().eq_str("ping"));
    }

    #[test]
    fn bad_containers() {
        for s in [
            "{",
            "[",
            "{\"a\"}",
            "{\"a\":}",
            "{\"a\":1,}",
            "[1,]",
            "[1 2]",
            "{a:1}",
            "{\"a\":1 \"b\":2}",
            "{} {}",
            "[]]",
            "",
            "   ",
            "nul",
            "truex",
            "{\"a\":1}x",
        ] {
            assert!(p(s).is_err(), "{s:?} should fail");
        }
    }

    #[test]
    fn depth_limit() {
        let mut deep = [b'['; 2 * MAX_DEPTH + 2];
        for b in &mut deep[MAX_DEPTH..] {
            *b = b']';
        }
        assert!(parse(&deep[..2 * MAX_DEPTH]).is_ok());
        let mut deeper = [b'['; 100_000];
        deeper[99_999] = b']';
        assert_eq!(parse(&deeper).unwrap_err().kind, ErrorKind::TooDeep);
    }
}
