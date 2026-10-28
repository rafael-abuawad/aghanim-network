//! Write authorization. Every peer runs these rules on every change, local or
//! remote, so no server has to be trusted.
//!
//! - `public/…`: anyone may write (writes are still signed).
//! - `users/{pk}/…`: only the key `pk` may write.
//! - `spaces/{id}/…`: the space document `spaces/{id}` is its ACL:
//!   - `owner` (hex key) and `salt` (bytes) are set once, by the owner, and
//!     must satisfy `id == space_id(owner, salt)`. Only the owner may change
//!     any field of the ACL document.
//!   - `m.{pk}` = `{role: "writer"|"reader", from: hlc, until?: hlc}` grants
//!     a member. Writers may write content stamped in `[from, until)`.
//!   - `epoch` is the current key epoch and `k.{epoch}.{pk}` holds the space
//!     key for that epoch, wrapped to member `pk`.
//!   - Content values (anything below the ACL document) must be encrypted.

use aghanim_core::path::{self, Namespace};
use aghanim_core::{Change, Doc, Hlc, OpKind, Value};
use aghanim_crypto::space_id;
use aghanim_store::{Kv, Store};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Verdict {
    Allow,
    /// Can't decide yet: the space's ACL (or this member's grant) hasn't
    /// arrived. The change is held and retried when the ACL changes.
    Pending,
    Deny(String),
}

pub const ROLE_WRITER: &str = "writer";
pub const ROLE_READER: &str = "reader";

pub fn member_field(pk_hex: &str) -> String {
    format!("m.{pk_hex}")
}

pub fn key_field(epoch: u32, pk_hex: &str) -> String {
    format!("k.{epoch}.{pk_hex}")
}

pub fn acl_path(space: &str) -> String {
    format!("spaces/{space}")
}

#[derive(Debug, Clone, PartialEq)]
pub struct Grant {
    pub role: String,
    pub from: Hlc,
    pub until: Option<Hlc>,
}

impl Grant {
    pub fn parse(v: &Value) -> Option<Grant> {
        let Value::Map(m) = v else { return None };
        Some(Grant {
            role: m.get("role")?.as_str()?.to_owned(),
            from: Hlc(m.get("from")?.as_i64()? as u64),
            until: m
                .get("until")
                .and_then(Value::as_i64)
                .map(|u| Hlc(u as u64)),
        })
    }

    pub fn to_value(&self) -> Value {
        let mut m = std::collections::BTreeMap::new();
        m.insert("role".into(), Value::Str(self.role.clone()));
        m.insert("from".into(), Value::Int(self.from.0 as i64));
        if let Some(u) = self.until {
            m.insert("until".into(), Value::Int(u.0 as i64));
        }
        Value::Map(m)
    }

    pub fn active(&self) -> bool {
        self.until.is_none()
    }

    fn can_write_at(&self, h: Hlc) -> bool {
        self.role == ROLE_WRITER && self.from <= h && self.until.is_none_or(|u| h < u)
    }
}

pub fn owner_of(acl: &Doc) -> Option<&str> {
    acl.get("owner")?.as_str()
}

pub fn authorize<K: Kv>(store: &Store<K>, change: &Change) -> Verdict {
    let Some(first) = change.ops.first() else {
        return Verdict::Deny("empty change".into());
    };
    let Ok(segs) = path::segments(&first.path) else {
        return Verdict::Deny("bad path".into());
    };
    let author_hex = hex::encode(change.author);
    match path::namespace(&segs) {
        Err(e) => Verdict::Deny(e.to_string()),
        Ok(Namespace::Public) => Verdict::Allow,
        Ok(Namespace::Users(pk)) => {
            if pk == author_hex {
                Verdict::Allow
            } else {
                Verdict::Deny(format!("only {pk} may write to users/{pk}"))
            }
        }
        Ok(Namespace::Spaces(id)) => authorize_space(store, change, id, &author_hex),
    }
}

fn authorize_space<K: Kv>(
    store: &Store<K>,
    change: &Change,
    id: &str,
    author_hex: &str,
) -> Verdict {
    let acl_path = acl_path(id);
    let acl = store.get_doc(&acl_path);
    let stored_owner = acl.as_ref().and_then(owner_of).map(str::to_owned);

    // Does this change create the space?
    let created_here = stored_owner.is_none() && creates_space(change, id, author_hex);
    let owner = stored_owner
        .clone()
        .or_else(|| created_here.then(|| author_hex.to_owned()));
    let mut pending = false;

    for op in &change.ops {
        if op.path == acl_path {
            match &owner {
                None => pending = true,
                Some(o) if o != author_hex => {
                    return Verdict::Deny("only the space owner may change its ACL".into())
                }
                Some(_) => {
                    if stored_owner.is_some() && touches_identity(&op.kind) {
                        return Verdict::Deny("a space's owner and salt can't change".into());
                    }
                    if matches!(op.kind, OpKind::DelDoc) {
                        return Verdict::Deny("a space's ACL document can't be deleted".into());
                    }
                }
            }
            continue;
        }

        // Content below the ACL document.
        if let OpKind::Set { value, .. } = &op.kind {
            if !matches!(value, Value::Encrypted(_)) {
                return Verdict::Deny("space content must be encrypted".into());
            }
        }
        match &owner {
            None => pending = true,
            Some(o) if o == author_hex => {}
            Some(_) => {
                let grant = acl
                    .as_ref()
                    .and_then(|d| d.get(&member_field(author_hex)))
                    .and_then(Grant::parse);
                match grant {
                    None => pending = true,
                    Some(g) if g.can_write_at(op.hlc) => {}
                    Some(_) => {
                        return Verdict::Deny("not a writer of this space at that time".into())
                    }
                }
            }
        }
    }
    if pending {
        Verdict::Pending
    } else {
        Verdict::Allow
    }
}

fn creates_space(change: &Change, id: &str, author_hex: &str) -> bool {
    let acl = acl_path(id);
    let mut owner_ok = false;
    let mut salt_ok = false;
    for op in change.ops.iter().filter(|o| o.path == acl) {
        if let OpKind::Set { field, value } = &op.kind {
            match (field.as_str(), value) {
                ("owner", Value::Str(o)) => owner_ok = o == author_hex,
                ("salt", Value::Bytes(s)) => salt_ok = space_id(&change.author, s) == id,
                _ => {}
            }
        }
    }
    owner_ok && salt_ok
}

fn touches_identity(kind: &OpKind) -> bool {
    match kind {
        OpKind::Set { field, .. } | OpKind::DelField { field } => {
            field == "owner" || field == "salt"
        }
        OpKind::DelDoc => true,
    }
}
