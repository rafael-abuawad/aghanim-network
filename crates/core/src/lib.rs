//! Sans-io data model for aghanim-network.
//!
//! Nothing here does I/O: the crate defines values, clocks, documents and
//! their merge, store key layout, queries, and the encoding of signed changes.

pub mod cbor;
pub mod doc;
pub mod hlc;
pub mod keys;
pub mod path;
pub mod query;
pub mod value;

pub use doc::{Cell, Change, Doc, Hash, Op, OpKind, PubKey, Sig};
pub use hlc::{Clock, Hlc};
pub use query::{Dir, Filter, FilterOp, Query};
pub use value::{Fields, Sealed, Value};
