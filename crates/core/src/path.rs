//! Document and collection paths.
//!
//! Paths alternate collection and document ids, like Firestore:
//! `collection/docId[/subcollection/docId…]`. The first two segments form the
//! path's *scope*, which is the unit of sync and authorization. The root
//! collection picks the namespace and its write rules.

use std::fmt;

pub const MAX_PATH_BYTES: usize = 1024;
pub const MAX_SEGMENTS: usize = 64;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PathError(pub String);

impl fmt::Display for PathError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "invalid path: {}", self.0)
    }
}

impl std::error::Error for PathError {}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Namespace<'a> {
    /// `public/{name}/…`: anyone may write.
    Public,
    /// `users/{pubkeyHex}/…`: only that key may write.
    Users(&'a str),
    /// `spaces/{spaceId}/…`: members only, values encrypted.
    Spaces(&'a str),
}

pub fn segments(path: &str) -> Result<Vec<&str>, PathError> {
    if path.is_empty() || path.len() > MAX_PATH_BYTES {
        return Err(PathError(format!(
            "length must be 1..={MAX_PATH_BYTES} bytes"
        )));
    }
    let segs: Vec<&str> = path.split('/').collect();
    if segs.len() > MAX_SEGMENTS {
        return Err(PathError("too many segments".into()));
    }
    for s in &segs {
        if s.is_empty() {
            return Err(PathError(format!("empty segment in {path:?}")));
        }
        if s.contains('\0') || *s == "." || *s == ".." {
            return Err(PathError(format!("bad segment {s:?}")));
        }
    }
    Ok(segs)
}

/// Validates a document path (an even number of segments, at least two).
pub fn check_doc(path: &str) -> Result<Vec<&str>, PathError> {
    let segs = segments(path)?;
    if segs.len() % 2 != 0 {
        return Err(PathError(format!(
            "{path:?} is a collection path, not a document path"
        )));
    }
    namespace(&segs)?;
    Ok(segs)
}

/// Validates a collection path. Root collections are not queryable on their
/// own, so this requires at least three segments (`scope/collection`).
pub fn check_collection(path: &str) -> Result<Vec<&str>, PathError> {
    let segs = segments(path)?;
    if segs.len() % 2 != 1 {
        return Err(PathError(format!(
            "{path:?} is a document path, not a collection path"
        )));
    }
    if segs.len() < 3 {
        return Err(PathError(format!(
            "{path:?}: collections must live inside a scope, e.g. public/<name>/<collection>"
        )));
    }
    namespace(&segs)?;
    Ok(segs)
}

pub fn namespace<'a>(segs: &[&'a str]) -> Result<Namespace<'a>, PathError> {
    if segs.len() < 2 {
        return Err(PathError("path must have at least two segments".into()));
    }
    match segs[0] {
        "public" => Ok(Namespace::Public),
        "users" => {
            let pk = segs[1];
            if pk.len() != 64
                || !pk
                    .bytes()
                    .all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase())
            {
                return Err(PathError(format!(
                    "users/{pk}: expected a lowercase hex public key"
                )));
            }
            Ok(Namespace::Users(pk))
        }
        "spaces" => Ok(Namespace::Spaces(segs[1])),
        other => Err(PathError(format!(
            "root collection must be public, users or spaces, not {other:?}"
        ))),
    }
}

/// The scope (first two segments) of a document or collection path.
pub fn scope_of(path: &str) -> Result<String, PathError> {
    let segs = segments(path)?;
    namespace(&segs)?;
    Ok(format!("{}/{}", segs[0], segs[1]))
}

/// Checks a scope string such as `spaces/abc`.
pub fn check_scope(scope: &str) -> Result<(), PathError> {
    let segs = segments(scope)?;
    if segs.len() != 2 {
        return Err(PathError(format!("{scope:?} is not a scope")));
    }
    namespace(&segs).map(|_| ())
}

/// Splits a document path into (parent collection path, document id).
pub fn split_doc(path: &str) -> Option<(&str, &str)> {
    path.rsplit_once('/')
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn doc_and_collection_paths() {
        assert!(check_doc("public/chat/messages/m1").is_ok());
        assert!(check_doc("public/chat/messages").is_err());
        assert!(check_collection("public/chat/messages").is_ok());
        assert!(check_collection("public").is_err());
        assert!(check_doc("other/x").is_err());
        assert!(check_doc("public//x/y").is_err());
        assert!(check_doc(&format!("users/{}/notes/n", "ab".repeat(32))).is_ok());
        assert!(check_doc("users/ABC/notes/n").is_err());
    }

    #[test]
    fn scope() {
        assert_eq!(scope_of("spaces/s1/todos/t1").unwrap(), "spaces/s1");
        assert!(check_scope("spaces/s1").is_ok());
        assert!(check_scope("spaces/s1/todos").is_err());
    }
}
