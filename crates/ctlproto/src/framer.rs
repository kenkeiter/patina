//! Incremental newline framer for the control channel.
//!
//! Feed it whatever chunks the transport delivers (vsock packets, console
//! port reads); it calls back once per complete line, with the `\n` (and a
//! trailing `\r`, if any) stripped. Blank lines are skipped. A line whose
//! bytes before the `\n` (including any `\r`) exceed the buffer capacity is
//! dropped and reported once as [`Frame::TooLong`]; framing resynchronises at
//! the next `\n`.

/// One framing result.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Frame<'a> {
    /// A complete line (without terminator).
    Line(&'a [u8]),
    /// A line exceeded the capacity and was discarded.
    TooLong,
}

/// Line framer with an inline buffer of `N` bytes (= max line length).
#[derive(Clone, Debug)]
pub struct LineFramer<const N: usize> {
    buf: [u8; N],
    len: usize,
    /// Dropping bytes until the next newline after an overflow.
    discarding: bool,
}

impl<const N: usize> Default for LineFramer<N> {
    fn default() -> Self {
        Self::new()
    }
}

impl<const N: usize> LineFramer<N> {
    pub const fn new() -> Self {
        LineFramer {
            buf: [0; N],
            len: 0,
            discarding: false,
        }
    }

    /// Maximum line length (excluding the `\n`, including any `\r`).
    pub const fn capacity(&self) -> usize {
        N
    }

    /// Bytes of an incomplete line currently buffered.
    pub fn pending(&self) -> usize {
        self.len
    }

    /// Drop any partial line (e.g. when the connection is replaced).
    pub fn reset(&mut self) {
        self.len = 0;
        self.discarding = false;
    }

    /// Consume `chunk`, calling `f` for each complete line.
    pub fn feed(&mut self, mut chunk: &[u8], mut f: impl FnMut(Frame<'_>)) {
        while !chunk.is_empty() {
            let nl = chunk.iter().position(|&b| b == b'\n');
            let (part, rest, complete) = match nl {
                Some(i) => (&chunk[..i], &chunk[i + 1..], true),
                None => (chunk, &chunk[chunk.len()..], false),
            };
            chunk = rest;

            if self.discarding {
                if complete {
                    self.discarding = false;
                }
                continue;
            }

            // Fast path: whole line inside this chunk and nothing buffered.
            if complete && self.len == 0 {
                if part.len() > N {
                    f(Frame::TooLong);
                } else {
                    emit(part, &mut f);
                }
                continue;
            }

            if self.len + part.len() > N {
                self.len = 0;
                f(Frame::TooLong);
                self.discarding = !complete;
                continue;
            }

            self.buf[self.len..self.len + part.len()].copy_from_slice(part);
            self.len += part.len();
            if complete {
                let total = self.len;
                self.len = 0;
                emit(&self.buf[..total], &mut f);
            }
        }
    }
}

fn emit(line: &[u8], f: &mut impl FnMut(Frame<'_>)) {
    let line = line.strip_suffix(b"\r").unwrap_or(line);
    if !line.is_empty() {
        f(Frame::Line(line));
    }
}

#[cfg(test)]
mod tests {
    extern crate std;
    use std::vec::Vec;

    use super::*;

    #[derive(Debug, PartialEq)]
    enum F {
        L(Vec<u8>),
        Long,
    }

    fn run<const N: usize>(fr: &mut LineFramer<N>, chunks: &[&[u8]]) -> Vec<F> {
        let mut out = Vec::new();
        for c in chunks {
            fr.feed(c, |f| {
                out.push(match f {
                    Frame::Line(l) => F::L(l.to_vec()),
                    Frame::TooLong => F::Long,
                })
            });
        }
        out
    }

    #[test]
    fn basic() {
        let mut fr = LineFramer::<64>::new();
        let out = run(
            &mut fr,
            &[b"{\"op\":\"ping\"}\n{\"op\":", b"\"dump\"}\r\n\n\npartial"],
        );
        assert_eq!(
            out,
            [
                F::L(b"{\"op\":\"ping\"}".to_vec()),
                F::L(b"{\"op\":\"dump\"}".to_vec())
            ]
        );
        assert_eq!(fr.pending(), 7);
        let out = run(&mut fr, &[b"\n"]);
        assert_eq!(out, [F::L(b"partial".to_vec())]);
        assert_eq!(fr.pending(), 0);
    }

    #[test]
    fn byte_at_a_time() {
        let input = b"abc\ndef\r\nghi\n";
        let mut fr = LineFramer::<8>::new();
        let chunks: Vec<&[u8]> = input.chunks(1).collect();
        let out = run(&mut fr, &chunks);
        assert_eq!(
            out,
            [
                F::L(b"abc".to_vec()),
                F::L(b"def".to_vec()),
                F::L(b"ghi".to_vec())
            ]
        );
    }

    #[test]
    fn too_long() {
        let mut fr = LineFramer::<4>::new();
        // Exactly capacity is fine; a CR counts towards it.
        assert_eq!(run(&mut fr, &[b"abcd\n"]), [F::L(b"abcd".to_vec())]);
        assert_eq!(run(&mut fr, &[b"ab", b"c\r\n"]), [F::L(b"abc".to_vec())]);
        assert_eq!(run(&mut fr, &[b"abcd\r\n"]), [F::Long]);
        assert_eq!(run(&mut fr, &[b"abcd\r", b"\n"]), [F::Long]);
        // One over: dropped in a single chunk.
        assert_eq!(
            run(&mut fr, &[b"abcde\nok\n"]),
            [F::Long, F::L(b"ok".to_vec())]
        );
        // Over across chunks: reported once, resync at newline.
        assert_eq!(
            run(&mut fr, &[b"abc", b"defgh", b"ijk", b"l\nok\n"]),
            [F::Long, F::L(b"ok".to_vec())]
        );
        assert_eq!(fr.pending(), 0);
        // Reset drops a partial line.
        run(&mut fr, &[b"ab"]);
        fr.reset();
        assert_eq!(run(&mut fr, &[b"cd\n"]), [F::L(b"cd".to_vec())]);
    }

    #[test]
    fn every_split_point() {
        let input: &[u8] = b"{\"op\":\"ping\",\"id\":1}\r\n\n{\"op\":\"stop\"}\nxx";
        for i in 0..=input.len() {
            for j in i..=input.len() {
                let mut fr = LineFramer::<32>::new();
                let out = run(&mut fr, &[&input[..i], &input[i..j], &input[j..]]);
                assert_eq!(
                    out,
                    [
                        F::L(b"{\"op\":\"ping\",\"id\":1}".to_vec()),
                        F::L(b"{\"op\":\"stop\"}".to_vec())
                    ],
                    "split {i},{j}"
                );
                assert_eq!(fr.pending(), 2);
            }
        }
    }
}
