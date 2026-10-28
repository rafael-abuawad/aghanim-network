//! redb-backed [`Kv`]. Writes go to an in-memory overlay and are committed in
//! one transaction per engine operation; reads see the overlay first.

use std::collections::BTreeMap;
use std::ops::Bound;
use std::sync::Arc;

use aghanim_store::Kv;
use redb::{Database, TableDefinition};

pub struct RedbKv {
    db: Arc<Database>,
    table: String,
    overlay: BTreeMap<Vec<u8>, Option<Vec<u8>>>,
}

impl RedbKv {
    pub fn new(db: Arc<Database>, table: impl Into<String>) -> Self {
        RedbKv {
            db,
            table: table.into(),
            overlay: BTreeMap::new(),
        }
    }

    fn def(&self) -> TableDefinition<'_, &'static [u8], &'static [u8]> {
        TableDefinition::new(&self.table)
    }

    fn db_range(&self, start: &[u8], end: &[u8], reverse: bool) -> Vec<(Vec<u8>, Vec<u8>)> {
        let Ok(tx) = self.db.begin_read() else {
            return vec![];
        };
        let Ok(t) = tx.open_table(self.def()) else {
            return vec![];
        };
        let Ok(range) = t.range::<&[u8]>(start..end) else {
            return vec![];
        };
        let it = range
            .filter_map(Result::ok)
            .map(|(k, v)| (k.value().to_vec(), v.value().to_vec()));
        if reverse {
            it.rev().collect()
        } else {
            it.collect()
        }
    }
}

impl Kv for RedbKv {
    fn get(&self, key: &[u8]) -> Option<Vec<u8>> {
        if let Some(v) = self.overlay.get(key) {
            return v.clone();
        }
        let tx = self.db.begin_read().ok()?;
        let t = tx.open_table(self.def()).ok()?;
        let v = t.get(key).ok()??.value().to_vec();
        Some(v)
    }

    fn put(&mut self, key: &[u8], value: &[u8]) {
        self.overlay.insert(key.to_vec(), Some(value.to_vec()));
    }

    fn delete(&mut self, key: &[u8]) {
        self.overlay.insert(key.to_vec(), None);
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
        // Merge the committed range with the overlay; the overlay wins on
        // equal keys, and `None` hides a committed key.
        let disk = self.db_range(start, end, reverse);
        let over: Vec<(&Vec<u8>, &Option<Vec<u8>>)> = {
            let r = self
                .overlay
                .range::<[u8], _>((Bound::Included(start), Bound::Excluded(end)));
            if reverse {
                r.rev().collect()
            } else {
                r.collect()
            }
        };
        let before = |a: &[u8], b: &[u8]| if reverse { a > b } else { a < b };
        let (mut i, mut j) = (0, 0);
        loop {
            let next: Option<(&[u8], Option<&[u8]>)> = match (disk.get(i), over.get(j)) {
                (None, None) => return,
                (Some((k, v)), None) => {
                    i += 1;
                    Some((k, Some(v)))
                }
                (None, Some((k, v))) => {
                    j += 1;
                    Some((k, v.as_deref()))
                }
                (Some((dk, dv)), Some((ok, ov))) => {
                    if dk.as_slice() == ok.as_slice() {
                        i += 1;
                        j += 1;
                        Some((ok, ov.as_deref()))
                    } else if before(dk, ok) {
                        i += 1;
                        Some((dk, Some(dv)))
                    } else {
                        j += 1;
                        Some((ok, ov.as_deref()))
                    }
                }
            };
            if let Some((k, Some(v))) = next {
                if !f(k, v) {
                    return;
                }
            }
        }
    }

    fn commit(&mut self) {
        if self.overlay.is_empty() {
            return;
        }
        let overlay = std::mem::take(&mut self.overlay);
        let tx = self.db.begin_write().expect("redb: begin write");
        {
            let mut t = tx.open_table(self.def()).expect("redb: open table");
            for (k, v) in &overlay {
                match v {
                    Some(v) => {
                        t.insert(k.as_slice(), v.as_slice()).expect("redb: insert");
                    }
                    None => {
                        t.remove(k.as_slice()).expect("redb: remove");
                    }
                }
            }
        }
        tx.commit().expect("redb: commit");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn overlay_and_disk_merge() {
        let dir = tempfile::tempdir().unwrap();
        let db = Arc::new(Database::create(dir.path().join("t.redb")).unwrap());
        let mut kv = RedbKv::new(db.clone(), "app:t");
        kv.put(b"a", b"1");
        kv.put(b"c", b"3");
        kv.commit();
        kv.put(b"b", b"2");
        kv.delete(b"c");
        kv.put(b"d", b"4");
        let mut seen = vec![];
        kv.scan(b"a", b"z", false, &mut |k, v| {
            seen.push((k.to_vec(), v.to_vec()));
            true
        });
        assert_eq!(
            seen,
            vec![
                (b"a".to_vec(), b"1".to_vec()),
                (b"b".to_vec(), b"2".to_vec()),
                (b"d".to_vec(), b"4".to_vec())
            ]
        );
        let mut rev = vec![];
        kv.scan(b"a", b"z", true, &mut |k, _| {
            rev.push(k.to_vec());
            rev.len() < 2
        });
        assert_eq!(rev, vec![b"d".to_vec(), b"b".to_vec()]);
        kv.commit();
        let kv2 = RedbKv::new(db, "app:t");
        assert_eq!(kv2.get(b"c"), None);
        assert_eq!(kv2.get(b"d"), Some(b"4".to_vec()));
    }
}
