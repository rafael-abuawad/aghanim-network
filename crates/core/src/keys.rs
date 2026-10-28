//! Order-preserving key encoding for the key-value store.
//!
//! Strings are escaped tuple elements in the style of FoundationDB: `0x00`
//! becomes `0x00 0xff` and each element ends with `0x00`, so byte order
//! matches element order. Key families:
//!
//! - `d` + segment count + segments: a document. Putting the segment count
//!   first means a collection's direct children share one prefix, without
//!   picking up documents in subcollections.
//! - `i` + collection + field + value + doc id: single-field index entry.
//! - `l` + scope + seq: the local change log, read by sync catch-up.
//! - `h` + change hash: dedupe set (value is the log seq).
//! - `c` + peer key + scope: our sync cursor into that peer's log.
//! - `m/…`: node metadata.

use crate::value::Value;

pub const META_SEQ: &[u8] = b"m/seq";
pub const META_DEVICE: &[u8] = b"m/device";
pub const META_USER: &[u8] = b"m/user";

pub fn push_str(out: &mut Vec<u8>, s: &[u8]) {
    for &b in s {
        out.push(b);
        if b == 0 {
            out.push(0xff);
        }
    }
    out.push(0);
}

/// Reads one escaped element starting at `pos`. Returns the element and the
/// position just after its terminator.
pub fn read_str(key: &[u8], mut pos: usize) -> Option<(Vec<u8>, usize)> {
    let mut out = Vec::new();
    while pos < key.len() {
        let b = key[pos];
        if b == 0 {
            if key.get(pos + 1) == Some(&0xff) {
                out.push(0);
                pos += 2;
                continue;
            }
            return Some((out, pos + 1));
        }
        out.push(b);
        pos += 1;
    }
    None
}

pub fn doc_key(path: &str) -> Vec<u8> {
    let segs: Vec<&str> = path.split('/').collect();
    let mut k = vec![b'd', segs.len() as u8];
    for s in segs {
        push_str(&mut k, s.as_bytes());
    }
    k
}

/// Prefix shared by every direct child document of `collection`.
pub fn children_prefix(collection: &str) -> Vec<u8> {
    let segs: Vec<&str> = collection.split('/').collect();
    let mut k = vec![b'd', segs.len() as u8 + 1];
    for s in segs {
        push_str(&mut k, s.as_bytes());
    }
    k
}

pub fn decode_doc_key(key: &[u8]) -> Option<String> {
    if key.first() != Some(&b'd') {
        return None;
    }
    let n = *key.get(1)? as usize;
    let mut pos = 2;
    let mut segs = Vec::with_capacity(n);
    for _ in 0..n {
        let (s, p) = read_str(key, pos)?;
        segs.push(String::from_utf8(s).ok()?);
        pos = p;
    }
    Some(segs.join("/"))
}

pub fn index_prefix(collection: &str, field: &str) -> Vec<u8> {
    let mut k = vec![b'i'];
    push_str(&mut k, collection.as_bytes());
    push_str(&mut k, field.as_bytes());
    k
}

/// Appends the sortable encoding of an indexable value. Returns false for
/// values that are not indexed (arrays, maps, encrypted values).
pub fn push_index_value(out: &mut Vec<u8>, v: &Value) -> bool {
    match v {
        Value::Null => out.push(0x10),
        Value::Bool(false) => out.push(0x20),
        Value::Bool(true) => out.push(0x21),
        Value::Int(_) | Value::Float(_) => {
            out.push(0x30);
            out.extend_from_slice(&sortable_f64(v.as_f64().unwrap()));
        }
        Value::Str(s) => {
            out.push(0x40);
            push_str(out, s.as_bytes());
        }
        Value::Bytes(b) => {
            out.push(0x50);
            push_str(out, b);
        }
        Value::Array(_) | Value::Map(_) | Value::Encrypted(_) => return false,
    }
    true
}

fn sortable_f64(f: f64) -> [u8; 8] {
    let f = if f == 0.0 {
        0.0
    } else if f.is_nan() {
        f64::NAN
    } else {
        f
    };
    let bits = f.to_bits();
    let bits = if bits >> 63 == 1 {
        !bits
    } else {
        bits ^ (1 << 63)
    };
    bits.to_be_bytes()
}

pub fn index_key(collection: &str, field: &str, v: &Value, doc_id: &str) -> Option<Vec<u8>> {
    let mut k = index_prefix(collection, field);
    if !push_index_value(&mut k, v) {
        return None;
    }
    push_str(&mut k, doc_id.as_bytes());
    Some(k)
}

/// Extracts the doc id from an index key, given the length of its
/// `index_prefix`.
pub fn index_key_doc_id(key: &[u8], prefix_len: usize) -> Option<String> {
    let tag = *key.get(prefix_len)?;
    let pos = match tag {
        0x10 | 0x20 | 0x21 => prefix_len + 1,
        0x30 => prefix_len + 9,
        0x40 | 0x50 => read_str(key, prefix_len + 1)?.1,
        _ => return None,
    };
    String::from_utf8(read_str(key, pos)?.0).ok()
}

pub fn log_prefix(scope: &str) -> Vec<u8> {
    let mut k = vec![b'l'];
    push_str(&mut k, scope.as_bytes());
    k
}

pub fn log_key(scope: &str, seq: u64) -> Vec<u8> {
    let mut k = log_prefix(scope);
    k.extend_from_slice(&seq.to_be_bytes());
    k
}

pub fn log_key_seq(key: &[u8]) -> Option<u64> {
    let tail = key.get(key.len().checked_sub(8)?..)?;
    Some(u64::from_be_bytes(tail.try_into().ok()?))
}

pub fn hash_key(hash: &[u8; 32]) -> Vec<u8> {
    let mut k = vec![b'h'];
    k.extend_from_slice(hash);
    k
}

pub fn cursor_key(peer: &[u8; 32], scope: &str) -> Vec<u8> {
    let mut k = vec![b'c'];
    k.extend_from_slice(peer);
    push_str(&mut k, scope.as_bytes());
    k
}

/// The smallest key greater than every key starting with `prefix`.
pub fn prefix_end(prefix: &[u8]) -> Vec<u8> {
    let mut end = prefix.to_vec();
    while let Some(last) = end.pop() {
        if last < 0xff {
            end.push(last + 1);
            return end;
        }
    }
    vec![0xff; prefix.len() + 1]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn doc_keys_roundtrip_and_group_children() {
        let k = doc_key("public/a/todos/t\u{0}1");
        assert_eq!(decode_doc_key(&k).unwrap(), "public/a/todos/t\u{0}1");
        let p = children_prefix("public/a/todos");
        assert!(doc_key("public/a/todos/x").starts_with(&p));
        assert!(!doc_key("public/a/todos/x/sub/y").starts_with(&p));
    }

    #[test]
    fn index_order_matches_value_order() {
        let vals = [
            Value::Null,
            Value::Bool(false),
            Value::Bool(true),
            Value::Float(-1e9),
            Value::Int(-1),
            Value::Int(0),
            Value::Float(0.5),
            Value::Int(7),
            Value::Str("".into()),
            Value::Str("a".into()),
            Value::Str("a\u{0}".into()),
            Value::Str("b".into()),
            Value::Bytes(vec![0]),
        ];
        let keys: Vec<_> = vals
            .iter()
            .map(|v| index_key("c", "f", v, "id").unwrap())
            .collect();
        let mut sorted = keys.clone();
        sorted.sort();
        assert_eq!(keys, sorted);
        let plen = index_prefix("c", "f").len();
        for k in &keys {
            assert_eq!(index_key_doc_id(k, plen).unwrap(), "id");
        }
    }

    #[test]
    fn prefix_end_bounds() {
        assert_eq!(prefix_end(b"ab"), b"ac".to_vec());
        assert_eq!(prefix_end(&[1, 0xff]), vec![2]);
        let k = log_key("public/x", 42);
        assert_eq!(log_key_seq(&k), Some(42));
        assert!(k > log_prefix("public/x") && k < prefix_end(&log_prefix("public/x")));
    }
}
