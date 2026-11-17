//! `ssc-crypto`: everything in Build Sheet §4.3 on the PC side. The algorithm is fixed by
//! `contracts/crypto/README.md`; `tests/vectors.rs` reproduces `contracts/crypto/vectors.json`
//! byte for byte.
//!
//! - [`keys`]: X25519 identity, shared secret (all-zero rejected), HKDF direction keys, room id,
//!   pairing proof.
//! - [`frame`]: seal / open of wire frames, the per-direction replay window, [`frame::SecureChannel`].
//! - [`pairing`]: the one-time-token pairing state machine and the stored [`pairing::PairedDevice`].
//! - [`qr`]: QR payload and SVG.
//!
//! Secrets (`StaticSecret`, shared secrets, direction keys, tokens) are zeroized on drop and never
//! appear in `Debug` output.

pub mod frame;
pub mod keys;
pub mod pairing;
pub mod qr;

pub use frame::{Counters, ReplayWindow, Role, SecureChannel};
pub use keys::{Identity, SessionKeys};
pub use pairing::{PairError, PairedDevice, PairingOffer};

/// Why a frame or key operation failed. Receivers drop the frame and count it; the variant is
/// for tests and diagnostics only (never sent to the peer).
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum CryptoError {
    #[error("frame too short")]
    TooShort,
    #[error("unknown frame version {0:#04x}")]
    BadVersion(u8),
    #[error("authentication failed")]
    Aead,
    #[error("malformed plaintext: {0}")]
    Malformed(String),
    #[error("envelope seq/session_id differ from the AAD")]
    AadMismatch,
    #[error("replayed or out-of-order seq {0}")]
    Replay(u64),
    #[error("all-zero shared secret (low-order public key)")]
    LowOrderPoint,
    #[error("bad key encoding")]
    BadKey,
    #[error("sequence numbers exhausted")]
    SeqExhausted,
}

/// Standard base64 with padding (the only encoding the contract uses).
pub fn b64(bytes: &[u8]) -> String {
    use base64::Engine;
    base64::engine::general_purpose::STANDARD.encode(bytes)
}

/// Strict standard-base64 decode into exactly `N` bytes.
pub fn b64_array<const N: usize>(s: &str) -> Result<[u8; N], CryptoError> {
    use base64::Engine;
    let v =
        zeroize::Zeroizing::new(base64::engine::general_purpose::STANDARD.decode(s).map_err(|_| CryptoError::BadKey)?);
    v.as_slice().try_into().map_err(|_| CryptoError::BadKey)
}

pub(crate) fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}
