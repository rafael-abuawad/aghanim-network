//! Queries over one collection: `where` filters, one `orderBy`, and `limit`.

use std::cmp::Ordering;

use crate::cbor::{DResult, Dec, DecodeError, Enc};
use crate::path;
use crate::value::{Fields, Value};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FilterOp {
    Eq,
    Ne,
    Lt,
    Le,
    Gt,
    Ge,
    In,
}

impl FilterOp {
    pub fn parse(s: &str) -> Option<Self> {
        Some(match s {
            "==" => Self::Eq,
            "!=" => Self::Ne,
            "<" => Self::Lt,
            "<=" => Self::Le,
            ">" => Self::Gt,
            ">=" => Self::Ge,
            "in" => Self::In,
            _ => return None,
        })
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Eq => "==",
            Self::Ne => "!=",
            Self::Lt => "<",
            Self::Le => "<=",
            Self::Gt => ">",
            Self::Ge => ">=",
            Self::In => "in",
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct Filter {
    pub field: String,
    pub op: FilterOp,
    pub value: Value,
}

impl Filter {
    pub fn matches(&self, fields: &Fields) -> bool {
        let Some(v) = fields.get(&self.field) else {
            return false;
        };
        let ord = || v.same_class(&self.value).then(|| v.total_cmp(&self.value));
        match self.op {
            FilterOp::Eq => v.query_eq(&self.value),
            FilterOp::Ne => !v.query_eq(&self.value),
            FilterOp::Lt => ord() == Some(Ordering::Less),
            FilterOp::Le => matches!(ord(), Some(Ordering::Less | Ordering::Equal)),
            FilterOp::Gt => ord() == Some(Ordering::Greater),
            FilterOp::Ge => matches!(ord(), Some(Ordering::Greater | Ordering::Equal)),
            FilterOp::In => match &self.value {
                Value::Array(opts) => opts.iter().any(|o| v.query_eq(o)),
                _ => false,
            },
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Dir {
    Asc,
    Desc,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Query {
    pub collection: String,
    pub filters: Vec<Filter>,
    pub order_by: Option<(String, Dir)>,
    pub limit: Option<usize>,
}

impl Query {
    pub fn new(collection: impl Into<String>) -> Self {
        Query {
            collection: collection.into(),
            filters: vec![],
            order_by: None,
            limit: None,
        }
    }

    pub fn filter(mut self, field: &str, op: FilterOp, value: Value) -> Self {
        self.filters.push(Filter {
            field: field.into(),
            op,
            value,
        });
        self
    }

    pub fn order(mut self, field: &str, dir: Dir) -> Self {
        self.order_by = Some((field.into(), dir));
        self
    }

    pub fn limit(mut self, n: usize) -> Self {
        self.limit = Some(n);
        self
    }

    pub fn validate(&self) -> Result<(), String> {
        path::check_collection(&self.collection).map_err(|e| e.to_string())?;
        for f in &self.filters {
            if f.op == FilterOp::In && !matches!(f.value, Value::Array(_)) {
                return Err(format!("'in' filter on {:?} needs an array", f.field));
            }
        }
        Ok(())
    }

    /// True if a document's fields pass every filter, and it has the
    /// `orderBy` field (Firestore drops documents without it).
    pub fn matches(&self, fields: &Fields) -> bool {
        if let Some((f, _)) = &self.order_by {
            if !fields.contains_key(f) {
                return false;
            }
        }
        self.filters.iter().all(|f| f.matches(fields))
    }

    /// Result order: by the `orderBy` field, then by document id.
    pub fn compare(&self, a: (&str, &Fields), b: (&str, &Fields)) -> Ordering {
        if let Some((f, dir)) = &self.order_by {
            let o = match (a.1.get(f), b.1.get(f)) {
                (Some(x), Some(y)) => x.total_cmp(y),
                (x, y) => x.is_some().cmp(&y.is_some()),
            };
            let o = if *dir == Dir::Desc { o.reverse() } else { o };
            if o != Ordering::Equal {
                return o;
            }
        }
        a.0.cmp(b.0)
    }

    /// Decodes the JS SDK's query shape:
    /// `{collection, where?: [[field, op, value]…], orderBy?: [field, "asc"|"desc"], limit?}`.
    pub fn decode(d: &mut Dec) -> DResult<Query> {
        let mut q = Query::new("");
        for _ in 0..d.map()? {
            match d.str()? {
                "collection" => q.collection = d.str()?.to_owned(),
                "where" => {
                    for _ in 0..d.array()? {
                        if d.array()? != 3 {
                            return Err(DecodeError(
                                "where clause must be [field, op, value]".into(),
                            ));
                        }
                        let field = d.str()?.to_owned();
                        let op = d.str()?;
                        let op = FilterOp::parse(op)
                            .ok_or_else(|| DecodeError(format!("unknown operator {op:?}")))?;
                        q.filters.push(Filter {
                            field,
                            op,
                            value: Value::decode(d)?,
                        });
                    }
                }
                "orderBy" => {
                    q.order_by = d.opt(|d| {
                        if d.array()? != 2 {
                            return Err(DecodeError("orderBy must be [field, dir]".into()));
                        }
                        let f = d.str()?.to_owned();
                        let dir = match d.str()? {
                            "asc" => Dir::Asc,
                            "desc" => Dir::Desc,
                            o => return Err(DecodeError(format!("bad direction {o:?}"))),
                        };
                        Ok((f, dir))
                    })?
                }
                "limit" => q.limit = d.opt(|d| Ok(d.u64()? as usize))?,
                _ => d.skip()?,
            }
        }
        Ok(q)
    }

    pub fn encode(&self, e: &mut Enc) {
        e.map(4)
            .str("collection")
            .str(&self.collection)
            .str("where")
            .array(self.filters.len());
        for f in &self.filters {
            e.array(3).str(&f.field).str(f.op.as_str());
            f.value.encode(e);
        }
        e.str("orderBy");
        match &self.order_by {
            Some((f, d)) => {
                e.array(2)
                    .str(f)
                    .str(if *d == Dir::Asc { "asc" } else { "desc" });
            }
            None => {
                e.null();
            }
        }
        e.str("limit");
        match self.limit {
            Some(n) => e.u64(n as u64),
            None => e.null(),
        };
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn doc(pairs: &[(&str, Value)]) -> Fields {
        pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.clone()))
            .collect()
    }

    #[test]
    fn filters() {
        let d = doc(&[("n", Value::Int(5)), ("s", Value::Str("x".into()))]);
        let q = |op, v| Query::new("public/a/c").filter("n", op, v);
        assert!(q(FilterOp::Eq, Value::Float(5.0)).matches(&d));
        assert!(q(FilterOp::Gt, Value::Int(4)).matches(&d));
        assert!(!q(FilterOp::Gt, Value::Str("a".into())).matches(&d));
        assert!(q(
            FilterOp::In,
            Value::Array(vec![Value::Int(1), Value::Int(5)])
        )
        .matches(&d));
        assert!(!Query::new("public/a/c")
            .filter("zz", FilterOp::Ne, Value::Null)
            .matches(&d));
        assert!(!Query::new("public/a/c").order("zz", Dir::Asc).matches(&d));
    }

    #[test]
    fn roundtrip() {
        let q = Query::new("public/a/c")
            .filter("done", FilterOp::Eq, Value::Bool(false))
            .order("at", Dir::Desc)
            .limit(3);
        let mut e = Enc::new();
        q.encode(&mut e);
        let b = e.into_bytes();
        assert_eq!(Query::decode(&mut Dec::new(&b)).unwrap(), q);
    }
}
