//! Pairing state machine (`contracts/crypto/README.md` §Pairing).
//!
//! One [`PairingOffer`] per QR code. Its token is single-use: it is burned (zeroized) after the
//! first successful `pair.hello`, after the 5th failed attempt, or on the first attempt after
//! `exp`. A burned offer rejects everything.

use crate::frame::Counters;
use crate::keys::{Identity, SessionKeys, random_32, room_id, verify_pairing_proof};
use crate::{CryptoError, b64, b64_array, hex};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use ssc_core::protocol::{PairHello, QrPayload};
use ssc_core::secrets::Secret;
use std::fmt;
use zeroize::Zeroizing;

/// QR lifetime (README: `exp = now + 120`).
pub const PAIRING_TTL_S: u64 = 120;
/// Failed attempts that burn the token.
pub const MAX_FAILURES: u8 = 5;

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum PairError {
    #[error("pairing token already used or burned")]
    Burned,
    #[error("pairing token expired")]
    Expired,
    #[error("pairing proof rejected")]
    BadProof,
    #[error("bad phone key: {0}")]
    BadKey(CryptoError),
}

pub struct PairingOffer {
    /// `None` once burned.
    token: Option<Zeroizing<[u8; 32]>>,
    room_id: String,
    exp: u64,
    failures: u8,
}

impl fmt::Debug for PairingOffer {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("PairingOffer")
            .field("room_id", &self.room_id)
            .field("exp", &self.exp)
            .field("failures", &self.failures)
            .field("burned", &self.is_burned())
            .finish_non_exhaustive()
    }
}

impl PairingOffer {
    /// Fresh random token, valid until `now_s + 120`.
    pub fn new(now_s: u64) -> Self {
        Self::with_token(*random_32(), now_s + PAIRING_TTL_S)
    }

    /// Fixed token (test vectors). Production code uses [`Self::new`].
    pub fn with_token(token: [u8; 32], exp: u64) -> Self {
        let token = Zeroizing::new(token);
        Self { room_id: room_id(&token), token: Some(token), exp, failures: 0 }
    }

    pub fn exp(&self) -> u64 {
        self.exp
    }

    /// Relay room for this pairing (16 bytes, hex).
    pub fn room_id(&self) -> &str {
        &self.room_id
    }

    pub fn is_burned(&self) -> bool {
        self.token.is_none()
    }

    pub fn failures(&self) -> u8 {
        self.failures
    }

    /// QR payload for this offer. `urls`: LAN first, relay (`wss://<relay>/r/<room_id>`) last.
    /// `None` if the offer is already burned.
    pub fn qr_payload(&self, pc: &Identity, urls: Vec<String>) -> Option<QrPayload> {
        let token = self.token.as_ref()?;
        Some(QrPayload { v: 1, urls, pc_pub: pc.public_b64(), token: b64(token.as_slice()), exp: self.exp })
    }

    fn burn(&mut self) {
        self.token = None; // Zeroizing wipes it on drop
    }

    fn fail(&mut self, e: PairError) -> PairError {
        self.failures += 1;
        if self.failures >= MAX_FAILURES {
            self.burn();
        }
        e
    }

    /// Check a plaintext `pair.hello`: `exp`, then the proof in constant time. On success the
    /// token is burned and the derived device returned.
    pub fn accept(
        &mut self,
        pc: &Identity,
        hello: &PairHello,
        now_s: u64,
        now_ms: u64,
    ) -> Result<PairedDevice, PairError> {
        let Some(token) = self.token.as_ref() else { return Err(PairError::Burned) };
        if now_s > self.exp {
            self.burn();
            return Err(PairError::Expired);
        }
        let token: Zeroizing<[u8; 32]> = token.clone();
        let Ok(phone_pub) = b64_array::<32>(&hello.phone_pub) else {
            return Err(self.fail(PairError::BadKey(CryptoError::BadKey)));
        };
        let proof = match b64_array::<32>(&hello.proof) {
            Ok(p) => p,
            Err(_) => return Err(self.fail(PairError::BadProof)),
        };
        if !verify_pairing_proof(&token, &phone_pub, &proof) {
            return Err(self.fail(PairError::BadProof));
        }
        let shared = match pc.shared_secret(&phone_pub) {
            Ok(s) => s,
            Err(e) => return Err(self.fail(PairError::BadKey(e))),
        };
        let keys = SessionKeys::derive(&shared, &token);
        let room_id = self.room_id.clone();
        self.burn();
        Ok(PairedDevice {
            device_id: device_id(&phone_pub),
            device_name: hello.device_name.clone(),
            phone_pub: b64(&phone_pub),
            keys,
            room_id,
            created_at: now_ms,
            counters: Counters::default(),
        })
    }
}

/// First 8 bytes of SHA-256(phone_pub), hex. Stable per phone key.
pub fn device_id(phone_pub: &[u8; 32]) -> String {
    hex(&Sha256::digest(phone_pub)[..8])
}

/// A paired phone, as stored in the keychain (`SecretKey::PairedDevice(device_id)`).
#[derive(Clone, PartialEq, Eq)]
pub struct PairedDevice {
    pub device_id: String,
    pub device_name: String,
    /// base64
    pub phone_pub: String,
    pub keys: SessionKeys,
    /// Relay room (hex), fixed at pairing.
    pub room_id: String,
    /// Unix ms.
    pub created_at: u64,
    /// Seq counters (persist on shutdown; see `frame` docs).
    pub counters: Counters,
}

impl fmt::Debug for PairedDevice {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("PairedDevice")
            .field("device_id", &self.device_id)
            .field("device_name", &self.device_name)
            .field("room_id", &self.room_id)
            .field("created_at", &self.created_at)
            .field("keys", &self.keys)
            .finish_non_exhaustive()
    }
}

#[derive(Serialize, Deserialize)]
struct StoredDevice {
    device_id: String,
    device_name: String,
    phone_pub: String,
    key_pc_to_phone: String,
    key_phone_to_pc: String,
    room_id: String,
    created_at: u64,
    #[serde(default)]
    counters: Counters,
}

impl Drop for StoredDevice {
    fn drop(&mut self) {
        use zeroize::Zeroize;
        self.key_pc_to_phone.zeroize();
        self.key_phone_to_pc.zeroize();
    }
}

impl PairedDevice {
    /// JSON for the keychain (`{device_id, phone_pub, keys, created_at, ...}`).
    pub fn to_secret(&self) -> Secret {
        let s = StoredDevice {
            device_id: self.device_id.clone(),
            device_name: self.device_name.clone(),
            phone_pub: self.phone_pub.clone(),
            key_pc_to_phone: b64(&self.keys.pc_to_phone),
            key_phone_to_pc: b64(&self.keys.phone_to_pc),
            room_id: self.room_id.clone(),
            created_at: self.created_at,
            counters: self.counters,
        };
        Secret::new(serde_json::to_string(&s).expect("plain struct serialises"))
    }

    pub fn from_secret(secret: &Secret) -> Result<Self, CryptoError> {
        let s: StoredDevice = serde_json::from_str(secret.expose()).map_err(|_| CryptoError::BadKey)?;
        Ok(Self {
            device_id: s.device_id.clone(),
            device_name: s.device_name.clone(),
            phone_pub: s.phone_pub.clone(),
            keys: SessionKeys {
                pc_to_phone: b64_array(&s.key_pc_to_phone)?,
                phone_to_pc: b64_array(&s.key_phone_to_pc)?,
            },
            room_id: s.room_id.clone(),
            created_at: s.created_at,
            counters: s.counters,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::keys::pairing_proof;

    fn hello(phone: &Identity, token: &[u8; 32]) -> PairHello {
        PairHello {
            device_name: "Pixel".into(),
            phone_pub: phone.public_b64(),
            proof: b64(&pairing_proof(token, &phone.public_bytes())),
        }
    }

    const TOKEN: [u8; 32] = [42; 32];

    #[test]
    fn success_burns_token_and_both_sides_agree() {
        let pc = Identity::generate();
        let phone = Identity::generate();
        let mut offer = PairingOffer::with_token(TOKEN, 1000);
        let dev = offer.accept(&pc, &hello(&phone, &TOKEN), 1000, 5).unwrap();
        assert!(offer.is_burned());
        // Phone side derives the same keys.
        let shared = phone.shared_secret(&pc.public_bytes()).unwrap();
        assert_eq!(SessionKeys::derive(&shared, &TOKEN), dev.keys);
        assert_eq!(dev.room_id, room_id(&TOKEN));
        // One use only: the same hello again fails.
        assert_eq!(offer.accept(&pc, &hello(&phone, &TOKEN), 1000, 5).unwrap_err(), PairError::Burned);
        // Keychain roundtrip.
        let back = PairedDevice::from_secret(&dev.to_secret()).unwrap();
        assert_eq!(back, dev);
        assert!(!format!("{dev:?}").contains(&b64(&dev.keys.pc_to_phone)));
    }

    #[test]
    fn expired_token_fails_and_burns() {
        let pc = Identity::generate();
        let phone = Identity::generate();
        let mut offer = PairingOffer::with_token(TOKEN, 1000);
        assert_eq!(offer.accept(&pc, &hello(&phone, &TOKEN), 1001, 0).unwrap_err(), PairError::Expired);
        assert!(offer.is_burned());
        assert_eq!(offer.accept(&pc, &hello(&phone, &TOKEN), 999, 0).unwrap_err(), PairError::Burned);
    }

    #[test]
    fn wrong_proof_fails_and_five_failures_burn() {
        let pc = Identity::generate();
        let phone = Identity::generate();
        let mut offer = PairingOffer::with_token(TOKEN, 1000);
        let wrong = hello(&phone, &[43; 32]);
        for i in 1..=4 {
            assert_eq!(offer.accept(&pc, &wrong, 10, 0).unwrap_err(), PairError::BadProof);
            assert_eq!(offer.failures(), i);
            assert!(!offer.is_burned());
        }
        // Proof for a different key than the one sent.
        let mut swapped = hello(&phone, &TOKEN);
        swapped.phone_pub = Identity::generate().public_b64();
        assert_eq!(offer.accept(&pc, &swapped, 10, 0).unwrap_err(), PairError::BadProof);
        assert!(offer.is_burned());
        // Even the right proof fails now.
        assert_eq!(offer.accept(&pc, &hello(&phone, &TOKEN), 10, 0).unwrap_err(), PairError::Burned);
    }

    #[test]
    fn low_order_phone_key_rejected_even_with_valid_proof() {
        let pc = Identity::generate();
        let mut offer = PairingOffer::with_token(TOKEN, 1000);
        let zero = [0u8; 32];
        let h = PairHello { device_name: "x".into(), phone_pub: b64(&zero), proof: b64(&pairing_proof(&TOKEN, &zero)) };
        assert_eq!(offer.accept(&pc, &h, 10, 0).unwrap_err(), PairError::BadKey(CryptoError::LowOrderPoint));
        assert!(!offer.is_burned());
    }

    #[test]
    fn qr_payload_validates_and_is_none_after_burn() {
        let pc = Identity::generate();
        let mut offer = PairingOffer::new(1_763_290_000);
        assert_eq!(offer.exp(), 1_763_290_120);
        let urls = vec!["ws://192.168.1.20:47823".into(), format!("wss://relay.example.com/r/{}", offer.room_id())];
        let qr = offer.qr_payload(&pc, urls.clone()).unwrap();
        let v = serde_json::to_value(&qr).unwrap();
        assert_eq!(QrPayload::from_value(&v).unwrap(), qr);
        offer.burn();
        assert!(offer.qr_payload(&pc, urls).is_none());
    }
}
