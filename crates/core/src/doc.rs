//! Documents, ops, changes, and the LWW merge.
//!
//! A [`Doc`] maps each field to a [`Cell`]. Merging keeps, per field, the cell
//! with the highest `(hlc, author, value bytes)` stamp. Deleting a whole
//! document records a document-level tombstone; a field is visible only if
//! its cell is newer than that tombstone. Both rules take a maximum, so the
//! merge is commutative, associative, and idempotent: peers that saw the same
//! ops in any order, with duplicates, end up with identical documents.

use std::cmp::Ordering;
use std::collections::BTreeMap;

use crate::cbor::{DResult, Dec, DecodeError, Enc};
use crate::hlc::Hlc;
use crate::path;
use crate::value::{Fields, Value};

pub type PubKey = [u8; 32];
pub type Sig = [u8; 64];
pub type Hash = [u8; 32];

#[derive(Debug, Clone, PartialEq)]
pub struct Cell {
    /// `None` is a field tombstone.
    pub value: Option<Value>,
    pub hlc: Hlc,
    pub author: PubKey,
}

impl Cell {
    fn value_bytes(&self) -> Vec<u8> {
        self.value.as_ref().map(Value::to_bytes).unwrap_or_default()
    }

    /// Total order used for LWW. The value bytes only matter when one author
    /// wrote two different values at the same timestamp (two devices sharing
    /// a key), and make the winner deterministic in that case too.
    pub fn stamp_cmp(&self, other: &Cell) -> Ordering {
        (self.hlc, self.author)
            .cmp(&(other.hlc, other.author))
            .then_with(|| self.value_bytes().cmp(&other.value_bytes()))
    }
}

#[derive(Debug, Clone, PartialEq, Default)]
pub struct Doc {
    pub fields: BTreeMap<String, Cell>,
    /// Document-level tombstone: fields stamped at or before it are hidden.
    pub deleted: Option<(Hlc, PubKey)>,
}

#[derive(Debug, Clone, PartialEq)]
pub enum OpKind {
    Set { field: String, value: Value },
    DelField { field: String },
    DelDoc,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Op {
    pub path: String,
    pub hlc: Hlc,
    pub kind: OpKind,
}

impl Doc {
    /// Applies one op. Returns true if the document changed.
    pub fn apply(&mut self, kind: &OpKind, hlc: Hlc, author: PubKey) -> bool {
        match kind {
            OpKind::Set { field, value } => self.put_cell(field, Some(value.clone()), hlc, author),
            OpKind::DelField { field } => self.put_cell(field, None, hlc, author),
            OpKind::DelDoc => {
                if self.deleted.is_none_or(|d| (hlc, author) > d) {
                    self.deleted = Some((hlc, author));
                    true
                } else {
                    false
                }
            }
        }
    }

    fn put_cell(&mut self, field: &str, value: Option<Value>, hlc: Hlc, author: PubKey) -> bool {
        let cell = Cell { value, hlc, author };
        match self.fields.get(field) {
            Some(old) if cell.stamp_cmp(old) != Ordering::Greater => false,
            _ => {
                self.fields.insert(field.to_owned(), cell);
                true
            }
        }
    }

    fn is_visible(&self, cell: &Cell) -> bool {
        cell.value.is_some() && self.deleted.is_none_or(|d| (cell.hlc, cell.author) > d)
    }

    /// The live fields of this document.
    pub fn visible(&self) -> impl Iterator<Item = (&String, &Value)> {
        self.fields
            .iter()
            .filter(|(_, c)| self.is_visible(c))
            .map(|(k, c)| (k, c.value.as_ref().unwrap()))
    }

    pub fn visible_fields(&self) -> Fields {
        self.visible()
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect()
    }

    pub fn get(&self, field: &str) -> Option<&Value> {
        self.fields
            .get(field)
            .filter(|c| self.is_visible(c))
            .and_then(|c| c.value.as_ref())
    }

    /// The live cell for a field, including its timestamp and author.
    pub fn cell(&self, field: &str) -> Option<&Cell> {
        self.fields.get(field).filter(|c| self.is_visible(c))
    }

    pub fn exists(&self) -> bool {
        self.visible().next().is_some()
    }

    pub fn encode(&self, e: &mut Enc) {
        e.array(2).map(self.fields.len());
        for (k, c) in &self.fields {
            e.str(k).array(3).u64(c.hlc.0).bytes(&c.author);
            match &c.value {
                Some(v) => v.encode(e),
                None => {
                    e.undefined();
                }
            }
        }
        match &self.deleted {
            Some((h, a)) => {
                e.array(2).u64(h.0).bytes(a);
            }
            None => {
                e.null();
            }
        }
    }

    pub fn to_bytes(&self) -> Vec<u8> {
        let mut e = Enc::new();
        self.encode(&mut e);
        e.into_bytes()
    }

    pub fn from_bytes(b: &[u8]) -> DResult<Doc> {
        let mut d = Dec::new(b);
        expect_len(d.array()?, 2)?;
        let n = d.map()?;
        let mut fields = BTreeMap::new();
        for _ in 0..n {
            let k = d.str()?.to_owned();
            expect_len(d.array()?, 3)?;
            let hlc = Hlc(d.u64()?);
            let author = d.array_n()?;
            // A stored `null` is `Value::Null`; tombstones are CBOR `undefined`.
            let value = match d.peek()? {
                crate::cbor::Kind::Undefined => {
                    d.null()?;
                    None
                }
                _ => Some(Value::decode(&mut d)?),
            };
            fields.insert(k, Cell { value, hlc, author });
        }
        let deleted = d.opt(|d| {
            expect_len(d.array()?, 2)?;
            Ok((Hlc(d.u64()?), d.array_n()?))
        })?;
        d.finish()?;
        Ok(Doc { fields, deleted })
    }
}

fn expect_len(n: usize, want: usize) -> DResult<()> {
    if n == want {
        Ok(())
    } else {
        Err(DecodeError(format!("expected array of {want}, found {n}")))
    }
}

impl Op {
    pub fn encode(&self, e: &mut Enc) {
        e.array(3).str(&self.path).u64(self.hlc.0);
        match &self.kind {
            OpKind::Set { field, value } => {
                e.array(3).u64(0).str(field);
                value.encode(e);
            }
            OpKind::DelField { field } => {
                e.array(2).u64(1).str(field);
            }
            OpKind::DelDoc => {
                e.array(1).u64(2);
            }
        }
    }

    pub fn decode(d: &mut Dec) -> DResult<Op> {
        expect_len(d.array()?, 3)?;
        let path = d.str()?.to_owned();
        let hlc = Hlc(d.u64()?);
        let n = d.array()?;
        let kind = match (d.u64()?, n) {
            (0, 3) => OpKind::Set {
                field: d.str()?.to_owned(),
                value: Value::decode(d)?,
            },
            (1, 2) => OpKind::DelField {
                field: d.str()?.to_owned(),
            },
            (2, 1) => OpKind::DelDoc,
            (k, n) => return Err(DecodeError(format!("bad op kind {k}/{n}"))),
        };
        Ok(Op { path, hlc, kind })
    }

    pub fn field(&self) -> Option<&str> {
        match &self.kind {
            OpKind::Set { field, .. } | OpKind::DelField { field } => Some(field),
            OpKind::DelDoc => None,
        }
    }
}

/// A signed batch of ops by one author, all within one scope.
#[derive(Debug, Clone, PartialEq)]
pub struct Change {
    pub author: PubKey,
    pub ops: Vec<Op>,
    pub sig: Sig,
}

pub const MAX_OPS_PER_CHANGE: usize = 500;
pub const MAX_FIELD_BYTES: usize = 1024;

impl Change {
    /// The bytes covered by the signature: `[author, ops]`.
    pub fn unsigned_bytes(author: &PubKey, ops: &[Op]) -> Vec<u8> {
        let mut e = Enc::new();
        e.array(2).bytes(author).array(ops.len());
        for op in ops {
            op.encode(&mut e);
        }
        e.into_bytes()
    }

    /// Content hash, used as the change id and as the signed message.
    pub fn hash_of(author: &PubKey, ops: &[Op]) -> Hash {
        let mut h = blake3::Hasher::new_derive_key("aghanim-network change v1");
        h.update(&Self::unsigned_bytes(author, ops));
        *h.finalize().as_bytes()
    }

    pub fn hash(&self) -> Hash {
        Self::hash_of(&self.author, &self.ops)
    }

    pub fn encode(&self, e: &mut Enc) {
        e.array(3).bytes(&self.author).array(self.ops.len());
        for op in &self.ops {
            op.encode(e);
        }
        e.bytes(&self.sig);
    }

    pub fn to_bytes(&self) -> Vec<u8> {
        let mut e = Enc::new();
        self.encode(&mut e);
        e.into_bytes()
    }

    pub fn decode(d: &mut Dec) -> DResult<Change> {
        expect_len(d.array()?, 3)?;
        let author = d.array_n()?;
        let n = d.array()?;
        if n == 0 || n > MAX_OPS_PER_CHANGE {
            return Err(DecodeError(format!(
                "change must have 1..={MAX_OPS_PER_CHANGE} ops"
            )));
        }
        let mut ops = Vec::with_capacity(n);
        for _ in 0..n {
            ops.push(Op::decode(d)?);
        }
        Ok(Change {
            author,
            ops,
            sig: d.array_n()?,
        })
    }

    pub fn from_bytes(b: &[u8]) -> DResult<Change> {
        let mut d = Dec::new(b);
        let c = Self::decode(&mut d)?;
        d.finish()?;
        Ok(c)
    }

    /// Checks structure: valid doc paths, one shared scope, sane field names.
    /// Returns the scope. Signature and authorization are checked elsewhere.
    pub fn check_shape(&self) -> Result<String, String> {
        let mut scope: Option<String> = None;
        for op in &self.ops {
            path::check_doc(&op.path).map_err(|e| e.to_string())?;
            let s = path::scope_of(&op.path).map_err(|e| e.to_string())?;
            match &scope {
                Some(prev) if *prev != s => return Err("ops span more than one scope".into()),
                None => scope = Some(s),
                _ => {}
            }
            if let Some(f) = op.field() {
                check_field_name(f)?;
            }
        }
        scope.ok_or_else(|| "empty change".into())
    }

    pub fn max_hlc(&self) -> Hlc {
        self.ops.iter().map(|o| o.hlc).max().unwrap_or_default()
    }
}

pub fn check_field_name(f: &str) -> Result<(), String> {
    if f.is_empty() || f.len() > MAX_FIELD_BYTES || f.contains('\0') {
        return Err(format!("invalid field name {f:?}"));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn set(f: &str, v: i64) -> OpKind {
        OpKind::Set {
            field: f.into(),
            value: Value::Int(v),
        }
    }

    #[test]
    fn newer_write_wins_and_old_one_is_ignored() {
        let mut d = Doc::default();
        assert!(d.apply(&set("a", 1), Hlc::new(2, 0), [1; 32]));
        assert!(!d.apply(&set("a", 0), Hlc::new(1, 0), [1; 32]));
        assert_eq!(d.get("a"), Some(&Value::Int(1)));
    }

    #[test]
    fn author_breaks_ties() {
        let mut a = Doc::default();
        a.apply(&set("x", 1), Hlc::new(5, 0), [1; 32]);
        a.apply(&set("x", 2), Hlc::new(5, 0), [2; 32]);
        let mut b = Doc::default();
        b.apply(&set("x", 2), Hlc::new(5, 0), [2; 32]);
        b.apply(&set("x", 1), Hlc::new(5, 0), [1; 32]);
        assert_eq!(a, b);
        assert_eq!(a.get("x"), Some(&Value::Int(2)));
    }

    #[test]
    fn doc_delete_hides_older_fields_only() {
        let mut d = Doc::default();
        d.apply(&set("a", 1), Hlc::new(1, 0), [1; 32]);
        d.apply(&OpKind::DelDoc, Hlc::new(2, 0), [1; 32]);
        assert!(!d.exists());
        d.apply(&set("b", 2), Hlc::new(3, 0), [1; 32]);
        assert_eq!(d.visible_fields().len(), 1);
        // A stale write to `a` from before the delete must not resurrect it.
        d.apply(&set("a", 9), Hlc::new(1, 5), [1; 32]);
        assert_eq!(d.get("a"), None);
    }

    #[test]
    fn doc_roundtrip_keeps_null_and_tombstones_apart() {
        let mut d = Doc::default();
        d.apply(
            &OpKind::Set {
                field: "n".into(),
                value: Value::Null,
            },
            Hlc::new(1, 0),
            [1; 32],
        );
        d.apply(
            &OpKind::DelField { field: "t".into() },
            Hlc::new(1, 0),
            [1; 32],
        );
        d.apply(&OpKind::DelDoc, Hlc::new(0, 1), [1; 32]);
        let back = Doc::from_bytes(&d.to_bytes()).unwrap();
        assert_eq!(back, d);
        assert_eq!(back.get("n"), Some(&Value::Null));
    }

    #[test]
    fn change_roundtrip_and_shape() {
        let c = Change {
            author: [3; 32],
            ops: vec![
                Op {
                    path: "public/p/c/d".into(),
                    hlc: Hlc::new(1, 0),
                    kind: set("a", 1),
                },
                Op {
                    path: "public/p/c/e".into(),
                    hlc: Hlc::new(1, 1),
                    kind: OpKind::DelDoc,
                },
            ],
            sig: [9; 64],
        };
        assert_eq!(Change::from_bytes(&c.to_bytes()).unwrap(), c);
        assert_eq!(c.check_shape().unwrap(), "public/p");
        let mut bad = c.clone();
        bad.ops[1].path = "public/q/c/e".into();
        assert!(bad.check_shape().is_err());
    }
}
