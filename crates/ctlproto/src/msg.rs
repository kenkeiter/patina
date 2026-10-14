//! Control-channel messages (design §10.2).
//!
//! Host → guest requests are decoded with [`decode_request`]; guest → host
//! events are encoded with [`Event::encode`] / [`Event::encode_to_slice`].
//! One message per line; encoders append the trailing `\n`.

use core::fmt::{self, Write};

use crate::json::{self, Value};

/// A short string stored inline (signal names, etc.).
#[derive(Clone, Copy, PartialEq, Eq)]
pub struct ShortStr {
    buf: [u8; ShortStr::CAP],
    len: u8,
}

impl ShortStr {
    pub const CAP: usize = 32;

    pub fn new(s: &str) -> Option<Self> {
        if s.len() > Self::CAP {
            return None;
        }
        let mut buf = [0u8; Self::CAP];
        buf[..s.len()].copy_from_slice(s.as_bytes());
        Some(ShortStr {
            buf,
            len: s.len() as u8,
        })
    }

    pub fn as_str(&self) -> &str {
        // Only ever filled from a &str, so this cannot fail.
        core::str::from_utf8(&self.buf[..self.len as usize]).unwrap_or("")
    }
}

impl fmt::Debug for ShortStr {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Debug::fmt(self.as_str(), f)
    }
}

/// Signal named in a `signal` op: `"SIGTERM"`/`"TERM"` or a number.
/// Resolve names with `patina_abi::signal::from_name`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SignalSpec {
    Name(ShortStr),
    Number(i64),
}

/// A host → guest operation.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Op {
    /// `{"op":"ping"}` → reply [`Event::Pong`] echoing the request id.
    Ping,
    /// `{"op":"signal","sig":"SIGTERM"}` → deliver process-wide.
    Signal(SignalSpec),
    /// `{"op":"stop","grace_ms":30000}` → SIGTERM then forced stop after
    /// the grace period. `None` when `grace_ms` is absent (caller picks the
    /// default, see [`DEFAULT_GRACE_MS`]); `Some(0)` means stop now.
    Stop { grace_ms: Option<u64> },
    /// `{"op":"time","epoch_ns":…}` → step the wall clock.
    Time { epoch_ns: i64 },
    /// `{"op":"dump"}` → same as `signal SIGQUIT`.
    Dump,
}

/// Default grace period when `stop` omits `grace_ms` (matches `patina run`).
pub const DEFAULT_GRACE_MS: u64 = 10_000;

/// A decoded request: the op plus the optional integer `id` to echo.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Request {
    pub id: Option<i64>,
    pub op: Op,
}

/// Why a request line was rejected.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DecodeErrorKind {
    /// Not valid JSON.
    Json(json::Error),
    /// Valid JSON but not an object.
    NotObject,
    /// No `"op"` member, or it is not a string.
    MissingOp,
    /// `"op"` names an operation this guest does not know.
    UnknownOp,
    /// A required field is missing.
    MissingField(&'static str),
    /// A field has the wrong type or an out-of-range value.
    BadField(&'static str),
}

/// A rejected request. `id` is set when the line was an object carrying an
/// integer `id`, so the error reply can echo it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DecodeError {
    pub id: Option<i64>,
    pub kind: DecodeErrorKind,
}

impl DecodeError {
    /// Human-readable message for an [`Event::Error`] reply.
    pub fn message(&self) -> &'static str {
        match self.kind {
            DecodeErrorKind::Json(_) => "invalid json",
            DecodeErrorKind::NotObject => "expected object",
            DecodeErrorKind::MissingOp => "missing op",
            DecodeErrorKind::UnknownOp => "unknown op",
            DecodeErrorKind::MissingField(_) => "missing field",
            DecodeErrorKind::BadField(_) => "bad field",
        }
    }

    /// The offending field, if any.
    pub fn field(&self) -> Option<&'static str> {
        match self.kind {
            DecodeErrorKind::MissingField(f) | DecodeErrorKind::BadField(f) => Some(f),
            _ => None,
        }
    }

    /// The reply event for this error.
    pub fn reply(&self) -> Event<'static> {
        Event::Error {
            id: self.id,
            msg: self.message(),
            field: self.field(),
        }
    }
}

/// Decode one request line (without or with its trailing newline). Unknown
/// fields are ignored; nothing here panics on any input.
pub fn decode_request(line: &[u8]) -> Result<Request, DecodeError> {
    let v = json::parse(line).map_err(|e| DecodeError {
        id: None,
        kind: DecodeErrorKind::Json(e),
    })?;
    let obj = v.as_object().ok_or(DecodeError {
        id: None,
        kind: DecodeErrorKind::NotObject,
    })?;
    let id = obj.get("id").and_then(|v| v.as_i64());
    let err = |kind| DecodeError { id, kind };
    let op = obj
        .get("op")
        .and_then(|v| v.as_str())
        .ok_or(err(DecodeErrorKind::MissingOp))?;

    let op = if op.eq_str("ping") {
        Op::Ping
    } else if op.eq_str("dump") {
        Op::Dump
    } else if op.eq_str("signal") {
        let spec = match obj.get("sig") {
            None => return Err(err(DecodeErrorKind::MissingField("sig"))),
            Some(Value::String(s)) => {
                let mut buf = [0u8; ShortStr::CAP];
                let name = s
                    .decode_into(&mut buf)
                    .ok_or(err(DecodeErrorKind::BadField("sig")))?;
                if name.is_empty() {
                    return Err(err(DecodeErrorKind::BadField("sig")));
                }
                SignalSpec::Name(ShortStr::new(name).ok_or(err(DecodeErrorKind::BadField("sig")))?)
            }
            Some(Value::Number(n)) => {
                SignalSpec::Number(n.as_i64().ok_or(err(DecodeErrorKind::BadField("sig")))?)
            }
            Some(_) => return Err(err(DecodeErrorKind::BadField("sig"))),
        };
        Op::Signal(spec)
    } else if op.eq_str("stop") {
        let grace_ms = match obj.get("grace_ms") {
            None | Some(Value::Null) => None,
            Some(v) => Some(
                v.as_u64()
                    .ok_or(err(DecodeErrorKind::BadField("grace_ms")))?,
            ),
        };
        Op::Stop { grace_ms }
    } else if op.eq_str("time") {
        let epoch_ns = match obj.get("epoch_ns") {
            None => return Err(err(DecodeErrorKind::MissingField("epoch_ns"))),
            Some(v) => v
                .as_i64()
                .ok_or(err(DecodeErrorKind::BadField("epoch_ns")))?,
        };
        Op::Time { epoch_ns }
    } else {
        return Err(err(DecodeErrorKind::UnknownOp));
    };
    Ok(Request { id, op })
}

/// A guest → host event.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Event<'a> {
    /// `{"ev":"pong","id":1,"uptime_ms":…}` (`id` omitted if the ping had none).
    Pong { id: Option<i64>, uptime_ms: u64 },
    /// `{"ev":"ready","go":"go1.25.1","cpus":2,"mem_mb":512}`.
    Ready { go: &'a str, cpus: u32, mem_mb: u64 },
    /// `{"ev":"exit","code":N,"signal":"SIGTERM","forced":bool}`
    /// (`signal` only for a `SIG_DFL` termination).
    Exit {
        code: i32,
        signal: Option<&'a str>,
        forced: bool,
    },
    /// `{"ev":"panic","msg":"…"}`.
    Panic { msg: &'a str },
    /// `{"ev":"log","level":"warn","msg":"ENOSYS 334"}`.
    Log { level: &'a str, msg: &'a str },
    /// `{"ev":"error","id":1,"msg":"unknown op","field":"sig"}`: reply to a
    /// rejected request (`id`/`field` omitted when unknown).
    Error {
        id: Option<i64>,
        msg: &'a str,
        field: Option<&'a str>,
    },
}

/// Write `s` as a JSON string literal (with quotes).
pub fn write_json_str<W: Write>(w: &mut W, s: &str) -> fmt::Result {
    w.write_char('"')?;
    let mut start = 0;
    for (i, c) in s.char_indices() {
        let esc: Option<&str> = match c {
            '"' => Some("\\\""),
            '\\' => Some("\\\\"),
            '\n' => Some("\\n"),
            '\r' => Some("\\r"),
            '\t' => Some("\\t"),
            '\u{8}' => Some("\\b"),
            '\u{c}' => Some("\\f"),
            // Line/paragraph separators break some line-oriented consumers.
            '\u{0}'..='\u{1f}' | '\u{7f}' | '\u{2028}' | '\u{2029}' => None,
            _ => continue,
        };
        w.write_str(&s[start..i])?;
        match esc {
            Some(e) => w.write_str(e)?,
            None => write!(w, "\\u{:04x}", c as u32)?,
        }
        start = i + c.len_utf8();
    }
    w.write_str(&s[start..])?;
    w.write_char('"')
}

impl Event<'_> {
    /// Write the event as one JSON line, including the trailing `\n`.
    pub fn encode<W: Write>(&self, w: &mut W) -> fmt::Result {
        match *self {
            Event::Pong { id, uptime_ms } => {
                w.write_str("{\"ev\":\"pong\"")?;
                if let Some(id) = id {
                    write!(w, ",\"id\":{id}")?;
                }
                write!(w, ",\"uptime_ms\":{uptime_ms}}}")?;
            }
            Event::Ready { go, cpus, mem_mb } => {
                w.write_str("{\"ev\":\"ready\",\"go\":")?;
                write_json_str(w, go)?;
                write!(w, ",\"cpus\":{cpus},\"mem_mb\":{mem_mb}}}")?;
            }
            Event::Exit {
                code,
                signal,
                forced,
            } => {
                write!(w, "{{\"ev\":\"exit\",\"code\":{code}")?;
                if let Some(sig) = signal {
                    w.write_str(",\"signal\":")?;
                    write_json_str(w, sig)?;
                }
                write!(w, ",\"forced\":{forced}}}")?;
            }
            Event::Panic { msg } => {
                w.write_str("{\"ev\":\"panic\",\"msg\":")?;
                write_json_str(w, msg)?;
                w.write_char('}')?;
            }
            Event::Log { level, msg } => {
                w.write_str("{\"ev\":\"log\",\"level\":")?;
                write_json_str(w, level)?;
                w.write_str(",\"msg\":")?;
                write_json_str(w, msg)?;
                w.write_char('}')?;
            }
            Event::Error { id, msg, field } => {
                w.write_str("{\"ev\":\"error\"")?;
                if let Some(id) = id {
                    write!(w, ",\"id\":{id}")?;
                }
                w.write_str(",\"msg\":")?;
                write_json_str(w, msg)?;
                if let Some(f) = field {
                    w.write_str(",\"field\":")?;
                    write_json_str(w, f)?;
                }
                w.write_char('}')?;
            }
        }
        w.write_char('\n')
    }

    /// Encode into `buf`; returns the length, or `None` if it does not fit.
    pub fn encode_to_slice(&self, buf: &mut [u8]) -> Option<usize> {
        let mut w = SliceWriter { buf, len: 0 };
        self.encode(&mut w).ok()?;
        Some(w.len)
    }
}

/// `fmt::Write` into a fixed byte buffer; errors when full.
#[derive(Debug)]
pub struct SliceWriter<'a> {
    pub buf: &'a mut [u8],
    pub len: usize,
}

impl Write for SliceWriter<'_> {
    fn write_str(&mut self, s: &str) -> fmt::Result {
        let end = self.len.checked_add(s.len()).ok_or(fmt::Error)?;
        if end > self.buf.len() {
            return Err(fmt::Error);
        }
        self.buf[self.len..end].copy_from_slice(s.as_bytes());
        self.len = end;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    extern crate std;
    use std::string::String;

    use super::*;

    fn enc(e: Event) -> String {
        let mut s = String::new();
        e.encode(&mut s).unwrap();
        s
    }

    fn dec(s: &str) -> Result<Request, DecodeError> {
        decode_request(s.as_bytes())
    }

    #[test]
    fn decode_ops() {
        assert_eq!(
            dec(r#"{"op":"ping","id":1}"#),
            Ok(Request {
                id: Some(1),
                op: Op::Ping
            })
        );
        assert_eq!(
            dec("{\"op\":\"ping\"}\n"),
            Ok(Request {
                id: None,
                op: Op::Ping
            })
        );
        assert_eq!(dec(r#"{"op":"dump"}"#).unwrap().op, Op::Dump);
        let r = dec(r#"{"op":"signal","sig":"SIGTERM"}"#).unwrap();
        assert_eq!(
            r.op,
            Op::Signal(SignalSpec::Name(ShortStr::new("SIGTERM").unwrap()))
        );
        match r.op {
            Op::Signal(SignalSpec::Name(n)) => assert_eq!(n.as_str(), "SIGTERM"),
            _ => unreachable!(),
        }
        assert_eq!(
            dec(r#"{"op":"signal","sig":15}"#).unwrap().op,
            Op::Signal(SignalSpec::Number(15))
        );
        assert_eq!(
            dec(r#"{"op":"signal","sig":"SIGHUP"}"#).unwrap().op,
            Op::Signal(SignalSpec::Name(ShortStr::new("SIGHUP").unwrap()))
        );
        assert_eq!(
            dec(r#"{"op":"stop","grace_ms":30000}"#).unwrap().op,
            Op::Stop {
                grace_ms: Some(30000)
            }
        );
        assert_eq!(
            dec(r#"{"op":"stop","grace_ms":0}"#).unwrap().op,
            Op::Stop { grace_ms: Some(0) }
        );
        assert_eq!(
            dec(r#"{"op":"stop"}"#).unwrap().op,
            Op::Stop { grace_ms: None }
        );
        assert_eq!(
            dec(r#"{"op":"time","epoch_ns":1760000000000000000}"#)
                .unwrap()
                .op,
            Op::Time {
                epoch_ns: 1_760_000_000_000_000_000
            }
        );
    }

    #[test]
    fn unknown_fields_ignored() {
        let r = dec(r#"{"x":{"deep":[1,2,{"y":null}]},"op":"ping","extra":"é","id":7,"f":1.5e3}"#)
            .unwrap();
        assert_eq!(
            r,
            Request {
                id: Some(7),
                op: Op::Ping
            }
        );
        // Non-integer id is ignored, not an error.
        assert_eq!(dec(r#"{"op":"ping","id":"abc"}"#).unwrap().id, None);
    }

    #[test]
    fn decode_errors() {
        let e = dec(r#"{"op":"reboot","id":3}"#).unwrap_err();
        assert_eq!(
            e,
            DecodeError {
                id: Some(3),
                kind: DecodeErrorKind::UnknownOp
            }
        );
        assert_eq!(
            enc(e.reply()),
            "{\"ev\":\"error\",\"id\":3,\"msg\":\"unknown op\"}\n"
        );
        assert_eq!(
            dec(r#"{"id":3}"#).unwrap_err().kind,
            DecodeErrorKind::MissingOp
        );
        assert_eq!(
            dec(r#"{"op":5}"#).unwrap_err().kind,
            DecodeErrorKind::MissingOp
        );
        assert_eq!(dec("[1]").unwrap_err().kind, DecodeErrorKind::NotObject);
        assert!(matches!(
            dec("{").unwrap_err().kind,
            DecodeErrorKind::Json(_)
        ));
        assert_eq!(
            dec(r#"{"op":"signal"}"#).unwrap_err().kind,
            DecodeErrorKind::MissingField("sig")
        );
        assert_eq!(
            dec(r#"{"op":"signal","sig":""}"#).unwrap_err().kind,
            DecodeErrorKind::BadField("sig")
        );
        assert_eq!(
            dec(r#"{"op":"signal","sig":true}"#).unwrap_err().kind,
            DecodeErrorKind::BadField("sig")
        );
        assert_eq!(
            dec(r#"{"op":"signal","sig":"SIGAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA"}"#)
                .unwrap_err()
                .kind,
            DecodeErrorKind::BadField("sig")
        );
        assert_eq!(
            dec(r#"{"op":"stop","grace_ms":-1}"#).unwrap_err().kind,
            DecodeErrorKind::BadField("grace_ms")
        );
        assert_eq!(
            dec(r#"{"op":"stop","grace_ms":"1s"}"#).unwrap_err().kind,
            DecodeErrorKind::BadField("grace_ms")
        );
        assert_eq!(
            dec(r#"{"op":"time"}"#).unwrap_err().kind,
            DecodeErrorKind::MissingField("epoch_ns")
        );
        assert_eq!(
            dec(r#"{"op":"time","epoch_ns":1.5}"#).unwrap_err().kind,
            DecodeErrorKind::BadField("epoch_ns")
        );
        let e = dec(r#"{"op":"time","id":9}"#).unwrap_err();
        assert_eq!(
            enc(e.reply()),
            "{\"ev\":\"error\",\"id\":9,\"msg\":\"missing field\",\"field\":\"epoch_ns\"}\n"
        );
    }

    #[test]
    fn encode_events() {
        assert_eq!(
            enc(Event::Pong {
                id: Some(1),
                uptime_ms: 1234
            }),
            "{\"ev\":\"pong\",\"id\":1,\"uptime_ms\":1234}\n"
        );
        assert_eq!(
            enc(Event::Pong {
                id: None,
                uptime_ms: 0
            }),
            "{\"ev\":\"pong\",\"uptime_ms\":0}\n"
        );
        assert_eq!(
            enc(Event::Ready {
                go: "go1.25.1",
                cpus: 2,
                mem_mb: 512
            }),
            "{\"ev\":\"ready\",\"go\":\"go1.25.1\",\"cpus\":2,\"mem_mb\":512}\n"
        );
        assert_eq!(
            enc(Event::Exit {
                code: 143,
                signal: Some("SIGTERM"),
                forced: false
            }),
            "{\"ev\":\"exit\",\"code\":143,\"signal\":\"SIGTERM\",\"forced\":false}\n"
        );
        assert_eq!(
            enc(Event::Exit {
                code: 137,
                signal: None,
                forced: true
            }),
            "{\"ev\":\"exit\",\"code\":137,\"forced\":true}\n"
        );
        assert_eq!(
            enc(Event::Log {
                level: "warn",
                msg: "ENOSYS 334"
            }),
            "{\"ev\":\"log\",\"level\":\"warn\",\"msg\":\"ENOSYS 334\"}\n"
        );
    }

    #[test]
    fn escaping() {
        let msg = "a\"b\\c\nd\re\tf\u{1}g\u{7f}h\u{2028}é😀";
        let line = enc(Event::Panic { msg });
        assert_eq!(
            line,
            "{\"ev\":\"panic\",\"msg\":\"a\\\"b\\\\c\\nd\\re\\tf\\u0001g\\u007fh\\u2028é😀\"}\n"
        );
        assert_eq!(line.matches('\n').count(), 1);
        // Round-trip through our own parser.
        let v = json::parse(line.trim_end().as_bytes()).unwrap();
        let s = v.as_object().unwrap().get("msg").unwrap().as_str().unwrap();
        assert!(s.eq_str(msg));
    }

    #[test]
    fn slice_encoding() {
        let mut buf = [0u8; 64];
        let n = Event::Pong {
            id: Some(1),
            uptime_ms: 5,
        }
        .encode_to_slice(&mut buf)
        .unwrap();
        assert_eq!(&buf[..n], b"{\"ev\":\"pong\",\"id\":1,\"uptime_ms\":5}\n");
        let mut small = [0u8; 10];
        assert_eq!(
            Event::Panic {
                msg: "too long for this"
            }
            .encode_to_slice(&mut small),
            None
        );
    }
}
