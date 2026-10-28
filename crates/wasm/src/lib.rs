//! WebAssembly bindings for the aghanim-network engine.
//!
//! Structured data crosses the JS boundary as CBOR bytes (the SDK uses
//! cbor-x), which keeps the bindings small and avoids per-field JS object
//! conversion. Storage is an in-memory map whose write journal the host
//! flushes to IndexedDB after each call (see `take_journal`).

use aghanim_core::cbor::{Dec, Enc};
use aghanim_core::value::{decode_field_updates, encode_fields};
use aghanim_core::{Fields, Query};
use aghanim_crypto::{identity_from_passphrase, Identity, KdfParams};
use aghanim_store::MemoryKv;
use aghanim_sync::{Config, ConnOpts, Effect, Node, Write};
use js_sys::{Array, Object, Reflect, Uint8Array};
use wasm_bindgen::prelude::*;

fn err(e: impl std::fmt::Display) -> JsError {
    JsError::new(&e.to_string())
}

fn set(obj: &Object, k: &str, v: impl Into<JsValue>) {
    let _ = Reflect::set(obj, &JsValue::from_str(k), &v.into());
}

fn effects_to_js(fx: Vec<Effect>) -> Array {
    let out = Array::new();
    for e in fx {
        let o = Object::new();
        match e {
            Effect::Send { conn, bytes } => {
                set(&o, "type", "send");
                set(&o, "conn", conn);
                set(&o, "data", Uint8Array::from(bytes.as_slice()));
            }
            Effect::Changed { paths } => {
                set(&o, "type", "changed");
                set(
                    &o,
                    "paths",
                    paths.into_iter().map(JsValue::from).collect::<Array>(),
                );
            }
            Effect::PeerReady { conn, peer } => {
                set(&o, "type", "ready");
                set(&o, "conn", conn);
                set(&o, "peer", hex::encode(peer));
            }
            Effect::Synced { conn, scope } => {
                set(&o, "type", "synced");
                set(&o, "conn", conn);
                set(&o, "scope", scope);
            }
            Effect::Close { conn, reason } => {
                set(&o, "type", "close");
                set(&o, "conn", conn);
                set(&o, "reason", reason);
            }
        }
        out.push(&o);
    }
    out
}

fn fields_to_cbor(f: &Fields) -> Vec<u8> {
    let mut e = Enc::new();
    encode_fields(&mut e, f);
    e.into_bytes()
}

/// Decodes the SDK's write batch: an array of
/// `["set", path, fields, merge] | ["update", path, fields] | ["delete", path]`.
fn decode_writes(b: &[u8]) -> Result<Vec<Write>, JsError> {
    let mut d = Dec::new(b);
    let n = d.array().map_err(err)?;
    let mut out = Vec::with_capacity(n);
    for _ in 0..n {
        let len = d.array().map_err(err)?;
        let kind = d.str().map_err(err)?.to_owned();
        let path = d.str().map_err(err)?.to_owned();
        out.push(match (kind.as_str(), len) {
            ("set", 4) => {
                let fields = decode_field_updates(&mut d).map_err(err)?;
                Write::Set {
                    path,
                    fields,
                    merge: d.bool().map_err(err)?,
                }
            }
            ("update", 3) => Write::Update {
                path,
                fields: decode_field_updates(&mut d).map_err(err)?,
            },
            ("delete", 2) => Write::Delete { path },
            _ => return Err(JsError::new(&format!("bad write {kind:?}"))),
        });
    }
    d.finish().map_err(err)?;
    Ok(out)
}

#[wasm_bindgen]
pub struct Engine {
    node: Node<MemoryKv>,
}

#[wasm_bindgen]
impl Engine {
    /// Opens an engine, restoring persisted entries (`keys[i]` → `values[i]`).
    #[wasm_bindgen(constructor)]
    pub fn new(app_id: String, keys: Array, values: Array) -> Result<Engine, JsError> {
        if keys.length() != values.length() {
            return Err(JsError::new("keys and values must have the same length"));
        }
        let mut kv = MemoryKv::with_journal();
        kv.load((0..keys.length()).map(|i| {
            (
                Uint8Array::new(&keys.get(i)).to_vec(),
                Uint8Array::new(&values.get(i)).to_vec(),
            )
        }));
        Ok(Engine {
            node: Node::open(kv, Config::new(app_id)),
        })
    }

    #[wasm_bindgen(js_name = deviceId)]
    pub fn device_id(&self) -> String {
        hex::encode(self.node.device_id())
    }

    #[wasm_bindgen(js_name = userId)]
    pub fn user_id(&self) -> Option<String> {
        self.node.user().map(Identity::public_hex)
    }

    /// Passphrase login (Argon2id, ~19 MiB). Returns the user's public key.
    pub fn login(
        &mut self,
        alias: &str,
        passphrase: &str,
        remember: bool,
    ) -> Result<String, JsError> {
        let id = identity_from_passphrase(alias, passphrase, KdfParams::DEFAULT).map_err(err)?;
        let pk = id.public_hex();
        self.node.set_user(Some(id), remember);
        Ok(pk)
    }

    #[wasm_bindgen(js_name = loginWithSeed)]
    pub fn login_with_seed(&mut self, seed: &[u8], remember: bool) -> Result<String, JsError> {
        let id = Identity::from_seed_slice(seed).map_err(err)?;
        let pk = id.public_hex();
        self.node.set_user(Some(id), remember);
        Ok(pk)
    }

    #[wasm_bindgen(js_name = createRandomUser)]
    pub fn create_random_user(&mut self, remember: bool) -> String {
        let id = Identity::random();
        let pk = id.public_hex();
        self.node.set_user(Some(id), remember);
        pk
    }

    /// The signed-in user's secret seed, for backup or moving to a new device.
    #[wasm_bindgen(js_name = exportSeed)]
    pub fn export_seed(&self) -> Option<Vec<u8>> {
        self.node.user().map(|u| u.seed().to_vec())
    }

    pub fn logout(&mut self) {
        self.node.set_user(None, false);
    }

    pub fn write(&mut self, writes: &[u8], now: f64) -> Result<Array, JsError> {
        let w = decode_writes(writes)?;
        Ok(effects_to_js(self.node.write(w, now as u64).map_err(err)?))
    }

    /// The document's fields as CBOR, or undefined if it doesn't exist.
    pub fn get(&self, path: &str) -> Result<Option<Vec<u8>>, JsError> {
        Ok(self
            .node
            .get(path)
            .map_err(err)?
            .map(|f| fields_to_cbor(&f)))
    }

    /// Runs a CBOR-encoded query; returns CBOR `[[path, fields], …]`.
    pub fn query(&self, query: &[u8]) -> Result<Vec<u8>, JsError> {
        let mut d = Dec::new(query);
        let q = Query::decode(&mut d).map_err(err)?;
        let rows = self.node.query(&q).map_err(err)?;
        let mut e = Enc::new();
        e.array(rows.len());
        for (path, fields) in &rows {
            e.array(2).str(path);
            encode_fields(&mut e, fields);
        }
        Ok(e.into_bytes())
    }

    pub fn subscribe(&mut self, scope: &str) -> Result<Array, JsError> {
        Ok(effects_to_js(self.node.subscribe(scope).map_err(err)?))
    }

    pub fn connect(&mut self, conn: u32, sync: bool, replicate: bool) -> Array {
        effects_to_js(self.node.connect(conn, ConnOpts { sync, replicate }))
    }

    pub fn disconnect(&mut self, conn: u32) {
        self.node.disconnect(conn);
    }

    pub fn receive(&mut self, conn: u32, bytes: &[u8], now: f64) -> Array {
        effects_to_js(self.node.receive(conn, bytes, now as u64))
    }

    #[wasm_bindgen(js_name = peerOf)]
    pub fn peer_of(&self, conn: u32) -> Option<String> {
        self.node.peer_of(conn).map(hex::encode)
    }

    /// Returns `[spaceId, effects]`.
    #[wasm_bindgen(js_name = createSpace)]
    pub fn create_space(&mut self, name: &str, now: f64) -> Result<Array, JsError> {
        let (id, fx) = self.node.create_space(name, now as u64).map_err(err)?;
        Ok(Array::of2(&JsValue::from(id), &effects_to_js(fx)))
    }

    pub fn invite(
        &mut self,
        space: &str,
        member: &str,
        role: &str,
        now: f64,
    ) -> Result<Array, JsError> {
        Ok(effects_to_js(
            self.node
                .invite(space, member, role, now as u64)
                .map_err(err)?,
        ))
    }

    #[wasm_bindgen(js_name = removeMember)]
    pub fn remove_member(&mut self, space: &str, member: &str, now: f64) -> Result<Array, JsError> {
        Ok(effects_to_js(
            self.node
                .remove_member(space, member, now as u64)
                .map_err(err)?,
        ))
    }

    /// CBOR `[[pubkey, role, active], …]`.
    pub fn members(&self, space: &str) -> Result<Vec<u8>, JsError> {
        let ms = self.node.members(space).map_err(err)?;
        let mut e = Enc::new();
        e.array(ms.len());
        for m in ms {
            e.array(3).str(&m.pubkey).str(&m.role).bool(m.active);
        }
        Ok(e.into_bytes())
    }

    /// Raw stored document for debugging (CBOR), including ciphertext.
    #[wasm_bindgen(js_name = rawDoc)]
    pub fn raw_doc(&self, path: &str) -> Option<Vec<u8>> {
        self.node.store.get_doc(path).map(|d| d.to_bytes())
    }

    /// Writes since the last call, as `[[key, value | null], …]`, for the
    /// host to persist.
    #[wasm_bindgen(js_name = takeJournal)]
    pub fn take_journal(&mut self) -> Array {
        self.node
            .store
            .kv
            .take_journal()
            .into_iter()
            .map(|(k, v)| {
                let val = v.map_or(JsValue::NULL, |v| Uint8Array::from(v.as_slice()).into());
                JsValue::from(Array::of2(&Uint8Array::from(k.as_slice()).into(), &val))
            })
            .collect()
    }
}
