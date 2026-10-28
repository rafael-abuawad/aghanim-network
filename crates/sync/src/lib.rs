//! Sans-io sync for aghanim-network.
//!
//! [`Node`] owns the local store and every peer session. Protocol, per
//! connection (reliable and ordered, e.g. a WebRTC DataChannel or WebSocket):
//!
//! 1. Both sides send `Hello {app, device key, nonce}` and answer with
//!    `Proof`, a signature over the other side's nonce.
//! 2. `Sub {scope, cursor}` asks for every change in a scope after `cursor`
//!    (a seq in the responder's log). The responder streams `Changes`, then
//!    `Synced`, then pushes new changes live. Cursors are saved per peer, so
//!    reconnecting only transfers the gap.
//! 3. Every change is verified (shape, signature, clock drift, namespace
//!    rules) before it is stored or forwarded; duplicates are dropped by hash,
//!    which stops gossip loops.

pub mod acl;
pub mod msg;
pub mod node;

pub use node::{Config, ConnId, ConnOpts, Effect, Error, Member, Node, Updates, Write};
