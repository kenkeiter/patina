//! Fuzz-ish robustness tests: random, mutated and truncated inputs must
//! never panic, and encode → parse must round-trip.

extern crate std;
use std::string::String;
use std::vec::Vec;

use crate::framer::{Frame, LineFramer};
use crate::json::{self, Value};
use crate::msg::{decode_request, Event};

/// xorshift64* — deterministic, dependency-free.
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        self.0 = x;
        x.wrapping_mul(0x2545_f491_4f6c_dd1d)
    }
    fn below(&mut self, n: usize) -> usize {
        (self.next() % n as u64) as usize
    }
}

const SEEDS: &[&str] = &[
    r#"{"op":"ping","id":1}"#,
    r#"{"op":"signal","sig":"SIGTERM"}"#,
    r#"{"op":"signal","sig":15}"#,
    r#"{"op":"stop","grace_ms":30000}"#,
    r#"{"op":"time","epoch_ns":1760000000000000000}"#,
    r#"{"op":"dump"}"#,
    r#"{"a":[1,-2.5e10,true,false,null,{"b":"é😀\n"}],"op":"ping"}"#,
];

const ALPHABET: &[u8] =
    b"{}[]:,\"\\ \t\r\nunltrfaes0123456789-+.eEopingsigSIGTERM\x00\x1f\x7f\xc3\xa9\xff";

/// Walk a parsed value completely (bounded by MAX_DEPTH via the parser).
fn walk(v: Value<'_>, depth: usize) {
    assert!(depth <= json::MAX_DEPTH + 1);
    match v {
        Value::String(s) => {
            let mut buf = [0u8; 64];
            let _ = s.decode_into(&mut buf);
            let _ = s.chars().count();
            let _ = s.eq_str("ping");
        }
        Value::Number(n) => {
            let _ = (n.as_i64(), n.as_u64());
        }
        Value::Array(a) => a.iter().for_each(|x| walk(x, depth + 1)),
        Value::Object(o) => {
            for (k, x) in o.iter() {
                let _ = k.chars().count();
                walk(x, depth + 1);
            }
            let _ = o.get("op");
        }
        Value::Null | Value::Bool(_) => {}
    }
}

fn exercise(input: &[u8]) {
    let _ = decode_request(input);
    if let Ok(v) = json::parse(input) {
        walk(v, 0);
    }
    if let Err(e) = decode_request(input) {
        let mut buf = [0u8; 128];
        assert!(e.reply().encode_to_slice(&mut buf).is_some());
    }
}

#[test]
fn truncations_of_valid_messages() {
    for s in SEEDS {
        let b = s.as_bytes();
        assert!(decode_request(b).is_ok() || s.contains("\"a\""), "{s}");
        for i in 0..b.len() {
            exercise(&b[..i]);
            exercise(&b[i..]);
        }
    }
}

#[test]
fn random_mutations() {
    let mut rng = Rng(0x9e37_79b9_7f4a_7c15);
    for _ in 0..50_000 {
        let mut v: Vec<u8> = SEEDS[rng.below(SEEDS.len())].as_bytes().to_vec();
        for _ in 0..1 + rng.below(4) {
            match rng.below(4) {
                0 if !v.is_empty() => {
                    let i = rng.below(v.len());
                    v[i] = ALPHABET[rng.below(ALPHABET.len())];
                }
                1 => {
                    let i = rng.below(v.len() + 1);
                    v.insert(i, ALPHABET[rng.below(ALPHABET.len())]);
                }
                2 if !v.is_empty() => {
                    let i = rng.below(v.len());
                    v.remove(i);
                }
                _ => {
                    let i = rng.below(v.len() + 1);
                    v.truncate(i);
                }
            }
        }
        exercise(&v);
    }
}

#[test]
fn random_garbage() {
    let mut rng = Rng(42);
    for _ in 0..20_000 {
        let len = rng.below(80);
        let v: Vec<u8> = (0..len)
            .map(|_| {
                if rng.below(2) == 0 {
                    ALPHABET[rng.below(ALPHABET.len())]
                } else {
                    rng.next() as u8
                }
            })
            .collect();
        exercise(&v);
    }
    // Pathological nesting.
    let mut deep = Vec::new();
    for i in 0..10_000 {
        deep.extend_from_slice(if i % 2 == 0 { b"{\"a\":[" } else { b"[" });
    }
    exercise(&deep);
}

#[test]
fn framer_random_chunks() {
    let mut rng = Rng(7);
    let mut stream = Vec::new();
    let mut expected = 0;
    for _ in 0..500 {
        let s = SEEDS[rng.below(SEEDS.len())];
        stream.extend_from_slice(s.as_bytes());
        stream.extend_from_slice(if rng.below(2) == 0 { b"\n" } else { b"\r\n" });
        expected += 1;
        if rng.below(10) == 0 {
            // Garbage line longer than the framer capacity.
            stream.extend(core::iter::repeat_n(b'x', 300));
            stream.push(b'\n');
        }
    }
    let mut fr = LineFramer::<128>::new();
    let mut got = 0;
    let mut pos = 0;
    while pos < stream.len() {
        let n = (1 + rng.below(64)).min(stream.len() - pos);
        fr.feed(&stream[pos..pos + n], |f| {
            if let Frame::Line(l) = f {
                exercise(l);
                assert!(json::parse(l).is_ok());
                got += 1;
            }
        });
        pos += n;
    }
    assert_eq!(got, expected);
}

#[test]
fn encode_parse_roundtrip() {
    let mut rng = Rng(1234);
    let pool: Vec<char> = "ab\"\\/\n\r\t\u{0}\u{1}\u{1f}\u{7f}é€😀\u{2028}\u{fffd} {}[]:,"
        .chars()
        .collect();
    for _ in 0..5_000 {
        let msg: String = (0..rng.below(40))
            .map(|_| pool[rng.below(pool.len())])
            .collect();
        let level: String = (0..rng.below(8))
            .map(|_| pool[rng.below(pool.len())])
            .collect();
        let mut line = String::new();
        Event::Log {
            level: &level,
            msg: &msg,
        }
        .encode(&mut line)
        .unwrap();
        assert!(line.ends_with('\n'));
        assert_eq!(
            line.matches('\n').count(),
            1,
            "raw newline leaked: {line:?}"
        );
        let v = json::parse(&line.as_bytes()[..line.len() - 1]).unwrap();
        let o = v.as_object().unwrap();
        assert!(o.get("ev").unwrap().as_str().unwrap().eq_str("log"));
        assert!(o.get("level").unwrap().as_str().unwrap().eq_str(&level));
        assert!(o.get("msg").unwrap().as_str().unwrap().eq_str(&msg));
    }
}
