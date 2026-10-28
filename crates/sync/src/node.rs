//! The sync node: local database, write path, and peer sessions.
//!
//! The node never does I/O. Hosts feed it API calls, incoming frames, and the
//! current wall-clock time, and carry out the [`Effect`]s it returns.

use std::cell::RefCell;
use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::fmt;

use aghanim_core::keys;
use aghanim_core::path::{self, Namespace};
use aghanim_core::{Change, Doc, Fields, Hash, Hlc, Op, OpKind, PubKey, Query, Value};
use aghanim_crypto::{self as crypto, Identity, SymKey};
use aghanim_store::{Kv, Store};

use crate::acl::{self, Grant, Verdict};
use crate::msg::{Msg, CATCHUP_CHUNK, PROTOCOL_VERSION};

pub type ConnId = u32;

#[derive(Debug, Clone)]
pub struct Config {
    pub app_id: String,
    /// Reject changes stamped further than this ahead of our clock.
    pub max_drift_ms: u64,
    /// Relays: store and serve every scope any peer subscribes to.
    pub replicate_all: bool,
    /// Max changes held while waiting for a space ACL, per scope.
    pub max_pending: usize,
}

impl Config {
    pub fn new(app_id: impl Into<String>) -> Self {
        Config {
            app_id: app_id.into(),
            max_drift_ms: aghanim_core::hlc::DEFAULT_MAX_DRIFT_MS,
            replicate_all: false,
            max_pending: 1000,
        }
    }
}

#[derive(Debug, Clone, Copy)]
pub struct ConnOpts {
    /// Exchange data on this connection (false: handshake only, e.g. a relay
    /// used just for signaling).
    pub sync: bool,
    /// Subscribe back to every scope this peer subscribes to, so we store and
    /// relay data for it (relays, and the leader tab for follower tabs).
    pub replicate: bool,
}

impl Default for ConnOpts {
    fn default() -> Self {
        ConnOpts {
            sync: true,
            replicate: false,
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum Effect {
    Send {
        conn: ConnId,
        bytes: Vec<u8>,
    },
    /// These documents changed (from a local write or a peer).
    Changed {
        paths: Vec<String>,
    },
    /// The handshake finished; `peer` is the authenticated device key.
    PeerReady {
        conn: ConnId,
        peer: PubKey,
    },
    /// The peer finished sending its backlog for `scope`.
    Synced {
        conn: ConnId,
        scope: String,
    },
    /// Protocol violation: the host should drop the connection.
    Close {
        conn: ConnId,
        reason: String,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Error {
    NotSignedIn,
    PermissionDenied(String),
    NotFound(String),
    Invalid(String),
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::NotSignedIn => write!(f, "not signed in"),
            Error::PermissionDenied(m) => write!(f, "permission denied: {m}"),
            Error::NotFound(m) => write!(f, "not found: {m}"),
            Error::Invalid(m) => write!(f, "invalid argument: {m}"),
        }
    }
}

impl std::error::Error for Error {}

pub type Result<T> = std::result::Result<T, Error>;

/// Field updates: `None` deletes the field.
pub type Updates = BTreeMap<String, Option<Value>>;

#[derive(Debug, Clone, PartialEq)]
pub enum Write {
    /// Replace the document, or with `merge`, only the given fields.
    Set {
        path: String,
        fields: Updates,
        merge: bool,
    },
    /// Change fields of a document that must already exist.
    Update {
        path: String,
        fields: Updates,
    },
    Delete {
        path: String,
    },
}

impl Write {
    fn path(&self) -> &str {
        match self {
            Write::Set { path, .. } | Write::Update { path, .. } | Write::Delete { path } => path,
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct Member {
    pub pubkey: String,
    pub role: String,
    pub active: bool,
}

struct Session {
    opts: ConnOpts,
    my_nonce: [u8; 32],
    peer: Option<PubKey>,
    authed: bool,
    /// Scopes the peer subscribed to on us: we push changes in these.
    subs_in: BTreeSet<String>,
    /// Scopes we subscribed to on the peer.
    subs_out: BTreeSet<String>,
}

/// The signed-in user and their unwrapped space keys.
#[derive(Default)]
struct Keyring {
    user: Option<Identity>,
    cache: RefCell<HashMap<(String, u32), SymKey>>,
}

impl Keyring {
    fn space_key<K: Kv>(&self, store: &Store<K>, space: &str, epoch: u32) -> Option<SymKey> {
        if let Some(k) = self.cache.borrow().get(&(space.to_owned(), epoch)) {
            return Some(*k);
        }
        let user = self.user.as_ref()?;
        let acl = store.get_doc(&acl::acl_path(space))?;
        let Value::Bytes(wrapped) = acl.get(&acl::key_field(epoch, &user.public_hex()))? else {
            return None;
        };
        let key = crypto::unwrap_key(user, wrapped, &crypto::wrap_context(space, epoch))?;
        self.cache
            .borrow_mut()
            .insert((space.to_owned(), epoch), key);
        Some(key)
    }

    /// What a reader sees: space content is decrypted, and fields we can't
    /// decrypt are left out.
    fn view<K: Kv>(&self, store: &Store<K>, path: &str, doc: &Doc) -> Fields {
        let space = space_content(path);
        doc.visible()
            .filter_map(|(f, v)| match (v, space) {
                (Value::Encrypted(s), Some(space)) => {
                    let key = self.space_key(store, space, s.epoch)?;
                    Some((f.clone(), crypto::decrypt_value(&key, path, f, s)?))
                }
                (Value::Encrypted(_), None) => None,
                _ => Some((f.clone(), v.clone())),
            })
            .collect()
    }
}

/// For a path below a space's ACL document, the space id.
fn space_content(path: &str) -> Option<&str> {
    let rest = path.strip_prefix("spaces/")?;
    let (id, tail) = rest.split_once('/')?;
    (!tail.is_empty()).then_some(id)
}

pub struct Node<K: Kv> {
    pub store: Store<K>,
    config: Config,
    device: Identity,
    keys: Keyring,
    clock: aghanim_core::Clock,
    sessions: HashMap<ConnId, Session>,
    interests: BTreeSet<String>,
    pending: BTreeMap<String, Vec<Change>>,
    fx: Vec<Effect>,
    changed: BTreeSet<String>,
}

impl<K: Kv> Node<K> {
    /// Opens a node over `kv`, restoring (or creating) its device key and any
    /// saved login.
    pub fn open(kv: K, config: Config) -> Self {
        let mut store = Store::new(kv);
        let device = match store
            .kv
            .get(keys::META_DEVICE)
            .and_then(|s| Identity::from_seed_slice(&s).ok())
        {
            Some(d) => d,
            None => {
                let d = Identity::random();
                store.kv.put(keys::META_DEVICE, &d.seed());
                d
            }
        };
        let user = store
            .kv
            .get(keys::META_USER)
            .and_then(|s| Identity::from_seed_slice(&s).ok());
        store.kv.commit();
        Node {
            store,
            clock: aghanim_core::Clock::new(config.max_drift_ms),
            config,
            device,
            keys: Keyring {
                user,
                cache: Default::default(),
            },
            sessions: HashMap::new(),
            interests: BTreeSet::new(),
            pending: BTreeMap::new(),
            fx: Vec::new(),
            changed: BTreeSet::new(),
        }
    }

    pub fn config(&self) -> &Config {
        &self.config
    }

    pub fn device_id(&self) -> PubKey {
        self.device.public()
    }

    pub fn user(&self) -> Option<&Identity> {
        self.keys.user.as_ref()
    }

    /// Signs in as `user` (or signs out with `None`). With `remember`, the
    /// secret seed is saved in local storage so the login survives restarts.
    pub fn set_user(&mut self, user: Option<Identity>, remember: bool) {
        match (&user, remember) {
            (Some(u), true) => self.store.kv.put(keys::META_USER, &u.seed()),
            _ => self.store.kv.delete(keys::META_USER),
        }
        self.store.kv.commit();
        self.keys = Keyring {
            user,
            cache: Default::default(),
        };
    }

    pub fn peer_of(&self, conn: ConnId) -> Option<PubKey> {
        self.sessions
            .get(&conn)
            .filter(|s| s.authed)
            .and_then(|s| s.peer)
    }

    // -----------------------------------------------------------------------
    // Reads

    pub fn get(&self, path: &str) -> Result<Option<Fields>> {
        path::check_doc(path).map_err(|e| Error::Invalid(e.to_string()))?;
        Ok(self
            .store
            .get_doc(path)
            .map(|d| self.keys.view(&self.store, path, &d))
            .filter(|f| !f.is_empty()))
    }

    pub fn query(&self, q: &Query) -> Result<Vec<(String, Fields)>> {
        q.validate().map_err(Error::Invalid)?;
        let encrypted = q.collection.starts_with("spaces/");
        Ok(self.store.query(q, !encrypted, &mut |p, d| {
            Some(self.keys.view(&self.store, p, d))
        }))
    }

    // -----------------------------------------------------------------------
    // Writes

    /// Applies writes atomically per scope, signs them, and pushes them to
    /// subscribed peers.
    pub fn write(&mut self, writes: Vec<Write>, now_ms: u64) -> Result<Vec<Effect>> {
        let user = self.keys.user.clone().ok_or(Error::NotSignedIn)?;
        let mut by_scope: Vec<(String, Vec<Op>)> = Vec::new();
        for w in &writes {
            path::check_doc(w.path()).map_err(|e| Error::Invalid(e.to_string()))?;
            let scope = path::scope_of(w.path()).map_err(|e| Error::Invalid(e.to_string()))?;
            let ops = self.ops_for(w, now_ms)?;
            match by_scope.iter_mut().find(|(s, _)| *s == scope) {
                Some((_, v)) => v.extend(ops),
                None => by_scope.push((scope, ops)),
            }
        }
        for (_, ops) in by_scope {
            for chunk in ops.chunks(aghanim_core::doc::MAX_OPS_PER_CHANGE) {
                self.commit_local(&user, chunk.to_vec())?;
            }
        }
        Ok(self.finish())
    }

    fn ops_for(&mut self, w: &Write, now_ms: u64) -> Result<Vec<Op>> {
        let path = w.path().to_owned();
        let mut ops = Vec::new();
        let fields = match w {
            Write::Delete { .. } => {
                ops.push(Op {
                    path,
                    hlc: self.clock.now(now_ms),
                    kind: OpKind::DelDoc,
                });
                return Ok(ops);
            }
            Write::Update { fields, .. } => {
                if !self.store.get_doc(&path).is_some_and(|d| d.exists()) {
                    return Err(Error::NotFound(path));
                }
                fields
            }
            Write::Set {
                fields,
                merge: false,
                ..
            } => {
                ops.push(Op {
                    path: path.clone(),
                    hlc: self.clock.now(now_ms),
                    kind: OpKind::DelDoc,
                });
                fields
            }
            Write::Set { fields, .. } => fields,
        };
        let space = space_content(&path).map(str::to_owned);
        let key = match &space {
            Some(s) => Some(self.current_space_key(s)?),
            None => None,
        };
        for (field, value) in fields {
            aghanim_core::doc::check_field_name(field).map_err(Error::Invalid)?;
            let hlc = self.clock.now(now_ms);
            let kind = match value {
                None if matches!(w, Write::Set { merge: false, .. }) => continue,
                None => OpKind::DelField {
                    field: field.clone(),
                },
                Some(v) => {
                    let v = match &key {
                        Some((epoch, k)) => crypto::encrypt_value(k, *epoch, &path, field, v),
                        None => v.clone(),
                    };
                    OpKind::Set {
                        field: field.clone(),
                        value: v,
                    }
                }
            };
            ops.push(Op {
                path: path.clone(),
                hlc,
                kind,
            });
        }
        Ok(ops)
    }

    fn current_space_key(&self, space: &str) -> Result<(u32, SymKey)> {
        let acl = self
            .store
            .get_doc(&acl::acl_path(space))
            .ok_or_else(|| Error::NotFound(format!("space {space} (not synced yet?)")))?;
        let epoch = acl.get("epoch").and_then(Value::as_i64).unwrap_or(1) as u32;
        let key = self
            .keys
            .space_key(&self.store, space, epoch)
            .ok_or_else(|| Error::PermissionDenied(format!("no key for space {space}")))?;
        Ok((epoch, key))
    }

    fn commit_local(&mut self, user: &Identity, ops: Vec<Op>) -> Result<()> {
        let change = crypto::sign_change(user, ops);
        let scope = change.check_shape().map_err(Error::Invalid)?;
        match acl::authorize(&self.store, &change) {
            Verdict::Allow => {}
            Verdict::Pending => {
                return Err(Error::PermissionDenied(format!(
                    "{scope}: space ACL not available yet"
                )))
            }
            Verdict::Deny(m) => return Err(Error::PermissionDenied(m)),
        }
        self.apply(change, &scope, None);
        Ok(())
    }

    // -----------------------------------------------------------------------
    // Spaces

    /// Creates an encrypted space owned by the signed-in user. Returns its id.
    pub fn create_space(&mut self, name: &str, now_ms: u64) -> Result<(String, Vec<Effect>)> {
        let user = self.keys.user.clone().ok_or(Error::NotSignedIn)?;
        let salt: [u8; 16] = crypto::random_bytes();
        let id = crypto::space_id(&user.public(), &salt);
        let key: SymKey = crypto::random_bytes();
        let wrapped = crypto::wrap_key(&user.public(), &key, &crypto::wrap_context(&id, 1))
            .map_err(|e| Error::Invalid(e.to_string()))?;
        let me = user.public_hex();
        let fields = [
            ("owner".to_owned(), Value::Str(me.clone())),
            ("salt".to_owned(), Value::Bytes(salt.to_vec())),
            ("name".to_owned(), Value::Str(name.to_owned())),
            ("epoch".to_owned(), Value::Int(1)),
            (acl::key_field(1, &me), Value::Bytes(wrapped)),
        ];
        let path = acl::acl_path(&id);
        let ops = fields
            .into_iter()
            .map(|(field, value)| Op {
                path: path.clone(),
                hlc: self.clock.now(now_ms),
                kind: OpKind::Set { field, value },
            })
            .collect();
        self.commit_local(&user, ops)?;
        self.interests.insert(format!("spaces/{id}"));
        self.subscribe_all(&format!("spaces/{id}"));
        Ok((id, self.finish()))
    }

    fn owned_acl(&self, space: &str) -> Result<(Identity, Doc)> {
        let user = self.keys.user.clone().ok_or(Error::NotSignedIn)?;
        let acl = self
            .store
            .get_doc(&acl::acl_path(space))
            .ok_or_else(|| Error::NotFound(format!("space {space}")))?;
        if acl::owner_of(&acl) != Some(&user.public_hex()) {
            return Err(Error::PermissionDenied(
                "only the owner can manage members".into(),
            ));
        }
        Ok((user, acl))
    }

    /// Adds a member and gives them every key epoch so far.
    pub fn invite(
        &mut self,
        space: &str,
        member_hex: &str,
        role: &str,
        now_ms: u64,
    ) -> Result<Vec<Effect>> {
        if role != acl::ROLE_WRITER && role != acl::ROLE_READER {
            return Err(Error::Invalid(format!(
                "role must be writer or reader, not {role:?}"
            )));
        }
        let member =
            crypto::parse_pubkey_hex(member_hex).map_err(|e| Error::Invalid(e.to_string()))?;
        let (user, acl_doc) = self.owned_acl(space)?;
        let epoch = acl_doc.get("epoch").and_then(Value::as_i64).unwrap_or(1) as u32;
        let path = acl::acl_path(space);
        let mut ops = Vec::new();
        for e in 1..=epoch {
            let Some(key) = self.keys.space_key(&self.store, space, e) else {
                continue;
            };
            let wrapped = crypto::wrap_key(&member, &key, &crypto::wrap_context(space, e))
                .map_err(|e| Error::Invalid(e.to_string()))?;
            ops.push(Op {
                path: path.clone(),
                hlc: self.clock.now(now_ms),
                kind: OpKind::Set {
                    field: acl::key_field(e, member_hex),
                    value: Value::Bytes(wrapped),
                },
            });
        }
        let h = self.clock.now(now_ms);
        let grant = Grant {
            role: role.to_owned(),
            from: h,
            until: None,
        };
        ops.push(Op {
            path,
            hlc: h,
            kind: OpKind::Set {
                field: acl::member_field(member_hex),
                value: grant.to_value(),
            },
        });
        self.commit_local(&user, ops)?;
        Ok(self.finish())
    }

    /// Removes a member and rotates the space key, so they can't read
    /// anything written afterwards.
    pub fn remove_member(
        &mut self,
        space: &str,
        member_hex: &str,
        now_ms: u64,
    ) -> Result<Vec<Effect>> {
        let (user, acl_doc) = self.owned_acl(space)?;
        let mfield = acl::member_field(member_hex);
        let grant = acl_doc
            .get(&mfield)
            .and_then(Grant::parse)
            .filter(Grant::active)
            .ok_or_else(|| Error::NotFound(format!("member {member_hex}")))?;
        let path = acl::acl_path(space);
        let epoch = acl_doc.get("epoch").and_then(Value::as_i64).unwrap_or(1) as u32 + 1;
        let key: SymKey = crypto::random_bytes();
        let h = self.clock.now(now_ms);
        let mut ops = vec![Op {
            path: path.clone(),
            hlc: h,
            kind: OpKind::Set {
                field: mfield,
                value: Grant {
                    until: Some(h),
                    ..grant
                }
                .to_value(),
            },
        }];
        let mut recipients = vec![user.public_hex()];
        recipients.extend(
            self.members_of(&acl_doc)
                .into_iter()
                .filter(|m| m.active && m.pubkey != member_hex)
                .map(|m| m.pubkey),
        );
        for pk in recipients {
            let pk_bytes =
                crypto::parse_pubkey_hex(&pk).map_err(|e| Error::Invalid(e.to_string()))?;
            let wrapped = crypto::wrap_key(&pk_bytes, &key, &crypto::wrap_context(space, epoch))
                .map_err(|e| Error::Invalid(e.to_string()))?;
            ops.push(Op {
                path: path.clone(),
                hlc: self.clock.now(now_ms),
                kind: OpKind::Set {
                    field: acl::key_field(epoch, &pk),
                    value: Value::Bytes(wrapped),
                },
            });
        }
        ops.push(Op {
            path,
            hlc: self.clock.now(now_ms),
            kind: OpKind::Set {
                field: "epoch".into(),
                value: Value::Int(epoch as i64),
            },
        });
        self.commit_local(&user, ops)?;
        Ok(self.finish())
    }

    fn members_of(&self, acl_doc: &Doc) -> Vec<Member> {
        acl_doc
            .visible()
            .filter_map(|(f, v)| {
                let pk = f.strip_prefix("m.")?;
                let g = Grant::parse(v)?;
                Some(Member {
                    pubkey: pk.to_owned(),
                    active: g.active(),
                    role: g.role,
                })
            })
            .collect()
    }

    /// The owner and members of a space, as currently known locally.
    pub fn members(&self, space: &str) -> Result<Vec<Member>> {
        let acl_doc = self
            .store
            .get_doc(&acl::acl_path(space))
            .ok_or_else(|| Error::NotFound(format!("space {space}")))?;
        let mut out = Vec::new();
        if let Some(o) = acl::owner_of(&acl_doc) {
            out.push(Member {
                pubkey: o.to_owned(),
                role: "owner".into(),
                active: true,
            });
        }
        out.extend(self.members_of(&acl_doc));
        Ok(out)
    }

    // -----------------------------------------------------------------------
    // Sessions

    /// Declares interest in a scope: subscribe to it on every peer, now and on
    /// future connections.
    pub fn subscribe(&mut self, scope: &str) -> Result<Vec<Effect>> {
        path::check_scope(scope).map_err(|e| Error::Invalid(e.to_string()))?;
        if self.interests.insert(scope.to_owned()) {
            self.subscribe_all(scope);
        }
        Ok(self.finish())
    }

    fn subscribe_all(&mut self, scope: &str) {
        let conns: Vec<ConnId> = self.sessions.keys().copied().collect();
        for conn in conns {
            self.send_sub(conn, scope);
        }
    }

    fn send_sub(&mut self, conn: ConnId, scope: &str) {
        let Some(s) = self.sessions.get_mut(&conn) else {
            return;
        };
        if !s.authed || !s.opts.sync || s.subs_out.contains(scope) {
            return;
        }
        let Some(peer) = s.peer else { return };
        s.subs_out.insert(scope.to_owned());
        let cursor = self.store.cursor(&peer, scope);
        self.send(
            conn,
            &Msg::Sub {
                scope: scope.to_owned(),
                cursor,
            },
        );
    }

    pub fn connect(&mut self, conn: ConnId, opts: ConnOpts) -> Vec<Effect> {
        let my_nonce = crypto::random_bytes();
        self.sessions.insert(
            conn,
            Session {
                opts,
                my_nonce,
                peer: None,
                authed: false,
                subs_in: BTreeSet::new(),
                subs_out: BTreeSet::new(),
            },
        );
        self.send(
            conn,
            &Msg::Hello {
                version: PROTOCOL_VERSION,
                app: self.config.app_id.clone(),
                peer: self.device.public(),
                nonce: my_nonce,
            },
        );
        self.finish()
    }

    pub fn disconnect(&mut self, conn: ConnId) {
        self.sessions.remove(&conn);
    }

    pub fn receive(&mut self, conn: ConnId, bytes: &[u8], now_ms: u64) -> Vec<Effect> {
        if let Err(reason) = self.handle(conn, bytes, now_ms) {
            self.sessions.remove(&conn);
            self.fx.push(Effect::Close { conn, reason });
        }
        self.finish()
    }

    fn handle(
        &mut self,
        conn: ConnId,
        bytes: &[u8],
        now_ms: u64,
    ) -> std::result::Result<(), String> {
        let msg = Msg::from_bytes(bytes).map_err(|e| e.to_string())?;
        let Some(session) = self.sessions.get_mut(&conn) else {
            return Ok(());
        };
        match msg {
            Msg::Hello {
                version,
                app,
                peer,
                nonce,
            } => {
                if version != PROTOCOL_VERSION {
                    return Err(format!("unsupported protocol version {version}"));
                }
                if app != self.config.app_id {
                    return Err(format!("app id mismatch: {app:?}"));
                }
                if peer == self.device.public() {
                    return Err("connected to self".into());
                }
                if session.peer.is_some() {
                    return Err("duplicate hello".into());
                }
                session.peer = Some(peer);
                let msg = crypto::hello_proof_message(&app, &nonce, &self.device.public());
                let sig = self.device.sign(&msg);
                self.send(conn, &Msg::Proof { sig });
            }
            Msg::Proof { sig } => {
                let peer = session.peer.ok_or("proof before hello")?;
                if session.authed {
                    return Err("duplicate proof".into());
                }
                let msg =
                    crypto::hello_proof_message(&self.config.app_id, &session.my_nonce, &peer);
                if !crypto::verify(&peer, &msg, &sig) {
                    return Err("bad handshake signature".into());
                }
                session.authed = true;
                self.fx.push(Effect::PeerReady { conn, peer });
                for scope in self.interests.clone() {
                    self.send_sub(conn, &scope);
                }
            }
            _ if !session.authed => return Err("message before handshake".into()),
            Msg::Sub { scope, cursor } => {
                path::check_scope(&scope).map_err(|e| e.to_string())?;
                if !session.opts.sync {
                    return Ok(());
                }
                session.subs_in.insert(scope.clone());
                let reciprocate = session.opts.replicate || self.config.replicate_all;
                let mut after = cursor;
                loop {
                    let items = self.store.changes_after(&scope, after, CATCHUP_CHUNK);
                    let Some(&(last, _)) = items.last() else {
                        break;
                    };
                    after = last;
                    let done = items.len() < CATCHUP_CHUNK;
                    self.send(
                        conn,
                        &Msg::Changes {
                            scope: scope.clone(),
                            items,
                        },
                    );
                    if done {
                        break;
                    }
                }
                let cursor = self.store.last_seq().max(cursor);
                self.send(
                    conn,
                    &Msg::Synced {
                        scope: scope.clone(),
                        cursor,
                    },
                );
                if reciprocate || self.interests.contains(&scope) {
                    self.send_sub(conn, &scope);
                }
            }
            Msg::Unsub { scope } => {
                session.subs_in.remove(&scope);
            }
            Msg::Changes { scope, items } => {
                let peer = session.peer.expect("authed sessions have a peer");
                let track = session.subs_out.contains(&scope);
                let mut max_seq = 0;
                for (seq, change) in items {
                    max_seq = max_seq.max(seq);
                    // Invalid changes are dropped; they can never become valid.
                    let _ = self.ingest(change, &scope, Some(conn), now_ms);
                }
                if track {
                    self.store.set_cursor(&peer, &scope, max_seq);
                }
            }
            Msg::Synced { scope, cursor } => {
                let peer = session.peer.expect("authed sessions have a peer");
                if session.subs_out.contains(&scope) {
                    self.store.set_cursor(&peer, &scope, cursor);
                    self.fx.push(Effect::Synced { conn, scope });
                }
            }
        }
        Ok(())
    }

    /// Validates and applies a change from a peer.
    fn ingest(
        &mut self,
        change: Change,
        scope: &str,
        from: Option<ConnId>,
        now_ms: u64,
    ) -> std::result::Result<(), String> {
        let hash = change.hash();
        if self.store.has_change(&hash) {
            return Ok(());
        }
        let actual = change.check_shape()?;
        if actual != scope {
            return Err(format!("change for {actual} sent under {scope}"));
        }
        if !crypto::verify_change(&change) {
            return Err("bad signature".into());
        }
        self.clock
            .check(change.max_hlc(), now_ms)
            .map_err(|e| e.to_string())?;
        match acl::authorize(&self.store, &change) {
            Verdict::Allow => {
                self.apply(change, scope, from);
                Ok(())
            }
            Verdict::Pending => {
                let q = self.pending.entry(scope.to_owned()).or_default();
                if !q.iter().any(|c| c.hash() == hash) {
                    if q.len() >= self.config.max_pending {
                        q.remove(0);
                    }
                    q.push(change);
                }
                Ok(())
            }
            Verdict::Deny(m) => Err(m),
        }
    }

    fn apply(&mut self, change: Change, scope: &str, from: Option<ConnId>) {
        for op in &change.ops {
            if self.store.apply_op(op, change.author) {
                self.changed.insert(op.path.clone());
            }
        }
        self.clock.observe(change.max_hlc());
        let seq = self.store.append_change(scope, &change);

        let touches_acl = path::segments(scope)
            .ok()
            .and_then(|s| match path::namespace(&s) {
                Ok(Namespace::Spaces(id)) => Some(acl::acl_path(id)),
                _ => None,
            })
            .is_some_and(|acl_path| change.ops.iter().any(|o| o.path == acl_path));

        let targets: Vec<ConnId> = self
            .sessions
            .iter()
            .filter(|(c, s)| Some(**c) != from && s.authed && s.subs_in.contains(scope))
            .map(|(c, _)| *c)
            .collect();
        if !targets.is_empty() {
            let bytes = Msg::Changes {
                scope: scope.to_owned(),
                items: vec![(seq, change)],
            }
            .to_bytes();
            for conn in targets {
                self.fx.push(Effect::Send {
                    conn,
                    bytes: bytes.clone(),
                });
            }
        }

        if touches_acl {
            // Newly visible grants or keys: retry held changes and refresh
            // what readers can decrypt.
            self.keys
                .cache
                .borrow_mut()
                .retain(|(s, _), _| format!("spaces/{s}") != scope);
            self.retry_pending(scope);
            self.mark_scope_changed(scope);
        }
    }

    fn retry_pending(&mut self, scope: &str) {
        loop {
            let Some(queue) = self.pending.remove(scope) else {
                return;
            };
            let mut keep = Vec::new();
            let mut progressed = false;
            for c in queue {
                if self.store.has_change(&c.hash()) {
                    continue;
                }
                match acl::authorize(&self.store, &c) {
                    Verdict::Allow => {
                        progressed = true;
                        // `apply` may re-enter here; the queue was taken out
                        // first, so leftovers merge back below.
                        self.apply(c, scope, None);
                    }
                    Verdict::Pending => keep.push(c),
                    Verdict::Deny(_) => {}
                }
            }
            if !keep.is_empty() {
                self.pending
                    .entry(scope.to_owned())
                    .or_default()
                    .extend(keep);
            }
            if !progressed {
                return;
            }
        }
    }

    /// Marks every stored document in a scope as changed. Used when a space's
    /// keys change, since that can change what the reader is able to decrypt.
    fn mark_scope_changed(&mut self, scope: &str) {
        let Some((root, id)) = scope.split_once('/') else {
            return;
        };
        let mut paths = Vec::new();
        // Doc keys start with their segment count, so visit each depth.
        for n in (2..=path::MAX_SEGMENTS).step_by(2) {
            let mut prefix = vec![b'd', n as u8];
            keys::push_str(&mut prefix, root.as_bytes());
            keys::push_str(&mut prefix, id.as_bytes());
            self.store
                .kv
                .scan(&prefix, &keys::prefix_end(&prefix), false, &mut |k, _| {
                    paths.extend(keys::decode_doc_key(k));
                    true
                });
        }
        self.changed.extend(paths);
    }

    fn send(&mut self, conn: ConnId, msg: &Msg) {
        self.fx.push(Effect::Send {
            conn,
            bytes: msg.to_bytes(),
        });
    }

    /// Commits storage and returns the effects collected by this call.
    fn finish(&mut self) -> Vec<Effect> {
        self.store.kv.commit();
        if !self.changed.is_empty() {
            let paths = std::mem::take(&mut self.changed).into_iter().collect();
            self.fx.push(Effect::Changed { paths });
        }
        std::mem::take(&mut self.fx)
    }

    /// Number of changes held while waiting for a space ACL (diagnostics).
    pub fn pending_count(&self) -> usize {
        self.pending.values().map(Vec::len).sum()
    }

    pub fn has_change(&self, hash: &Hash) -> bool {
        self.store.has_change(hash)
    }

    /// Latest timestamp the clock has issued or observed.
    pub fn clock_last(&self) -> Hlc {
        self.clock.last()
    }
}
