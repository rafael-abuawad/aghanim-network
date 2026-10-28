//! Field values.
//!
//! Values map naturally onto CBOR (and therefore onto JS values through
//! cbor-x). Non-JSON types use CBOR tags, so new field types, such as CRDT
//! counters or text, can be added later as new tags without breaking old data.

use std::cmp::Ordering;
use std::collections::BTreeMap;

use crate::cbor::{DResult, Dec, DecodeError, Enc, Kind};

/// CBOR tag for an encrypted value: `[epoch, nonce, ciphertext]`.
pub const TAG_ENCRYPTED: u64 = 40100;
/// Tags from 40101 up are reserved for future CRDT field types.
pub const TAG_RESERVED_START: u64 = 40101;

const MAX_DEPTH: u32 = 32;

#[derive(Debug, Clone, PartialEq)]
pub enum Value {
    Null,
    Bool(bool),
    Int(i64),
    Float(f64),
    Str(String),
    Bytes(Vec<u8>),
    Array(Vec<Value>),
    Map(BTreeMap<String, Value>),
    /// A value encrypted with a space key. See `aghanim-crypto`.
    Encrypted(Sealed),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Sealed {
    pub epoch: u32,
    pub nonce: [u8; 24],
    pub ct: Vec<u8>,
}

pub type Fields = BTreeMap<String, Value>;

impl Value {
    pub fn encode(&self, e: &mut Enc) {
        match self {
            Value::Null => {
                e.null();
            }
            Value::Bool(b) => {
                e.bool(*b);
            }
            Value::Int(i) => {
                e.i64(*i);
            }
            Value::Float(f) => {
                e.f64(*f);
            }
            Value::Str(s) => {
                e.str(s);
            }
            Value::Bytes(b) => {
                e.bytes(b);
            }
            Value::Array(a) => {
                e.array(a.len());
                for v in a {
                    v.encode(e);
                }
            }
            Value::Map(m) => {
                e.map(m.len());
                for (k, v) in m {
                    e.str(k);
                    v.encode(e);
                }
            }
            Value::Encrypted(s) => {
                e.tag(TAG_ENCRYPTED)
                    .array(3)
                    .u64(s.epoch as u64)
                    .bytes(&s.nonce)
                    .bytes(&s.ct);
            }
        }
    }

    pub fn to_bytes(&self) -> Vec<u8> {
        let mut e = Enc::new();
        self.encode(&mut e);
        e.into_bytes()
    }

    pub fn decode(d: &mut Dec) -> DResult<Value> {
        Self::decode_depth(d, 0)
    }

    pub fn from_bytes(b: &[u8]) -> DResult<Value> {
        let mut d = Dec::new(b);
        let v = Self::decode(&mut d)?;
        d.finish()?;
        Ok(v)
    }

    fn decode_depth(d: &mut Dec, depth: u32) -> DResult<Value> {
        if depth > MAX_DEPTH {
            return Err(DecodeError("value nested too deeply".into()));
        }
        Ok(match d.peek()? {
            Kind::Uint => {
                let n = d.u64()?;
                match i64::try_from(n) {
                    Ok(i) => Value::Int(i),
                    Err(_) => Value::Float(n as f64),
                }
            }
            Kind::Nint => Value::Int(d.i64()?),
            Kind::Float => {
                let f = d.f64()?;
                // JS has a single number type. Store integral floats as ints so
                // a value written as `1` from JS and `1` from Rust is identical.
                if f.fract() == 0.0
                    && f.abs() < 9.007_199_254_740_992e15
                    && !(f == 0.0 && f.is_sign_negative())
                {
                    Value::Int(f as i64)
                } else {
                    Value::Float(f)
                }
            }
            Kind::Bytes => Value::Bytes(d.bytes()?.to_vec()),
            Kind::Str => Value::Str(d.str()?.to_owned()),
            Kind::Array => {
                let n = d.array()?;
                let mut a = Vec::with_capacity(n.min(1024));
                for _ in 0..n {
                    a.push(Self::decode_depth(d, depth + 1)?);
                }
                Value::Array(a)
            }
            Kind::Map => {
                let n = d.map()?;
                let mut m = BTreeMap::new();
                for _ in 0..n {
                    let k = d.str()?.to_owned();
                    m.insert(k, Self::decode_depth(d, depth + 1)?);
                }
                Value::Map(m)
            }
            Kind::Tag => {
                let tag = d.tag()?;
                if tag != TAG_ENCRYPTED {
                    return Err(DecodeError(format!("unknown value tag {tag}")));
                }
                if d.array()? != 3 {
                    return Err(DecodeError("bad encrypted value".into()));
                }
                Value::Encrypted(Sealed {
                    epoch: d.u32()?,
                    nonce: d.array_n()?,
                    ct: d.bytes()?.to_vec(),
                })
            }
            Kind::Bool => Value::Bool(d.bool()?),
            Kind::Null | Kind::Undefined => {
                d.null()?;
                Value::Null
            }
        })
    }

    /// Firestore-style type rank, used to order values of different types.
    fn rank(&self) -> u8 {
        match self {
            Value::Null => 0,
            Value::Bool(_) => 1,
            Value::Int(_) | Value::Float(_) => 2,
            Value::Str(_) => 3,
            Value::Bytes(_) => 4,
            Value::Array(_) => 5,
            Value::Map(_) => 6,
            Value::Encrypted(_) => 7,
        }
    }

    pub fn as_f64(&self) -> Option<f64> {
        match self {
            Value::Int(i) => Some(*i as f64),
            Value::Float(f) => Some(*f),
            _ => None,
        }
    }

    pub fn as_str(&self) -> Option<&str> {
        match self {
            Value::Str(s) => Some(s),
            _ => None,
        }
    }

    pub fn as_i64(&self) -> Option<i64> {
        match self {
            Value::Int(i) => Some(*i),
            _ => None,
        }
    }

    /// True if the two values can be compared with `<`/`>` (same type class).
    pub fn same_class(&self, other: &Value) -> bool {
        self.rank() == other.rank()
    }

    /// Total order across all values: by type rank, then by value.
    pub fn total_cmp(&self, other: &Value) -> Ordering {
        match self.rank().cmp(&other.rank()) {
            Ordering::Equal => {}
            o => return o,
        }
        match (self, other) {
            (Value::Bool(a), Value::Bool(b)) => a.cmp(b),
            (Value::Int(a), Value::Int(b)) => a.cmp(b),
            (a @ (Value::Int(_) | Value::Float(_)), b) => {
                a.as_f64().unwrap().total_cmp(&b.as_f64().unwrap())
            }
            (Value::Str(a), Value::Str(b)) => a.cmp(b),
            (Value::Bytes(a), Value::Bytes(b)) => a.cmp(b),
            (Value::Array(a), Value::Array(b)) => {
                for (x, y) in a.iter().zip(b) {
                    match x.total_cmp(y) {
                        Ordering::Equal => {}
                        o => return o,
                    }
                }
                a.len().cmp(&b.len())
            }
            (Value::Map(_), Value::Map(_)) | (Value::Encrypted(_), Value::Encrypted(_)) => {
                self.to_bytes().cmp(&other.to_bytes())
            }
            _ => Ordering::Equal,
        }
    }

    /// Query equality: numbers compare by value, so `1 == 1.0`.
    pub fn query_eq(&self, other: &Value) -> bool {
        self.total_cmp(other) == Ordering::Equal
    }
}

pub fn encode_fields(e: &mut Enc, fields: &Fields) {
    e.map(fields.len());
    for (k, v) in fields {
        e.str(k);
        v.encode(e);
    }
}

pub fn fields_to_bytes(fields: &Fields) -> Vec<u8> {
    let mut e = Enc::new();
    encode_fields(&mut e, fields);
    e.into_bytes()
}

/// Decodes a field map whose values may be `undefined`, meaning "delete this
/// field" (what `deleteField()` sends from the JS SDK).
pub fn decode_field_updates(d: &mut Dec) -> DResult<BTreeMap<String, Option<Value>>> {
    let n = d.map()?;
    let mut m = BTreeMap::new();
    for _ in 0..n {
        let k = d.str()?.to_owned();
        let v = if d.peek()? == Kind::Undefined {
            d.null()?;
            None
        } else {
            Some(Value::decode(d)?)
        };
        m.insert(k, v);
    }
    Ok(m)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip_nested() {
        let mut m = BTreeMap::new();
        m.insert(
            "a".into(),
            Value::Array(vec![Value::Int(-3), Value::Float(2.5), Value::Null]),
        );
        m.insert("b".into(), Value::Bytes(vec![0, 1]));
        m.insert(
            "c".into(),
            Value::Encrypted(Sealed {
                epoch: 2,
                nonce: [7; 24],
                ct: vec![1, 2, 3],
            }),
        );
        let v = Value::Map(m);
        assert_eq!(Value::from_bytes(&v.to_bytes()).unwrap(), v);
    }

    #[test]
    fn integral_floats_become_ints() {
        // f64 1.0 as cbor-x would send it.
        let b = [0xfb, 0x3f, 0xf0, 0, 0, 0, 0, 0, 0];
        assert_eq!(Value::from_bytes(&b).unwrap(), Value::Int(1));
    }

    #[test]
    fn ordering_across_types() {
        let mut v = vec![
            Value::Str("a".into()),
            Value::Float(1.5),
            Value::Null,
            Value::Int(1),
            Value::Bool(true),
        ];
        v.sort_by(|a, b| a.total_cmp(b));
        assert_eq!(
            v,
            vec![
                Value::Null,
                Value::Bool(true),
                Value::Int(1),
                Value::Float(1.5),
                Value::Str("a".into())
            ]
        );
    }

    #[test]
    fn rejects_unknown_tags() {
        let mut e = Enc::new();
        e.tag(TAG_RESERVED_START).null();
        assert!(Value::from_bytes(&e.into_bytes()).is_err());
    }
}
