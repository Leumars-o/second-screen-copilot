//! Wire frames: `0x01 || nonce(24) || XChaCha20-Poly1305(key, nonce, envelope JSON, aad)`,
//! `aad = UTF-8(session_id or "") || seq as u64 BE`.
//!
//! ## Opening a frame (CR-1, lead decision; mirrored by `packages/protocol-ts`)
//!
//! The AAD holds `seq` and `session_id`, but the wire carries neither in the clear. The receiver
//! therefore recovers them in two steps:
//!
//! 1. *Peek*: XChaCha20 keystream-decrypt the body (block counter 1, i.e. seek to byte 64:
//!    exactly the AEAD's ciphertext keystream) without checking the tag, and read only `seq` and
//!    `session_id` from the JSON. This unauthenticated plaintext is used for nothing else and is
//!    zeroized.
//! 2. *Open*: rebuild the AAD from those two values and run the full AEAD decryption. Only the
//!    authenticated output is parsed and used; its `seq`/`session_id` must equal the AAD values.
//!
//! A forger without the key cannot pass step 2 for any AAD, so step 1 gives no advantage beyond
//! making the receiver parse up to one frame of attacker-chosen JSON (which it must do anyway).
//!
//! ## Sequence numbers (CR-3, lead decision)
//!
//! `seq` is scoped to the pairing, not the connection: it increases by one per frame and
//! direction across reconnects and restarts. Persisted state per pairing is [`Counters`]:
//! `next_send` is a *reservation* (every seq ever sent is below it) and `last_recv` the replay
//! window. On start the channel reserves a block of [`SEQ_RESERVE_BLOCK`] seqs above the
//! persisted value and asks to persist the new reservation ([`SecureChannel::take_persist`]);
//! it does so again each time it crosses into a new block. A crash before a reservation is
//! written can only make the peer drop some frames as replays: nonces are random, so no
//! (key, nonce) pair is reused.

use crate::CryptoError;
use crate::keys::SessionKeys;
use chacha20::XChaCha20;
use chacha20::cipher::{KeyIvInit, StreamCipher, StreamCipherSeek};
use chacha20poly1305::aead::{Aead, KeyInit, Payload};
use chacha20poly1305::{XChaCha20Poly1305, XNonce};
use rand_core::{OsRng, RngCore};
use serde::{Deserialize, Serialize};
use ssc_core::protocol::{Envelope, Message};
use uuid::Uuid;
use zeroize::Zeroizing;

pub const FRAME_VERSION: u8 = 0x01;
pub const NONCE_LEN: usize = 24;
pub const TAG_LEN: usize = 16;
pub const HEADER_LEN: usize = 1 + NONCE_LEN;
/// Largest seq the envelope schema allows (2^53 − 1, JS safe integer).
pub const MAX_SEQ: u64 = 9_007_199_254_740_991;

/// `UTF-8(session_id as lowercase hyphenated UUID, or "") || seq as u64 BE`.
pub fn aad(session_id: Option<&str>, seq: u64) -> Vec<u8> {
    let sid = session_id.unwrap_or("");
    let mut out = Vec::with_capacity(sid.len() + 8);
    out.extend_from_slice(sid.as_bytes());
    out.extend_from_slice(&seq.to_be_bytes());
    out
}

fn sid_string(session_id: Option<Uuid>) -> Option<String> {
    session_id.map(|s| s.hyphenated().to_string())
}

/// Seal `plaintext` with an explicit nonce (tests and vectors). Production code uses [`seal`].
pub fn seal_with_nonce(
    key: &[u8; 32],
    nonce: &[u8; NONCE_LEN],
    session_id: Option<&str>,
    seq: u64,
    plaintext: &[u8],
) -> Vec<u8> {
    let cipher = XChaCha20Poly1305::new(key.into());
    let aad = aad(session_id, seq);
    let ct = cipher
        .encrypt(XNonce::from_slice(nonce), Payload { msg: plaintext, aad: &aad })
        .expect("XChaCha20-Poly1305 encryption cannot fail for in-memory buffers");
    let mut wire = Vec::with_capacity(HEADER_LEN + ct.len());
    wire.push(FRAME_VERSION);
    wire.extend_from_slice(nonce);
    wire.extend_from_slice(&ct);
    wire
}

/// Seal an envelope with a fresh random nonce.
pub fn seal(key: &[u8; 32], env: &Envelope) -> Vec<u8> {
    let mut nonce = [0u8; NONCE_LEN];
    OsRng.fill_bytes(&mut nonce);
    let sid = sid_string(env.session_id);
    let plaintext = Zeroizing::new(env.to_bytes());
    seal_with_nonce(key, &nonce, sid.as_deref(), env.seq, &plaintext)
}

#[derive(Deserialize)]
struct Peek {
    seq: u64,
    session_id: Option<String>,
}

/// Open a frame: version check, AAD recovery (see module docs), AEAD, schema validation,
/// AAD/envelope consistency. Does **not** check replay; [`SecureChannel::open`] does.
pub fn open(key: &[u8; 32], wire: &[u8]) -> Result<Envelope, CryptoError> {
    if wire.len() < HEADER_LEN + TAG_LEN {
        return Err(CryptoError::TooShort);
    }
    if wire[0] != FRAME_VERSION {
        return Err(CryptoError::BadVersion(wire[0]));
    }
    let nonce: &[u8; NONCE_LEN] = wire[1..HEADER_LEN].try_into().expect("length checked");
    let body = &wire[HEADER_LEN..];

    // CR-1 step 1: unauthenticated peek at seq + session_id only.
    let (seq, sid) = {
        let mut peek = Zeroizing::new(body[..body.len() - TAG_LEN].to_vec());
        let mut stream = XChaCha20::new(key.into(), nonce.into());
        stream.seek(64u64); // block 0 is the Poly1305 key; the ciphertext starts at block 1
        stream.apply_keystream(&mut peek);
        let p: Peek = serde_json::from_slice(&peek).map_err(|_| CryptoError::Aead)?;
        (p.seq, p.session_id)
    };

    // CR-1 step 2: authenticated decryption with the recovered AAD; nothing before this is trusted.
    let cipher = XChaCha20Poly1305::new(key.into());
    let aad = aad(sid.as_deref(), seq);
    let plaintext = Zeroizing::new(
        cipher.decrypt(XNonce::from_slice(nonce), Payload { msg: body, aad: &aad }).map_err(|_| CryptoError::Aead)?,
    );
    let env = Envelope::from_slice(&plaintext).map_err(|e| CryptoError::Malformed(e.to_string()))?;
    if env.seq != seq || sid_string(env.session_id) != sid {
        return Err(CryptoError::AadMismatch);
    }
    Ok(env)
}

/// Per-direction replay protection: accept only `seq` strictly greater than the last accepted.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReplayWindow {
    pub last: Option<u64>,
}

impl ReplayWindow {
    pub fn would_accept(&self, seq: u64) -> bool {
        self.last.is_none_or(|l| seq > l)
    }
    /// Record `seq` if it is new. Returns false (and changes nothing) for a replay.
    pub fn accept(&mut self, seq: u64) -> bool {
        if !self.would_accept(seq) {
            return false;
        }
        self.last = Some(seq);
        true
    }
}

/// Which end of the link this channel is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Role {
    Pc,
    Phone,
}

/// How many seqs a sender reserves (and persists) at a time. CR-3.
pub const SEQ_RESERVE_BLOCK: u64 = 1000;

/// Per-pairing seq state to persist with the paired device. CR-3.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Counters {
    /// Reservation: the first seq this side may use after a restart. Every seq sent so far is
    /// below it. 0 = nothing sent yet (start at 1).
    pub next_send: u64,
    /// Last seq accepted from the peer (the replay window).
    pub last_recv: Option<u64>,
}

/// One end of an established link: own send key + seq, peer key + replay window.
pub struct SecureChannel {
    send_key: Zeroizing<[u8; 32]>,
    recv_key: Zeroizing<[u8; 32]>,
    next_seq: u64,
    /// Exclusive upper bound of the current seq reservation.
    reserved: u64,
    /// The reservation changed and must be persisted.
    persist: bool,
    window: ReplayWindow,
    dropped: u64,
}

impl SecureChannel {
    /// Resume from persisted `counters` (use `Counters::default()` right after pairing).
    /// Immediately reserves a new block: call [`Self::take_persist`] and persist [`Self::counters`].
    pub fn new(role: Role, keys: &SessionKeys, counters: Counters) -> Self {
        let (send, recv) = match role {
            Role::Pc => (keys.pc_to_phone, keys.phone_to_pc),
            Role::Phone => (keys.phone_to_pc, keys.pc_to_phone),
        };
        let next_seq = counters.next_send.max(1);
        Self {
            send_key: Zeroizing::new(send),
            recv_key: Zeroizing::new(recv),
            next_seq,
            reserved: next_seq.saturating_add(SEQ_RESERVE_BLOCK),
            persist: true,
            window: ReplayWindow { last: counters.last_recv },
            dropped: 0,
        }
    }

    /// Build, validate and seal the next envelope. Returns it with its wire bytes.
    pub fn seal(
        &mut self,
        ts: u64,
        session_id: Option<Uuid>,
        message: Message,
    ) -> Result<(Envelope, Vec<u8>), CryptoError> {
        if self.next_seq > MAX_SEQ {
            return Err(CryptoError::SeqExhausted);
        }
        let env = Envelope::new(self.next_seq, ts, session_id, message);
        // Never put a frame on the wire that the peer's schema check would reject.
        let env = Envelope::from_slice(&env.to_bytes()).map_err(|e| CryptoError::Malformed(e.to_string()))?;
        let wire = seal(&self.send_key, &env);
        self.next_seq += 1;
        if self.next_seq >= self.reserved {
            // CR-3: next block.
            self.reserved = self.next_seq.saturating_add(SEQ_RESERVE_BLOCK);
            self.persist = true;
        }
        Ok((env, wire))
    }

    /// Open a frame from the peer and enforce the replay window. Failures bump [`Self::dropped`].
    pub fn open(&mut self, wire: &[u8]) -> Result<Envelope, CryptoError> {
        let r = open(&self.recv_key, wire)
            .and_then(|env| if self.window.accept(env.seq) { Ok(env) } else { Err(CryptoError::Replay(env.seq)) });
        if r.is_err() {
            self.dropped += 1;
        }
        r
    }

    /// Frames dropped so far (bad version, AEAD, schema, AAD mismatch, replay).
    pub fn dropped(&self) -> u64 {
        self.dropped
    }

    /// The seq the next sealed frame gets.
    pub fn next_seq(&self) -> u64 {
        self.next_seq
    }

    /// State to persist: the current reservation and the replay window.
    pub fn counters(&self) -> Counters {
        Counters { next_send: self.reserved, last_recv: self.window.last }
    }

    /// True once per new reservation: persist [`Self::counters`] now.
    pub fn take_persist(&mut self) -> bool {
        std::mem::take(&mut self.persist)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn replay_window_is_strict() {
        let mut w = ReplayWindow::default();
        assert!(w.accept(5));
        assert!(!w.accept(5));
        assert!(!w.accept(4));
        assert!(w.accept(6));
        assert!(w.accept(100));
        assert_eq!(w.last, Some(100));
    }

    #[test]
    fn seq_reservation_survives_restart() {
        let keys = SessionKeys { pc_to_phone: [1; 32], phone_to_pc: [2; 32] };
        let mut ch = SecureChannel::new(Role::Pc, &keys, Counters::default());
        assert!(ch.take_persist(), "a fresh channel reserves a block");
        assert!(!ch.take_persist());
        assert_eq!(ch.counters().next_send, 1 + SEQ_RESERVE_BLOCK);
        let msg = || Message::Heartbeat(ssc_core::protocol::Heartbeat::default());
        for i in 1..SEQ_RESERVE_BLOCK {
            assert_eq!(ch.seal(0, None, msg()).unwrap().0.seq, i);
            assert!(!ch.take_persist());
        }
        // Seq 1000 is the last of the block: sealing it moves the reservation.
        assert_eq!(ch.seal(0, None, msg()).unwrap().0.seq, SEQ_RESERVE_BLOCK);
        assert!(ch.take_persist());
        let persisted = ch.counters();
        assert_eq!(persisted.next_send, 1 + 2 * SEQ_RESERVE_BLOCK);
        // Restart from the persisted reservation: never reuses a seq.
        let mut ch2 = SecureChannel::new(Role::Pc, &keys, persisted);
        assert_eq!(ch2.seal(0, None, msg()).unwrap().0.seq, 1 + 2 * SEQ_RESERVE_BLOCK);
    }
}
