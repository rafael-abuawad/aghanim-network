//! Cryptography for aghanim-network.
//!
//! - **Identity:** one Ed25519 key. Its X25519 form (the same birational map
//!   libsodium and `age` use) receives wrapped space keys, so a member is
//!   addressed by a single 32-byte public key.
//! - **Passphrase login:** Argon2id over `alias + passphrase` derives the
//!   identity seed, like Gun SEA's `user.auth`.
//! - **Changes:** signed once per batch over the BLAKE3 change hash.
//! - **Fields:** XChaCha20-Poly1305 with the document path and field name as
//!   associated data, so ciphertext can't be moved to another field.
//! - **Space keys:** wrapped per member with an ephemeral X25519 sealed box.

use std::fmt;

use aghanim_core::{Change, Op, PubKey, Sealed, Sig, Value};
use chacha20poly1305::aead::{Aead, KeyInit, Payload};
use chacha20poly1305::{XChaCha20Poly1305, XNonce};
use ed25519_dalek::{Signer, SigningKey, VerifyingKey};

pub type SymKey = [u8; 32];

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CryptoError(pub String);

impl fmt::Display for CryptoError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "crypto error: {}", self.0)
    }
}

impl std::error::Error for CryptoError {}

pub fn random_bytes<const N: usize>() -> [u8; N] {
    let mut b = [0u8; N];
    getrandom::getrandom(&mut b).expect("system random source unavailable");
    b
}

#[derive(Clone)]
pub struct Identity {
    sk: SigningKey,
}

impl fmt::Debug for Identity {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Identity({})", self.public_hex())
    }
}

impl Identity {
    pub fn random() -> Self {
        Self::from_seed(random_bytes())
    }

    pub fn from_seed(seed: [u8; 32]) -> Self {
        Identity {
            sk: SigningKey::from_bytes(&seed),
        }
    }

    pub fn from_seed_slice(seed: &[u8]) -> Result<Self, CryptoError> {
        let seed: [u8; 32] = seed
            .try_into()
            .map_err(|_| CryptoError("identity seed must be 32 bytes".into()))?;
        Ok(Self::from_seed(seed))
    }

    /// The secret seed. Whoever holds it can act as this identity.
    pub fn seed(&self) -> [u8; 32] {
        self.sk.to_bytes()
    }

    pub fn public(&self) -> PubKey {
        self.sk.verifying_key().to_bytes()
    }

    pub fn public_hex(&self) -> String {
        hex::encode(self.public())
    }

    pub fn sign(&self, msg: &[u8]) -> Sig {
        self.sk.sign(msg).to_bytes()
    }

    fn x25519_secret(&self) -> x25519_dalek::StaticSecret {
        x25519_dalek::StaticSecret::from(self.sk.to_scalar_bytes())
    }
}

pub fn verify(pk: &PubKey, msg: &[u8], sig: &Sig) -> bool {
    let Ok(vk) = VerifyingKey::from_bytes(pk) else {
        return false;
    };
    vk.verify_strict(msg, &ed25519_dalek::Signature::from_bytes(sig))
        .is_ok()
}

pub fn parse_pubkey_hex(s: &str) -> Result<PubKey, CryptoError> {
    let mut pk = [0u8; 32];
    hex::decode_to_slice(s, &mut pk).map_err(|_| CryptoError(format!("bad public key {s:?}")))?;
    VerifyingKey::from_bytes(&pk).map_err(|_| CryptoError("not a valid ed25519 key".into()))?;
    Ok(pk)
}

// ---------------------------------------------------------------------------
// Passphrase login

/// Argon2id cost parameters.
#[derive(Debug, Clone, Copy)]
pub struct KdfParams {
    pub m_kib: u32,
    pub t: u32,
    pub p: u32,
}

impl KdfParams {
    /// OWASP's recommended Argon2id setting (19 MiB, 2 passes).
    pub const DEFAULT: KdfParams = KdfParams {
        m_kib: 19 * 1024,
        t: 2,
        p: 1,
    };
    /// Cheap parameters for tests only.
    pub const INSECURE_FAST: KdfParams = KdfParams {
        m_kib: 64,
        t: 1,
        p: 1,
    };
}

/// Derives an identity from an alias and passphrase. The same inputs always
/// give the same identity, on any device.
pub fn identity_from_passphrase(
    alias: &str,
    passphrase: &str,
    params: KdfParams,
) -> Result<Identity, CryptoError> {
    if alias.is_empty() || passphrase.is_empty() {
        return Err(CryptoError("alias and passphrase must not be empty".into()));
    }
    let mut salt = blake3::Hasher::new_derive_key("aghanim-network login salt v1");
    salt.update(alias.as_bytes());
    let salt = salt.finalize();
    let argon = argon2::Argon2::new(
        argon2::Algorithm::Argon2id,
        argon2::Version::V0x13,
        argon2::Params::new(params.m_kib, params.t, params.p, Some(32))
            .map_err(|e| CryptoError(e.to_string()))?,
    );
    let mut seed = [0u8; 32];
    argon
        .hash_password_into(passphrase.as_bytes(), &salt.as_bytes()[..16], &mut seed)
        .map_err(|e| CryptoError(e.to_string()))?;
    Ok(Identity::from_seed(seed))
}

// ---------------------------------------------------------------------------
// Signed changes and peer handshake

pub fn sign_change(id: &Identity, ops: Vec<Op>) -> Change {
    let author = id.public();
    let hash = Change::hash_of(&author, &ops);
    Change {
        author,
        sig: id.sign(&hash),
        ops,
    }
}

pub fn verify_change(c: &Change) -> bool {
    verify(&c.author, &c.hash(), &c.sig)
}

/// What a peer signs to prove it holds its key during the sync handshake:
/// the app id, the *other* side's fresh nonce, and its own public key.
pub fn hello_proof_message(app_id: &str, their_nonce: &[u8; 32], my_pk: &PubKey) -> Vec<u8> {
    let mut m = b"aghanim-network hello v1\0".to_vec();
    m.extend_from_slice(app_id.as_bytes());
    m.push(0);
    m.extend_from_slice(their_nonce);
    m.extend_from_slice(my_pk);
    m
}

// ---------------------------------------------------------------------------
// Spaces

/// A space id is derived from the owner's key and a random salt, so anyone can
/// check who owns a space without trusting whoever told them.
pub fn space_id(owner: &PubKey, salt: &[u8]) -> String {
    let mut h = blake3::Hasher::new_derive_key("aghanim-network space id v1");
    h.update(owner);
    h.update(salt);
    hex::encode(&h.finalize().as_bytes()[..16])
}

fn field_aad(path: &str, field: &str, epoch: u32) -> Vec<u8> {
    let mut a = b"aghanim-network field v1\0".to_vec();
    a.extend_from_slice(path.as_bytes());
    a.push(0);
    a.extend_from_slice(field.as_bytes());
    a.push(0);
    a.extend_from_slice(&epoch.to_be_bytes());
    a
}

pub fn encrypt_value(key: &SymKey, epoch: u32, path: &str, field: &str, v: &Value) -> Value {
    let nonce: [u8; 24] = random_bytes();
    let cipher = XChaCha20Poly1305::new(key.into());
    let ct = cipher
        .encrypt(
            XNonce::from_slice(&nonce),
            Payload {
                msg: &v.to_bytes(),
                aad: &field_aad(path, field, epoch),
            },
        )
        .expect("encryption cannot fail for in-memory buffers");
    Value::Encrypted(Sealed { epoch, nonce, ct })
}

pub fn decrypt_value(key: &SymKey, path: &str, field: &str, s: &Sealed) -> Option<Value> {
    let cipher = XChaCha20Poly1305::new(key.into());
    let pt = cipher
        .decrypt(
            XNonce::from_slice(&s.nonce),
            Payload {
                msg: &s.ct,
                aad: &field_aad(path, field, s.epoch),
            },
        )
        .ok()?;
    Value::from_bytes(&pt).ok()
}

/// The X25519 public key that corresponds to an Ed25519 identity key.
pub fn x25519_public_of(pk: &PubKey) -> Option<[u8; 32]> {
    Some(
        VerifyingKey::from_bytes(pk)
            .ok()?
            .to_montgomery()
            .to_bytes(),
    )
}

fn wrap_kek(shared: &[u8; 32], eph: &[u8; 32], recipient: &[u8; 32]) -> SymKey {
    let mut h = blake3::Hasher::new_derive_key("aghanim-network key wrap v1");
    h.update(shared);
    h.update(eph);
    h.update(recipient);
    *h.finalize().as_bytes()
}

/// Seals a symmetric key to a recipient: `eph_pub ‖ nonce ‖ ciphertext`.
/// `context` (space id and epoch) is bound in as associated data.
pub fn wrap_key(recipient: &PubKey, key: &SymKey, context: &[u8]) -> Result<Vec<u8>, CryptoError> {
    let rpub =
        x25519_public_of(recipient).ok_or_else(|| CryptoError("bad recipient key".into()))?;
    let eph = x25519_dalek::StaticSecret::from(random_bytes::<32>());
    let eph_pub = x25519_dalek::PublicKey::from(&eph).to_bytes();
    let shared = eph.diffie_hellman(&x25519_dalek::PublicKey::from(rpub));
    if !shared.was_contributory() {
        return Err(CryptoError("low-order recipient key".into()));
    }
    let kek = wrap_kek(shared.as_bytes(), &eph_pub, &rpub);
    let nonce: [u8; 24] = random_bytes();
    let ct = XChaCha20Poly1305::new((&kek).into())
        .encrypt(
            XNonce::from_slice(&nonce),
            Payload {
                msg: key,
                aad: context,
            },
        )
        .expect("encryption cannot fail for in-memory buffers");
    let mut out = Vec::with_capacity(32 + 24 + ct.len());
    out.extend_from_slice(&eph_pub);
    out.extend_from_slice(&nonce);
    out.extend_from_slice(&ct);
    Ok(out)
}

pub fn unwrap_key(id: &Identity, wrapped: &[u8], context: &[u8]) -> Option<SymKey> {
    if wrapped.len() < 32 + 24 + 16 {
        return None;
    }
    let eph_pub: [u8; 32] = wrapped[..32].try_into().ok()?;
    let nonce = &wrapped[32..56];
    let secret = id.x25519_secret();
    let mypub = x25519_dalek::PublicKey::from(&secret).to_bytes();
    let shared = secret.diffie_hellman(&x25519_dalek::PublicKey::from(eph_pub));
    if !shared.was_contributory() {
        return None;
    }
    let kek = wrap_kek(shared.as_bytes(), &eph_pub, &mypub);
    let pt = XChaCha20Poly1305::new((&kek).into())
        .decrypt(
            XNonce::from_slice(nonce),
            Payload {
                msg: &wrapped[56..],
                aad: context,
            },
        )
        .ok()?;
    pt.try_into().ok()
}

/// Associated data that binds a wrapped key to its space and epoch.
pub fn wrap_context(space_id: &str, epoch: u32) -> Vec<u8> {
    let mut c = b"aghanim-network space key v1\0".to_vec();
    c.extend_from_slice(space_id.as_bytes());
    c.push(0);
    c.extend_from_slice(&epoch.to_be_bytes());
    c
}

#[cfg(test)]
mod tests {
    use super::*;
    use aghanim_core::{Hlc, OpKind};

    fn unhex<const N: usize>(s: &str) -> [u8; N] {
        hex::decode(s).unwrap().try_into().unwrap()
    }

    #[test]
    fn rfc8032_test_vector_1() {
        let id = Identity::from_seed(unhex(
            "9d61b19deffd5a60ba844af492ec2cc44449c5697b326919703bac031cae7f60",
        ));
        assert_eq!(
            id.public_hex(),
            "d75a980182b10ab7d54bfed3c964073a0ee172f3daa62325af021a68f707511a"
        );
        let sig = id.sign(b"");
        assert_eq!(
            hex::encode(sig),
            "e5564300c360ac729086e2cc806e828a84877f1eb8e5d974d873e065224901555fb8821590a33bacc61e39701cf9b46bd25bf5f0595bbe24655141438e7a100b"
        );
        assert!(verify(&id.public(), b"", &sig));
        assert!(!verify(&id.public(), b"x", &sig));
    }

    #[test]
    fn x25519_form_matches_secret() {
        let id = Identity::random();
        let from_secret = x25519_dalek::PublicKey::from(&id.x25519_secret()).to_bytes();
        assert_eq!(x25519_public_of(&id.public()).unwrap(), from_secret);
    }

    #[test]
    fn passphrase_login_is_deterministic() {
        let p = KdfParams::INSECURE_FAST;
        let a = identity_from_passphrase("alice", "correct horse", p).unwrap();
        let b = identity_from_passphrase("alice", "correct horse", p).unwrap();
        let c = identity_from_passphrase("alice", "wrong horse", p).unwrap();
        let d = identity_from_passphrase("bob", "correct horse", p).unwrap();
        assert_eq!(a.public(), b.public());
        assert_ne!(a.public(), c.public());
        assert_ne!(a.public(), d.public());
        // Pinned so an accidental change to the derivation is caught.
        assert_eq!(a.public_hex(), PINNED_ALICE);
    }

    const PINNED_ALICE: &str = "8b08f426937645d20e2d2a53e642bd0ef697f09955ab4feddf67610d7e4a46f6";

    fn ops() -> Vec<Op> {
        vec![Op {
            path: "public/p/c/d".into(),
            hlc: Hlc::new(1, 0),
            kind: OpKind::Set {
                field: "a".into(),
                value: Value::Int(1),
            },
        }]
    }

    #[test]
    fn signed_changes_detect_tampering_and_forgery() {
        let alice = Identity::random();
        let c = sign_change(&alice, ops());
        assert!(verify_change(&c));

        let mut tampered = c.clone();
        tampered.ops[0].kind = OpKind::Set {
            field: "a".into(),
            value: Value::Int(2),
        };
        assert!(!verify_change(&tampered));

        let mut reauthored = c.clone();
        reauthored.author = Identity::random().public();
        assert!(!verify_change(&reauthored));

        let mut bad_sig = c;
        bad_sig.sig[0] ^= 1;
        assert!(!verify_change(&bad_sig));
    }

    #[test]
    fn field_encryption_binds_path_and_field() {
        let key: SymKey = random_bytes();
        let v = Value::Str("secret".into());
        let Value::Encrypted(s) = encrypt_value(&key, 3, "spaces/s/t/1", "title", &v) else {
            panic!("not encrypted");
        };
        assert_eq!(s.epoch, 3);
        assert_eq!(decrypt_value(&key, "spaces/s/t/1", "title", &s), Some(v));
        assert_eq!(decrypt_value(&key, "spaces/s/t/2", "title", &s), None);
        assert_eq!(decrypt_value(&key, "spaces/s/t/1", "other", &s), None);
        assert_eq!(
            decrypt_value(&random_bytes(), "spaces/s/t/1", "title", &s),
            None
        );
        let mut moved = s.clone();
        moved.epoch = 4;
        assert_eq!(decrypt_value(&key, "spaces/s/t/1", "title", &moved), None);
    }

    #[test]
    fn key_wrapping() {
        let bob = Identity::random();
        let eve = Identity::random();
        let key: SymKey = random_bytes();
        let ctx = wrap_context("space1", 1);
        let w = wrap_key(&bob.public(), &key, &ctx).unwrap();
        assert_eq!(unwrap_key(&bob, &w, &ctx), Some(key));
        assert_eq!(unwrap_key(&eve, &w, &ctx), None);
        assert_eq!(unwrap_key(&bob, &w, &wrap_context("space1", 2)), None);
    }

    #[test]
    fn space_ids_are_bound_to_owner() {
        let a = Identity::random().public();
        let b = Identity::random().public();
        assert_ne!(space_id(&a, b"salt"), space_id(&b, b"salt"));
        assert_eq!(space_id(&a, b"salt").len(), 32);
    }
}
