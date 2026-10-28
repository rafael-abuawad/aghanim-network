//! Wire messages. Each frame is one CBOR array `[type, …fields]`.

use aghanim_core::cbor::{DResult, Dec, DecodeError, Enc};
use aghanim_core::{Change, PubKey, Sig};

pub const PROTOCOL_VERSION: u64 = 1;
/// Maximum changes per `Changes` frame during catch-up.
pub const CATCHUP_CHUNK: usize = 128;

#[derive(Debug, Clone, PartialEq)]
pub enum Msg {
    /// Opens a session: who we are, plus a fresh nonce for the peer to sign.
    Hello {
        version: u64,
        app: String,
        peer: PubKey,
        nonce: [u8; 32],
    },
    /// Signature over our nonce, proving the peer holds its key.
    Proof {
        sig: Sig,
    },
    /// Asks the peer for every change in `scope` after `cursor` (a seq in the
    /// peer's log), followed by live pushes.
    Sub {
        scope: String,
        cursor: u64,
    },
    Unsub {
        scope: String,
    },
    /// Changes with their seq in the sender's log.
    Changes {
        scope: String,
        items: Vec<(u64, Change)>,
    },
    /// Catch-up for `scope` is complete up to `cursor`.
    Synced {
        scope: String,
        cursor: u64,
    },
}

impl Msg {
    pub fn to_bytes(&self) -> Vec<u8> {
        let mut e = Enc::new();
        match self {
            Msg::Hello {
                version,
                app,
                peer,
                nonce,
            } => {
                e.array(5)
                    .u64(0)
                    .u64(*version)
                    .str(app)
                    .bytes(peer)
                    .bytes(nonce);
            }
            Msg::Proof { sig } => {
                e.array(2).u64(1).bytes(sig);
            }
            Msg::Sub { scope, cursor } => {
                e.array(3).u64(2).str(scope).u64(*cursor);
            }
            Msg::Unsub { scope } => {
                e.array(2).u64(3).str(scope);
            }
            Msg::Changes { scope, items } => {
                e.array(3).u64(4).str(scope).array(items.len());
                for (seq, c) in items {
                    e.array(2).u64(*seq);
                    c.encode(&mut e);
                }
            }
            Msg::Synced { scope, cursor } => {
                e.array(3).u64(5).str(scope).u64(*cursor);
            }
        }
        e.into_bytes()
    }

    pub fn from_bytes(b: &[u8]) -> DResult<Msg> {
        let mut d = Dec::new(b);
        let n = d.array()?;
        let t = d.u64()?;
        let bad = || Err(DecodeError(format!("bad message type {t} with {n} fields")));
        let m = match (t, n) {
            (0, 5) => Msg::Hello {
                version: d.u64()?,
                app: d.str()?.to_owned(),
                peer: d.array_n()?,
                nonce: d.array_n()?,
            },
            (1, 2) => Msg::Proof { sig: d.array_n()? },
            (2, 3) => Msg::Sub {
                scope: d.str()?.to_owned(),
                cursor: d.u64()?,
            },
            (3, 2) => Msg::Unsub {
                scope: d.str()?.to_owned(),
            },
            (4, 3) => {
                let scope = d.str()?.to_owned();
                let k = d.array()?;
                if k > 4 * CATCHUP_CHUNK {
                    return Err(DecodeError("too many changes in one frame".into()));
                }
                let mut items = Vec::with_capacity(k);
                for _ in 0..k {
                    if d.array()? != 2 {
                        return Err(DecodeError("bad change item".into()));
                    }
                    items.push((d.u64()?, Change::decode(&mut d)?));
                }
                Msg::Changes { scope, items }
            }
            (5, 3) => Msg::Synced {
                scope: d.str()?.to_owned(),
                cursor: d.u64()?,
            },
            _ => return bad(),
        };
        d.finish()?;
        Ok(m)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use aghanim_core::{Hlc, Op, OpKind, Value};

    #[test]
    fn roundtrip_all() {
        let c = Change {
            author: [1; 32],
            ops: vec![Op {
                path: "public/a/b/c".into(),
                hlc: Hlc::new(1, 2),
                kind: OpKind::Set {
                    field: "x".into(),
                    value: Value::Bool(true),
                },
            }],
            sig: [2; 64],
        };
        for m in [
            Msg::Hello {
                version: 1,
                app: "app".into(),
                peer: [3; 32],
                nonce: [4; 32],
            },
            Msg::Proof { sig: [5; 64] },
            Msg::Sub {
                scope: "public/a".into(),
                cursor: 9,
            },
            Msg::Unsub {
                scope: "public/a".into(),
            },
            Msg::Changes {
                scope: "public/a".into(),
                items: vec![(7, c)],
            },
            Msg::Synced {
                scope: "public/a".into(),
                cursor: 7,
            },
        ] {
            assert_eq!(Msg::from_bytes(&m.to_bytes()).unwrap(), m);
        }
        assert!(Msg::from_bytes(&[0x82, 0x09, 0x00]).is_err());
    }
}
