//! Storage for aghanim-network.
//!
//! [`Kv`] is a small ordered key-value interface. It is synchronous on
//! purpose: the engine is sans-io and runs one operation at a time, and both
//! backends can serve it synchronously. Browsers use [`MemoryKv`] with a
//! journal that JS flushes to IndexedDB; relays use redb with a write overlay
//! committed once per operation.
//!
//! [`Store`] layers documents, automatic single-field indexes, the per-scope
//! change log, sync cursors, and query execution on top of a `Kv`.

use std::collections::{BTreeMap, BTreeSet};

use aghanim_core::keys;
use aghanim_core::path::split_doc;
use aghanim_core::query::{Dir, FilterOp};
use aghanim_core::{Change, Doc, Fields, Hash, Op, PubKey, Query, Value};

pub trait Kv {
    fn get(&self, key: &[u8]) -> Option<Vec<u8>>;
    fn put(&mut self, key: &[u8], value: &[u8]);
    fn delete(&mut self, key: &[u8]);
    /// Visits keys in `[start, end)`, ascending or descending, until `f`
    /// returns false.
    fn scan(
        &self,
        start: &[u8],
        end: &[u8],
        reverse: bool,
        f: &mut dyn FnMut(&[u8], &[u8]) -> bool,
    );
    /// Makes buffered writes durable. Called once per engine operation.
    fn commit(&mut self) {}
}

pub type JournalEntry = (Vec<u8>, Option<Vec<u8>>);

/// In-memory ordered map. With a journal enabled it records every write so
/// a host can persist them (the browser engine flushes it to IndexedDB).
#[derive(Default)]
pub struct MemoryKv {
    map: BTreeMap<Vec<u8>, Vec<u8>>,
    journal: Option<Vec<JournalEntry>>,
}

impl MemoryKv {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn with_journal() -> Self {
        MemoryKv {
            map: BTreeMap::new(),
            journal: Some(Vec::new()),
        }
    }

    /// Loads persisted entries without journaling them again.
    pub fn load(&mut self, entries: impl IntoIterator<Item = (Vec<u8>, Vec<u8>)>) {
        self.map.extend(entries);
    }

    pub fn take_journal(&mut self) -> Vec<JournalEntry> {
        self.journal
            .as_mut()
            .map(std::mem::take)
            .unwrap_or_default()
    }

    pub fn len(&self) -> usize {
        self.map.len()
    }

    pub fn is_empty(&self) -> bool {
        self.map.is_empty()
    }

    pub fn iter(&self) -> impl Iterator<Item = (&Vec<u8>, &Vec<u8>)> {
        self.map.iter()
    }
}

impl Kv for MemoryKv {
    fn get(&self, key: &[u8]) -> Option<Vec<u8>> {
        self.map.get(key).cloned()
    }

    fn put(&mut self, key: &[u8], value: &[u8]) {
        self.map.insert(key.to_vec(), value.to_vec());
        if let Some(j) = &mut self.journal {
            j.push((key.to_vec(), Some(value.to_vec())));
        }
    }

    fn delete(&mut self, key: &[u8]) {
        if self.map.remove(key).is_some() {
            if let Some(j) = &mut self.journal {
                j.push((key.to_vec(), None));
            }
        }
    }

    fn scan(
        &self,
        start: &[u8],
        end: &[u8],
        reverse: bool,
        f: &mut dyn FnMut(&[u8], &[u8]) -> bool,
    ) {
        if start >= end {
            return;
        }
        let range = self.map.range::<[u8], _>((
            std::ops::Bound::Included(start),
            std::ops::Bound::Excluded(end),
        ));
        if reverse {
            for (k, v) in range.rev() {
                if !f(k, v) {
                    break;
                }
            }
        } else {
            for (k, v) in range {
                if !f(k, v) {
                    break;
                }
            }
        }
    }
}

/// Collects keys in a prefix (convenience over [`Kv::scan`]).
pub fn scan_prefix<K: Kv + ?Sized>(kv: &K, prefix: &[u8]) -> Vec<(Vec<u8>, Vec<u8>)> {
    let mut out = Vec::new();
    kv.scan(prefix, &keys::prefix_end(prefix), false, &mut |k, v| {
        out.push((k.to_vec(), v.to_vec()));
        true
    });
    out
}

pub struct Store<K: Kv> {
    pub kv: K,
}

impl<K: Kv> Store<K> {
    pub fn new(kv: K) -> Self {
        Store { kv }
    }

    pub fn get_doc(&self, path: &str) -> Option<Doc> {
        let b = self.kv.get(&keys::doc_key(path))?;
        // Stored docs were written by us; a decode failure means corruption,
        // which we surface as a missing document rather than a crash.
        Doc::from_bytes(&b).ok()
    }

    /// Applies one op to its document and keeps the indexes in step.
    /// Returns true if the document changed.
    pub fn apply_op(&mut self, op: &Op, author: PubKey) -> bool {
        let mut doc = self.get_doc(&op.path).unwrap_or_default();
        let before = index_entries(&op.path, &doc);
        if !doc.apply(&op.kind, op.hlc, author) {
            return false;
        }
        let after = index_entries(&op.path, &doc);
        for k in before.difference(&after) {
            self.kv.delete(k);
        }
        for k in after.difference(&before) {
            self.kv.put(k, &[]);
        }
        self.kv.put(&keys::doc_key(&op.path), &doc.to_bytes());
        true
    }

    pub fn has_change(&self, hash: &Hash) -> bool {
        self.kv.get(&keys::hash_key(hash)).is_some()
    }

    fn next_seq(&mut self) -> u64 {
        let seq = self
            .kv
            .get(keys::META_SEQ)
            .and_then(|b| b.try_into().ok())
            .map(u64::from_be_bytes)
            .unwrap_or(0)
            + 1;
        self.kv.put(keys::META_SEQ, &seq.to_be_bytes());
        seq
    }

    /// Appends a change to the scope's log and records its hash.
    /// Returns its sequence number in our log.
    pub fn append_change(&mut self, scope: &str, change: &Change) -> u64 {
        let seq = self.next_seq();
        self.kv.put(&keys::log_key(scope, seq), &change.to_bytes());
        self.kv
            .put(&keys::hash_key(&change.hash()), &seq.to_be_bytes());
        seq
    }

    /// Up to `max` logged changes in `scope` with seq greater than `after`.
    pub fn changes_after(&self, scope: &str, after: u64, max: usize) -> Vec<(u64, Change)> {
        let prefix = keys::log_prefix(scope);
        let start = keys::log_key(scope, after.saturating_add(1));
        let mut out = Vec::new();
        self.kv
            .scan(&start, &keys::prefix_end(&prefix), false, &mut |k, v| {
                if let (Some(seq), Ok(c)) = (keys::log_key_seq(k), Change::from_bytes(v)) {
                    out.push((seq, c));
                }
                out.len() < max
            });
        out
    }

    pub fn last_seq(&self) -> u64 {
        self.kv
            .get(keys::META_SEQ)
            .and_then(|b| b.try_into().ok())
            .map(u64::from_be_bytes)
            .unwrap_or(0)
    }

    pub fn cursor(&self, peer: &PubKey, scope: &str) -> u64 {
        self.kv
            .get(&keys::cursor_key(peer, scope))
            .and_then(|b| b.try_into().ok())
            .map(u64::from_be_bytes)
            .unwrap_or(0)
    }

    pub fn set_cursor(&mut self, peer: &PubKey, scope: &str, seq: u64) {
        if seq > self.cursor(peer, scope) {
            self.kv
                .put(&keys::cursor_key(peer, scope), &seq.to_be_bytes());
        }
    }

    /// Runs a query. `view` turns a stored document into the fields the caller
    /// should see (the engine decrypts space documents here), or `None` to
    /// skip it. Set `use_index` to false for collections whose values are
    /// encrypted, since those are never indexed.
    pub fn query(
        &self,
        q: &Query,
        use_index: bool,
        view: &mut dyn FnMut(&str, &Doc) -> Option<Fields>,
    ) -> Vec<(String, Fields)> {
        let limit = q.limit.unwrap_or(usize::MAX);
        if limit == 0 {
            return vec![];
        }
        let mut load = |id: &str, out: &mut Vec<(String, Fields)>| {
            let path = format!("{}/{}", q.collection, id);
            if let Some(doc) = self.get_doc(&path) {
                if let Some(fields) = view(&path, &doc) {
                    if !fields.is_empty() && q.matches(&fields) {
                        out.push((path, fields));
                    }
                }
            }
        };
        let mut out = Vec::new();

        if use_index {
            if let Some((field, dir)) = &q.order_by {
                // Walk the orderBy index in order and stop at the limit.
                let prefix = keys::index_prefix(&q.collection, field);
                let plen = prefix.len();
                let mut ids = Vec::new();
                self.kv.scan(
                    &prefix,
                    &keys::prefix_end(&prefix),
                    *dir == Dir::Desc,
                    &mut |k, _| {
                        if let Some(id) = keys::index_key_doc_id(k, plen) {
                            ids.push(id);
                        }
                        true
                    },
                );
                for id in ids {
                    load(&id, &mut out);
                    if out.len() >= limit {
                        break;
                    }
                }
                sort_and_limit(q, &mut out, limit);
                return out;
            }
            if let Some(ids) = self.index_candidates(q) {
                for id in ids {
                    load(&id, &mut out);
                }
                sort_and_limit(q, &mut out, limit);
                return out;
            }
        }

        // Full collection scan, in doc id order.
        let prefix = keys::children_prefix(&q.collection);
        let mut paths = Vec::new();
        self.kv
            .scan(&prefix, &keys::prefix_end(&prefix), false, &mut |k, _| {
                if let Some(p) = keys::decode_doc_key(k) {
                    paths.push(p);
                }
                true
            });
        for p in paths {
            let id = split_doc(&p)
                .map(|(_, id)| id)
                .unwrap_or_default()
                .to_owned();
            load(&id, &mut out);
            if q.order_by.is_none() && out.len() >= limit {
                break;
            }
        }
        sort_and_limit(q, &mut out, limit);
        out
    }

    /// Candidate doc ids from the index for the first indexable filter.
    /// Candidates are a superset; every doc is re-checked against all filters.
    fn index_candidates(&self, q: &Query) -> Option<BTreeSet<String>> {
        let filter = q
            .filters
            .iter()
            .find(|f| f.op == FilterOp::Eq || f.op == FilterOp::In)
            .or_else(|| q.filters.iter().find(|f| f.op != FilterOp::Ne))?;
        let prefix = keys::index_prefix(&q.collection, &filter.field);
        let plen = prefix.len();
        let mut ranges: Vec<(Vec<u8>, Vec<u8>)> = Vec::new();
        let enc = |v: &Value| {
            let mut k = prefix.clone();
            keys::push_index_value(&mut k, v).then_some(k)
        };
        match filter.op {
            FilterOp::Eq => {
                let k = enc(&filter.value)?;
                ranges.push((k.clone(), keys::prefix_end(&k)));
            }
            FilterOp::In => {
                let Value::Array(opts) = &filter.value else {
                    return None;
                };
                for v in opts {
                    let k = enc(v)?;
                    ranges.push((k.clone(), keys::prefix_end(&k)));
                }
            }
            FilterOp::Lt | FilterOp::Le | FilterOp::Gt | FilterOp::Ge => {
                let k = enc(&filter.value)?;
                // Range filters only match values of the same type class.
                let class = k[plen] & 0xf0;
                let class_start = [prefix.as_slice(), &[class]].concat();
                let class_end = [prefix.as_slice(), &[class + 0x10]].concat();
                ranges.push(match filter.op {
                    FilterOp::Lt => (class_start, k),
                    FilterOp::Le => (class_start, keys::prefix_end(&k)),
                    FilterOp::Gt => (keys::prefix_end(&k), class_end),
                    _ => (k, class_end),
                });
            }
            FilterOp::Ne => return None,
        }
        let mut ids = BTreeSet::new();
        for (start, end) in ranges {
            self.kv.scan(&start, &end, false, &mut |k, _| {
                if let Some(id) = keys::index_key_doc_id(k, plen) {
                    ids.insert(id);
                }
                true
            });
        }
        Some(ids)
    }
}

fn sort_and_limit(q: &Query, out: &mut Vec<(String, Fields)>, limit: usize) {
    out.sort_by(|a, b| {
        let ida = split_doc(&a.0).map(|x| x.1).unwrap_or_default();
        let idb = split_doc(&b.0).map(|x| x.1).unwrap_or_default();
        q.compare((ida, &a.1), (idb, &b.1))
    });
    out.truncate(limit);
}

/// Index keys for a document's live, indexable field values.
fn index_entries(path: &str, doc: &Doc) -> BTreeSet<Vec<u8>> {
    let Some((coll, id)) = split_doc(path) else {
        return BTreeSet::new();
    };
    doc.visible()
        .filter_map(|(f, v)| keys::index_key(coll, f, v, id))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use aghanim_core::{Hlc, OpKind};

    fn set(store: &mut Store<MemoryKv>, path: &str, field: &str, v: Value, t: u64) {
        let op = Op {
            path: path.into(),
            hlc: Hlc::new(t, 0),
            kind: OpKind::Set {
                field: field.into(),
                value: v,
            },
        };
        store.apply_op(&op, [1; 32]);
    }

    fn ids(r: &[(String, Fields)]) -> Vec<&str> {
        r.iter().map(|(p, _)| split_doc(p).unwrap().1).collect()
    }

    fn plain(_: &str, d: &Doc) -> Option<Fields> {
        Some(d.visible_fields())
    }

    fn sample() -> Store<MemoryKv> {
        let mut s = Store::new(MemoryKv::new());
        let c = "public/app/todos";
        for (i, (done, n)) in [(false, 3), (true, 1), (false, 2), (false, 5)]
            .iter()
            .enumerate()
        {
            let p = format!("{c}/t{i}");
            set(&mut s, &p, "done", Value::Bool(*done), 1);
            set(&mut s, &p, "n", Value::Int(*n), 1);
        }
        // A doc in a subcollection must not show up in `todos` queries.
        set(
            &mut s,
            "public/app/todos/t0/sub/x",
            "done",
            Value::Bool(false),
            1,
        );
        s
    }

    #[test]
    fn query_plans_agree_with_full_scan() {
        let s = sample();
        let q = Query::new("public/app/todos");
        let cases = vec![
            q.clone(),
            q.clone().filter("done", FilterOp::Eq, Value::Bool(false)),
            q.clone().filter("n", FilterOp::Gt, Value::Int(1)).filter(
                "done",
                FilterOp::Ne,
                Value::Bool(true),
            ),
            q.clone().filter(
                "n",
                FilterOp::In,
                Value::Array(vec![Value::Int(1), Value::Int(5)]),
            ),
            q.clone().order("n", Dir::Asc).limit(2),
            q.clone().order("n", Dir::Desc),
            q.clone()
                .filter("n", FilterOp::Le, Value::Int(3))
                .order("n", Dir::Desc)
                .limit(2),
        ];
        for c in cases {
            let indexed = s.query(&c, true, &mut plain);
            let scanned = s.query(&c, false, &mut plain);
            assert_eq!(indexed, scanned, "query {c:?}");
        }
        assert_eq!(
            ids(&s.query(&q.clone().order("n", Dir::Asc), true, &mut plain)),
            ["t1", "t2", "t0", "t3"]
        );
        assert_eq!(
            ids(&s.query(
                &q.clone().filter("done", FilterOp::Eq, Value::Bool(false)),
                true,
                &mut plain
            )),
            ["t0", "t2", "t3"]
        );
    }

    #[test]
    fn index_follows_updates_and_deletes() {
        let mut s = sample();
        set(&mut s, "public/app/todos/t1", "done", Value::Bool(false), 2);
        s.apply_op(
            &Op {
                path: "public/app/todos/t0".into(),
                hlc: Hlc::new(3, 0),
                kind: OpKind::DelDoc,
            },
            [1; 32],
        );
        let q = Query::new("public/app/todos").filter("done", FilterOp::Eq, Value::Bool(false));
        assert_eq!(ids(&s.query(&q, true, &mut plain)), ["t1", "t2", "t3"]);
        // No index entries remain for the deleted doc.
        let leftover = scan_prefix(&s.kv, &keys::index_prefix("public/app/todos", "n"));
        assert_eq!(leftover.len(), 3);
    }

    #[test]
    fn log_and_cursors() {
        let mut s = Store::new(MemoryKv::new());
        let mk = |n: i64| Change {
            author: [1; 32],
            ops: vec![Op {
                path: "public/a/c/d".into(),
                hlc: Hlc::new(n as u64, 0),
                kind: OpKind::Set {
                    field: "f".into(),
                    value: Value::Int(n),
                },
            }],
            sig: [0; 64],
        };
        let s1 = s.append_change("public/a", &mk(1));
        let _ = s.append_change("public/b", &mk(2));
        let s3 = s.append_change("public/a", &mk(3));
        assert!(s.has_change(&mk(1).hash()));
        let after: Vec<u64> = s
            .changes_after("public/a", 0, 10)
            .iter()
            .map(|x| x.0)
            .collect();
        assert_eq!(after, vec![s1, s3]);
        assert_eq!(s.changes_after("public/a", s1, 10).len(), 1);
        s.set_cursor(&[2; 32], "public/a", 5);
        s.set_cursor(&[2; 32], "public/a", 4);
        assert_eq!(s.cursor(&[2; 32], "public/a"), 5);
    }

    #[test]
    fn journal_records_writes() {
        let mut kv = MemoryKv::with_journal();
        kv.put(b"a", b"1");
        kv.delete(b"a");
        kv.delete(b"missing");
        assert_eq!(
            kv.take_journal(),
            vec![(b"a".to_vec(), Some(b"1".to_vec())), (b"a".to_vec(), None)]
        );
        assert!(kv.take_journal().is_empty());
    }
}
