//! Keys: X25519 identity, shared secret, HKDF direction keys, relay room id, pairing proof.

use crate::{CryptoError, b64, b64_array, hex};
use hkdf::Hkdf;
use hmac::{Hmac, Mac};
use rand_core::{OsRng, RngCore};
use sha2::Sha256;
use ssc_core::secrets::Secret;
use std::fmt;
use subtle::ConstantTimeEq;
use x25519_dalek::{PublicKey, StaticSecret};
use zeroize::{Zeroize, ZeroizeOnDrop, Zeroizing};

pub const INFO_PC_TO_PHONE: &[u8] = b"ssc/v1/pc->phone";
pub const INFO_PHONE_TO_PC: &[u8] = b"ssc/v1/phone->pc";
pub const INFO_ROOM: &[u8] = b"ssc/v1/room";

/// A long-lived X25519 keypair (the PC's identity, or a phone's in tests).
/// The private half is zeroized on drop (`x25519-dalek` `zeroize` feature).
pub struct Identity {
    secret: StaticSecret,
    public: PublicKey,
}

impl Identity {
    pub fn generate() -> Self {
        Self::from_secret(StaticSecret::random_from_rng(OsRng))
    }

    pub fn from_bytes(mut bytes: [u8; 32]) -> Self {
        let id = Self::from_secret(StaticSecret::from(bytes));
        bytes.zeroize();
        id
    }

    fn from_secret(secret: StaticSecret) -> Self {
        let public = PublicKey::from(&secret);
        Self { secret, public }
    }

    /// For the keychain (`SecretKey::PcIdentity`): base64 of the private key.
    pub fn to_secret(&self) -> Secret {
        let bytes = Zeroizing::new(self.secret.to_bytes());
        Secret::new(b64(bytes.as_slice()))
    }

    pub fn from_secret_b64(s: &Secret) -> Result<Self, CryptoError> {
        Ok(Self::from_bytes(b64_array::<32>(s.expose())?))
    }

    pub fn public_bytes(&self) -> [u8; 32] {
        self.public.to_bytes()
    }

    pub fn public_b64(&self) -> String {
        b64(self.public.as_bytes())
    }

    /// X25519(own_priv, peer_pub). Rejects an all-zero result (low-order peer key).
    pub fn shared_secret(&self, peer_pub: &[u8; 32]) -> Result<Zeroizing<[u8; 32]>, CryptoError> {
        let shared = self.secret.diffie_hellman(&PublicKey::from(*peer_pub));
        let out = Zeroizing::new(shared.to_bytes());
        // Constant-time all-zero check (same condition as `!shared.was_contributory()`).
        if bool::from(out.ct_eq(&[0u8; 32])) {
            return Err(CryptoError::LowOrderPoint);
        }
        Ok(out)
    }
}

impl fmt::Debug for Identity {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Identity(pub={}, priv=***)", self.public_b64())
    }
}

/// The two direction keys, 32 bytes each. Zeroized on drop; never printed.
#[derive(Clone, Zeroize, ZeroizeOnDrop)]
pub struct SessionKeys {
    pub pc_to_phone: [u8; 32],
    pub phone_to_pc: [u8; 32],
}

/// Constant-time comparison.
impl PartialEq for SessionKeys {
    fn eq(&self, other: &Self) -> bool {
        (self.pc_to_phone.ct_eq(&other.pc_to_phone) & self.phone_to_pc.ct_eq(&other.phone_to_pc)).into()
    }
}
impl Eq for SessionKeys {}

impl fmt::Debug for SessionKeys {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("SessionKeys(***)")
    }
}

impl SessionKeys {
    /// `HKDF-SHA256(ikm = shared, salt = token, info = "ssc/v1/pc->phone" | "ssc/v1/phone->pc")`.
    pub fn derive(shared: &[u8; 32], token: &[u8; 32]) -> Self {
        let hk = Hkdf::<Sha256>::new(Some(token), shared);
        let mut keys = Self { pc_to_phone: [0; 32], phone_to_pc: [0; 32] };
        hk.expand(INFO_PC_TO_PHONE, &mut keys.pc_to_phone).expect("32 <= 255*32");
        hk.expand(INFO_PHONE_TO_PC, &mut keys.phone_to_pc).expect("32 <= 255*32");
        keys
    }
}

/// Relay room id: `HKDF-SHA256(ikm = token, salt = empty, info = "ssc/v1/room", L = 16)`, hex.
pub fn room_id(token: &[u8; 32]) -> String {
    let mut out = [0u8; 16];
    Hkdf::<Sha256>::new(Some(&[]), token).expand(INFO_ROOM, &mut out).expect("16 <= 255*32");
    hex(&out)
}

/// `proof = HMAC-SHA256(key = token, msg = raw phone_pub)`.
pub fn pairing_proof(token: &[u8; 32], phone_pub: &[u8; 32]) -> [u8; 32] {
    let mut mac = <Hmac<Sha256> as Mac>::new_from_slice(token).expect("HMAC takes any key length");
    mac.update(phone_pub);
    mac.finalize().into_bytes().into()
}

/// Constant-time proof check (`hmac`'s `verify_slice` compares with `subtle`).
pub fn verify_pairing_proof(token: &[u8; 32], phone_pub: &[u8; 32], proof: &[u8]) -> bool {
    let mut mac = <Hmac<Sha256> as Mac>::new_from_slice(token).expect("HMAC takes any key length");
    mac.update(phone_pub);
    mac.verify_slice(proof).is_ok()
}

/// 32 random bytes from the OS RNG (tokens).
pub fn random_32() -> Zeroizing<[u8; 32]> {
    let mut b = Zeroizing::new([0u8; 32]);
    OsRng.fill_bytes(b.as_mut_slice());
    b
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_low_order_points() {
        let me = Identity::generate();
        // The all-zero point and the order-2/order-4 points all give an all-zero shared secret.
        let zero = [0u8; 32];
        let mut one = [0u8; 32];
        one[0] = 1;
        for p in [zero, one] {
            assert_eq!(me.shared_secret(&p).unwrap_err(), CryptoError::LowOrderPoint);
        }
    }

    #[test]
    fn identity_secret_roundtrip_and_redaction() {
        let a = Identity::generate();
        let b = Identity::from_secret_b64(&a.to_secret()).unwrap();
        assert_eq!(a.public_bytes(), b.public_bytes());
        assert!(!format!("{a:?}").contains(a.to_secret().expose()));
        let keys = SessionKeys { pc_to_phone: [7; 32], phone_to_pc: [8; 32] };
        assert_eq!(format!("{keys:?}"), "SessionKeys(***)");
    }

    #[test]
    fn proof_verification_is_exact() {
        let token = [9u8; 32];
        let pubk = [5u8; 32];
        let p = pairing_proof(&token, &pubk);
        assert!(verify_pairing_proof(&token, &pubk, &p));
        let mut bad = p;
        bad[31] ^= 1;
        assert!(!verify_pairing_proof(&token, &pubk, &bad));
        assert!(!verify_pairing_proof(&token, &pubk, &p[..31]));
        assert!(!verify_pairing_proof(&[8u8; 32], &pubk, &p));
    }
}
