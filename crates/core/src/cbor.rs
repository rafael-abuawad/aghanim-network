//! A small, deterministic CBOR (RFC 8949) codec.
//!
//! Everything aghanim signs or stores goes through this encoder, so it always
//! emits the shortest head encoding and definite lengths. That makes the bytes
//! for a given value unique, which signature verification relies on.
//! The decoder also accepts what common JS encoders (cbor-x) produce: f16/f32
//! floats and `undefined`.

use std::fmt;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DecodeError(pub String);

impl fmt::Display for DecodeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "cbor decode error: {}", self.0)
    }
}

impl std::error::Error for DecodeError {}

pub type DResult<T> = Result<T, DecodeError>;

fn err<T>(msg: impl Into<String>) -> DResult<T> {
    Err(DecodeError(msg.into()))
}

/// Upper bound on any declared length, to stop hostile input from forcing
/// huge allocations before the data is actually read.
const MAX_LEN: u64 = 64 * 1024 * 1024;

#[derive(Default)]
pub struct Enc {
    buf: Vec<u8>,
}

impl Enc {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn into_bytes(self) -> Vec<u8> {
        self.buf
    }

    fn head(&mut self, major: u8, n: u64) {
        let m = major << 5;
        if n < 24 {
            self.buf.push(m | n as u8);
        } else if n <= u8::MAX as u64 {
            self.buf.extend_from_slice(&[m | 24, n as u8]);
        } else if n <= u16::MAX as u64 {
            self.buf.push(m | 25);
            self.buf.extend_from_slice(&(n as u16).to_be_bytes());
        } else if n <= u32::MAX as u64 {
            self.buf.push(m | 26);
            self.buf.extend_from_slice(&(n as u32).to_be_bytes());
        } else {
            self.buf.push(m | 27);
            self.buf.extend_from_slice(&n.to_be_bytes());
        }
    }

    pub fn u64(&mut self, n: u64) -> &mut Self {
        self.head(0, n);
        self
    }

    pub fn i64(&mut self, n: i64) -> &mut Self {
        if n >= 0 {
            self.head(0, n as u64);
        } else {
            self.head(1, (-1 - n) as u64);
        }
        self
    }

    pub fn bytes(&mut self, b: &[u8]) -> &mut Self {
        self.head(2, b.len() as u64);
        self.buf.extend_from_slice(b);
        self
    }

    pub fn str(&mut self, s: &str) -> &mut Self {
        self.head(3, s.len() as u64);
        self.buf.extend_from_slice(s.as_bytes());
        self
    }

    pub fn array(&mut self, len: usize) -> &mut Self {
        self.head(4, len as u64);
        self
    }

    pub fn map(&mut self, len: usize) -> &mut Self {
        self.head(5, len as u64);
        self
    }

    pub fn tag(&mut self, tag: u64) -> &mut Self {
        self.head(6, tag);
        self
    }

    pub fn bool(&mut self, b: bool) -> &mut Self {
        self.buf.push(if b { 0xf5 } else { 0xf4 });
        self
    }

    pub fn null(&mut self) -> &mut Self {
        self.buf.push(0xf6);
        self
    }

    pub fn undefined(&mut self) -> &mut Self {
        self.buf.push(0xf7);
        self
    }

    pub fn f64(&mut self, f: f64) -> &mut Self {
        self.buf.push(0xfb);
        self.buf.extend_from_slice(&f.to_bits().to_be_bytes());
        self
    }

    /// Append already-encoded CBOR.
    pub fn raw(&mut self, b: &[u8]) -> &mut Self {
        self.buf.extend_from_slice(b);
        self
    }
}

/// The kind of the next item, as seen by [`Dec::peek`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Uint,
    Nint,
    Bytes,
    Str,
    Array,
    Map,
    Tag,
    Bool,
    Null,
    Undefined,
    Float,
}

pub struct Dec<'a> {
    b: &'a [u8],
    pos: usize,
}

impl<'a> Dec<'a> {
    pub fn new(b: &'a [u8]) -> Self {
        Self { b, pos: 0 }
    }

    pub fn is_empty(&self) -> bool {
        self.pos >= self.b.len()
    }

    pub fn finish(&self) -> DResult<()> {
        if self.is_empty() {
            Ok(())
        } else {
            err("trailing bytes")
        }
    }

    fn byte(&mut self) -> DResult<u8> {
        let Some(&x) = self.b.get(self.pos) else {
            return err("unexpected end of input");
        };
        self.pos += 1;
        Ok(x)
    }

    fn take(&mut self, n: usize) -> DResult<&'a [u8]> {
        let end = self.pos.checked_add(n).filter(|&e| e <= self.b.len());
        let Some(end) = end else {
            return err("unexpected end of input");
        };
        let s = &self.b[self.pos..end];
        self.pos = end;
        Ok(s)
    }

    fn arg(&mut self, info: u8) -> DResult<u64> {
        Ok(match info {
            0..=23 => info as u64,
            24 => self.byte()? as u64,
            25 => u16::from_be_bytes(self.take(2)?.try_into().unwrap()) as u64,
            26 => u32::from_be_bytes(self.take(4)?.try_into().unwrap()) as u64,
            27 => u64::from_be_bytes(self.take(8)?.try_into().unwrap()),
            _ => return err("indefinite lengths are not supported"),
        })
    }

    fn head(&mut self, want: u8) -> DResult<u64> {
        let ib = self.byte()?;
        if ib >> 5 != want {
            self.pos -= 1;
            return err(format!("expected major type {want}, found {}", ib >> 5));
        }
        self.arg(ib & 0x1f)
    }

    fn len_head(&mut self, want: u8) -> DResult<usize> {
        let n = self.head(want)?;
        if n > MAX_LEN {
            return err("length too large");
        }
        Ok(n as usize)
    }

    pub fn peek(&self) -> DResult<Kind> {
        let Some(&ib) = self.b.get(self.pos) else {
            return err("unexpected end of input");
        };
        Ok(match ib >> 5 {
            0 => Kind::Uint,
            1 => Kind::Nint,
            2 => Kind::Bytes,
            3 => Kind::Str,
            4 => Kind::Array,
            5 => Kind::Map,
            6 => Kind::Tag,
            _ => match ib {
                0xf4 | 0xf5 => Kind::Bool,
                0xf6 => Kind::Null,
                0xf7 => Kind::Undefined,
                0xf9..=0xfb => Kind::Float,
                _ => return err(format!("unsupported simple value 0x{ib:02x}")),
            },
        })
    }

    pub fn u64(&mut self) -> DResult<u64> {
        self.head(0)
    }

    pub fn u32(&mut self) -> DResult<u32> {
        u32::try_from(self.u64()?).or_else(|_| err("integer out of range"))
    }

    pub fn i64(&mut self) -> DResult<i64> {
        match self.peek()? {
            Kind::Uint => i64::try_from(self.head(0)?).or_else(|_| err("integer out of range")),
            Kind::Nint => {
                let n = self.head(1)?;
                if n > i64::MAX as u64 {
                    return err("integer out of range");
                }
                Ok(-1 - n as i64)
            }
            k => err(format!("expected integer, found {k:?}")),
        }
    }

    pub fn bytes(&mut self) -> DResult<&'a [u8]> {
        let n = self.len_head(2)?;
        self.take(n)
    }

    pub fn array_n<const N: usize>(&mut self) -> DResult<[u8; N]> {
        let b = self.bytes()?;
        b.try_into()
            .or_else(|_| err(format!("expected {N} bytes, found {}", b.len())))
    }

    pub fn str(&mut self) -> DResult<&'a str> {
        let n = self.len_head(3)?;
        std::str::from_utf8(self.take(n)?).or_else(|_| err("invalid utf-8"))
    }

    pub fn array(&mut self) -> DResult<usize> {
        self.len_head(4)
    }

    pub fn map(&mut self) -> DResult<usize> {
        self.len_head(5)
    }

    pub fn tag(&mut self) -> DResult<u64> {
        self.head(6)
    }

    pub fn bool(&mut self) -> DResult<bool> {
        match self.byte()? {
            0xf4 => Ok(false),
            0xf5 => Ok(true),
            _ => {
                self.pos -= 1;
                err("expected bool")
            }
        }
    }

    pub fn null(&mut self) -> DResult<()> {
        match self.byte()? {
            0xf6 | 0xf7 => Ok(()),
            _ => {
                self.pos -= 1;
                err("expected null")
            }
        }
    }

    /// Reads `null` as `None`, otherwise runs `f`.
    pub fn opt<T>(&mut self, f: impl FnOnce(&mut Self) -> DResult<T>) -> DResult<Option<T>> {
        if matches!(self.peek()?, Kind::Null | Kind::Undefined) {
            self.pos += 1;
            Ok(None)
        } else {
            f(self).map(Some)
        }
    }

    pub fn f64(&mut self) -> DResult<f64> {
        match self.byte()? {
            0xf9 => {
                let h = u16::from_be_bytes(self.take(2)?.try_into().unwrap());
                Ok(f16_to_f64(h))
            }
            0xfa => {
                Ok(f32::from_bits(u32::from_be_bytes(self.take(4)?.try_into().unwrap())) as f64)
            }
            0xfb => Ok(f64::from_bits(u64::from_be_bytes(
                self.take(8)?.try_into().unwrap(),
            ))),
            _ => {
                self.pos -= 1;
                err("expected float")
            }
        }
    }

    /// Skips one complete item, including nested items.
    pub fn skip(&mut self) -> DResult<()> {
        self.skip_depth(0)
    }

    fn skip_depth(&mut self, depth: u32) -> DResult<()> {
        if depth > 64 {
            return err("nesting too deep");
        }
        match self.peek()? {
            Kind::Uint | Kind::Nint => {
                let ib = self.byte()?;
                self.arg(ib & 0x1f)?;
            }
            Kind::Bytes => {
                self.bytes()?;
            }
            Kind::Str => {
                self.str()?;
            }
            Kind::Array => {
                for _ in 0..self.array()? {
                    self.skip_depth(depth + 1)?;
                }
            }
            Kind::Map => {
                for _ in 0..self.map()? {
                    self.skip_depth(depth + 1)?;
                    self.skip_depth(depth + 1)?;
                }
            }
            Kind::Tag => {
                self.tag()?;
                self.skip_depth(depth + 1)?;
            }
            Kind::Bool | Kind::Null | Kind::Undefined => {
                self.pos += 1;
            }
            Kind::Float => {
                self.f64()?;
            }
        }
        Ok(())
    }
}

fn f16_to_f64(h: u16) -> f64 {
    let sign = if h & 0x8000 != 0 { -1.0 } else { 1.0 };
    let exp = ((h >> 10) & 0x1f) as i32;
    let frac = (h & 0x3ff) as f64;
    sign * match exp {
        0 => frac * 2f64.powi(-24),
        31 => {
            if frac == 0.0 {
                f64::INFINITY
            } else {
                f64::NAN
            }
        }
        _ => (1.0 + frac / 1024.0) * 2f64.powi(exp - 15),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ints_use_shortest_head() {
        let mut e = Enc::new();
        e.u64(23).u64(24).u64(256).i64(-1).i64(-25);
        assert_eq!(
            e.into_bytes(),
            vec![0x17, 0x18, 0x18, 0x19, 0x01, 0x00, 0x20, 0x38, 0x18]
        );
    }

    #[test]
    fn roundtrip_basic() {
        let mut e = Enc::new();
        e.array(4).str("hi").bytes(&[1, 2]).i64(-500).f64(1.5);
        let b = e.into_bytes();
        let mut d = Dec::new(&b);
        assert_eq!(d.array().unwrap(), 4);
        assert_eq!(d.str().unwrap(), "hi");
        assert_eq!(d.bytes().unwrap(), &[1, 2]);
        assert_eq!(d.i64().unwrap(), -500);
        assert_eq!(d.f64().unwrap(), 1.5);
        d.finish().unwrap();
    }

    #[test]
    fn decodes_half_and_single_floats() {
        assert_eq!(Dec::new(&[0xf9, 0x3c, 0x00]).f64().unwrap(), 1.0);
        assert_eq!(
            Dec::new(&[0xfa, 0x3f, 0xc0, 0x00, 0x00]).f64().unwrap(),
            1.5
        );
    }

    #[test]
    fn rejects_truncated_and_huge() {
        assert!(Dec::new(&[0x62, b'a']).str().is_err());
        assert!(
            Dec::new(&[0x5b, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff])
                .bytes()
                .is_err()
        );
    }
}
