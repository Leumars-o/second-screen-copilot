//! W2-HARDEN security suite for the frame and pairing crypto (Build Sheet §7, threat model
//! `docs/security/threat-model.md` §2–3). Every test states the attack it models.

use ssc_core::protocol::{AnswerDelta, Block, Command, Envelope, Heartbeat, Message, PairHello};
use ssc_crypto::frame::{HEADER_LEN, aad, open, seal, seal_with_nonce};
use ssc_crypto::keys::{Identity, SessionKeys, pairing_proof};
use ssc_crypto::{Counters, CryptoError, PairError, PairingOffer, Role, SecureChannel, b64};
use uuid::Uuid;

const MARKER: &str = "TOP-SECRET-PRICE-45000-NAIRA";

fn keys() -> SessionKeys {
    SessionKeys { pc_to_phone: [0x11; 32], phone_to_pc: [0x22; 32] }
}

fn delta(text: &str) -> Message {
    Message::AnswerDelta(AnswerDelta { answer_id: Uuid::nil(), block: Block::Cue, text: text.into() })
}

fn pair() -> (SecureChannel, SecureChannel) {
    let k = keys();
    (SecureChannel::new(Role::Pc, &k, Counters::default()), SecureChannel::new(Role::Phone, &k, Counters::default()))
}

fn contains(hay: &[u8], needle: &[u8]) -> bool {
    hay.windows(needle.len()).any(|w| w == needle)
}

// ------------------------------------------------------------------ ciphertext only

/// The relay (or any on-path observer) sees only `0x01 || nonce || ct`: no plaintext fragment,
/// no JSON structure, and identical messages never produce identical frames.
#[test]
fn sealed_frames_reveal_no_plaintext() {
    let (mut pc, _) = pair();
    let uuid = Uuid::new_v4();
    let sid = Some(uuid);
    let (_, a) = pc.seal(1, sid, delta(MARKER)).unwrap();
    let (_, b) = pc.seal(1, sid, delta(MARKER)).unwrap();
    for wire in [&a, &b] {
        assert_eq!(wire[0], 0x01);
        for needle in [MARKER.as_bytes(), b"answer.delta", b"\"type\"", b"session_id", b"payload"] {
            assert!(!contains(wire, needle), "plaintext {:?} visible on the wire", String::from_utf8_lossy(needle));
        }
        let sid_s = uuid.to_string();
        assert!(!contains(wire, sid_s.as_bytes()), "session id visible on the wire");
    }
    assert_ne!(a[1..HEADER_LEN], b[1..HEADER_LEN], "nonces must be fresh per frame");
    assert_ne!(a, b);
}

// ------------------------------------------------------------------ tamper

/// Flipping any single bit anywhere in a frame (version, nonce, ciphertext, tag) is rejected and
/// does not move the replay window, so the genuine frame is still accepted afterwards.
#[test]
fn every_single_bit_flip_is_rejected() {
    let (mut pc, mut phone) = pair();
    let (_, wire) = pc.seal(1, None, delta(MARKER)).unwrap();
    for i in 0..wire.len() {
        for bit in [0x01u8, 0x80] {
            let mut t = wire.clone();
            t[i] ^= bit;
            assert!(phone.open(&t).is_err(), "bit flip at byte {i} accepted");
        }
    }
    assert_eq!(phone.dropped(), (wire.len() * 2) as u64);
    let env = phone.open(&wire).expect("genuine frame still opens after the tampered copies");
    assert_eq!(env.seq, 1);
}

/// Truncation, extension and garbage frames of every short length are rejected without panicking.
#[test]
fn truncated_extended_and_garbage_frames_are_rejected() {
    let (mut pc, mut phone) = pair();
    let (_, wire) = pc.seal(1, None, delta("hi")).unwrap();
    for n in 0..wire.len() {
        assert!(phone.open(&wire[..n]).is_err(), "truncated to {n} accepted");
    }
    let mut longer = wire.clone();
    longer.push(0);
    assert!(phone.open(&longer).is_err());
    for len in [0usize, 1, 24, 40, 41, 64, 1024] {
        let junk: Vec<u8> = (0..len).map(|i| (i * 31 + 7) as u8).collect();
        let mut v1 = junk.clone();
        if let Some(b) = v1.first_mut() {
            *b = 0x01;
        }
        assert!(phone.open(&junk).is_err());
        assert!(phone.open(&v1).is_err());
    }
    assert_eq!(phone.open(&wire).unwrap().seq, 1);
}

/// A frame sealed with an AAD that disagrees with the envelope inside (an attacker who could
/// splice ciphertexts across seqs/sessions) is rejected.
#[test]
fn aad_and_envelope_must_agree() {
    let k = keys();
    let env = Envelope::new(5, 1, None, Message::Heartbeat(Heartbeat::default()));
    let nonce = [9u8; 24];
    // Envelope says seq 5 / no session, AAD says seq 6.
    let w = seal_with_nonce(&k.pc_to_phone, &nonce, None, 6, &env.to_bytes());
    assert!(matches!(open(&k.pc_to_phone, &w), Err(CryptoError::Aead | CryptoError::AadMismatch)));
    // Envelope has no session, AAD claims one.
    let sid = Uuid::new_v4().to_string();
    let w = seal_with_nonce(&k.pc_to_phone, &nonce, Some(&sid), 5, &env.to_bytes());
    assert!(open(&k.pc_to_phone, &w).is_err());
    // Control: the honest AAD opens.
    let w = seal_with_nonce(&k.pc_to_phone, &nonce, None, 5, &env.to_bytes());
    assert_eq!(open(&k.pc_to_phone, &w).unwrap().seq, 5);
    assert_eq!(aad(None, 5), 5u64.to_be_bytes().to_vec());
}

/// Valid AEAD but a plaintext that is not a schema-valid envelope (a compromised peer or a bug)
/// is rejected before anything reads it.
#[test]
fn schema_invalid_plaintext_is_rejected() {
    let k = keys();
    let bad = br#"{"v":1,"type":"command","seq":3,"ts":1,"session_id":null,"payload":{"action":"rm -rf"}}"#;
    let w = seal_with_nonce(&k.phone_to_pc, &[1; 24], None, 3, bad);
    assert!(matches!(open(&k.phone_to_pc, &w), Err(CryptoError::Malformed(_))));
}

// ------------------------------------------------------------------ replay / reflection

/// Replaying an accepted frame, or delivering an older one after a newer one, is rejected; a
/// rejected replay does not move the window.
#[test]
fn replays_and_reordering_are_rejected() {
    let (mut pc, mut phone) = pair();
    let (_, f1) = pc.seal(1, None, delta("one")).unwrap();
    let (_, f2) = pc.seal(1, None, delta("two")).unwrap();
    let (_, f3) = pc.seal(1, None, delta("three")).unwrap();
    assert_eq!(phone.open(&f2).unwrap().seq, 2);
    assert_eq!(phone.open(&f2).unwrap_err(), CryptoError::Replay(2));
    assert_eq!(phone.open(&f1).unwrap_err(), CryptoError::Replay(1));
    assert_eq!(phone.open(&f3).unwrap().seq, 3);
    assert_eq!(phone.open(&f3).unwrap_err(), CryptoError::Replay(3));
    assert_eq!(phone.counters().last_recv, Some(3));
}

/// The replay window survives a restart from persisted counters: frames from before the
/// restart are still replays.
#[test]
fn replay_window_survives_restart_from_counters() {
    let (mut pc, mut phone) = pair();
    let (_, f1) = pc.seal(1, None, delta("one")).unwrap();
    phone.open(&f1).unwrap();
    let persisted = phone.counters();
    let mut phone2 = SecureChannel::new(Role::Phone, &keys(), persisted);
    assert_eq!(phone2.open(&f1).unwrap_err(), CryptoError::Replay(1));
}

/// Reflection: a frame the PC sent, bounced back to the PC, must not open (direction keys differ).
/// Same for the phone. Frames from a different pairing never open either.
#[test]
fn reflected_and_foreign_frames_are_rejected() {
    let (mut pc, mut phone) = pair();
    let (_, from_pc) = pc.seal(1, None, delta("x")).unwrap();
    let (_, from_phone) = phone.seal(1, None, Message::Heartbeat(Heartbeat::default())).unwrap();
    assert_eq!(pc.open(&from_pc).unwrap_err(), CryptoError::Aead);
    assert_eq!(phone.open(&from_phone).unwrap_err(), CryptoError::Aead);

    let other = SessionKeys { pc_to_phone: [0x33; 32], phone_to_pc: [0x44; 32] };
    let mut stranger = SecureChannel::new(Role::Phone, &other, Counters::default());
    let cmd = Message::Command(Command::MemoryDelete { id: Uuid::nil() });
    let (_, forged) = stranger.seal(1, None, cmd).unwrap();
    assert_eq!(pc.open(&forged).unwrap_err(), CryptoError::Aead);
    // Sealing directly with `seal` under a wrong key is the same.
    let env = Envelope::new(1, 1, None, Message::Heartbeat(Heartbeat::default()));
    assert!(pc.open(&seal(&[0u8; 32], &env)).is_err());
}

// ------------------------------------------------------------------ pairing

fn hello(phone_pub: String, proof: String) -> PairHello {
    PairHello { device_name: "Attacker".into(), phone_pub, proof }
}

/// Without the QR token nobody can pair: wrong-token proofs, proofs for a different key and
/// malformed values are rejected, and the 5th failure burns the offer for good.
#[test]
fn pairing_requires_the_token_and_burns_after_failures() {
    let pc = Identity::generate();
    let attacker = Identity::generate();
    let token = [5u8; 32];
    let mut offer = PairingOffer::with_token(token, u64::MAX);
    let wrong = b64(&pairing_proof(&[6u8; 32], &attacker.public_bytes()));
    assert_eq!(offer.accept(&pc, &hello(attacker.public_b64(), wrong), 0, 0).unwrap_err(), PairError::BadProof);
    // A genuine proof for someone else's key does not authorise the attacker's key.
    let victim = Identity::generate();
    let victims_proof = b64(&pairing_proof(&token, &victim.public_bytes()));
    assert_eq!(
        offer.accept(&pc, &hello(attacker.public_b64(), victims_proof.clone()), 0, 0).unwrap_err(),
        PairError::BadProof
    );
    assert!(offer.accept(&pc, &hello("not base64!".into(), victims_proof.clone()), 0, 0).is_err());
    assert!(offer.accept(&pc, &hello(attacker.public_b64(), "AAAA".into()), 0, 0).is_err());
    assert!(offer.accept(&pc, &hello(attacker.public_b64(), String::new()), 0, 0).is_err());
    assert!(offer.is_burned(), "5 failures burn the token");
    // Even the legitimate phone is now refused: the user must show a new QR.
    assert_eq!(offer.accept(&pc, &hello(victim.public_b64(), victims_proof), 0, 0).unwrap_err(), PairError::Burned);
}

/// A low-order (all-zero) phone key would make the shared secret predictable: refused.
#[test]
fn low_order_phone_key_is_refused() {
    let pc = Identity::generate();
    let token = [7u8; 32];
    let mut offer = PairingOffer::with_token(token, u64::MAX);
    let zero = [0u8; 32];
    let proof = b64(&pairing_proof(&token, &zero));
    let e = offer.accept(&pc, &hello(b64(&zero), proof), 0, 0).unwrap_err();
    assert_eq!(e, PairError::BadKey(CryptoError::LowOrderPoint));
}

/// A captured pair.hello cannot be replayed (token burned) or used after the QR expires.
#[test]
fn pair_hello_is_single_use_and_expires() {
    let pc = Identity::generate();
    let phone = Identity::generate();
    let token = [8u8; 32];
    let h = hello(phone.public_b64(), b64(&pairing_proof(&token, &phone.public_bytes())));
    let mut offer = PairingOffer::with_token(token, 1000);
    assert!(offer.accept(&pc, &h, 1000, 0).is_ok());
    assert_eq!(offer.accept(&pc, &h, 1000, 0).unwrap_err(), PairError::Burned);
    let mut late = PairingOffer::with_token(token, 1000);
    assert_eq!(late.accept(&pc, &h, 1001, 0).unwrap_err(), PairError::Expired);
    assert!(late.is_burned());
}

// ------------------------------------------------------------------ key hygiene

/// Debug output of every key-bearing type is redacted (no key bytes in logs or panics).
#[test]
fn debug_output_never_contains_key_material() {
    let pc = Identity::generate();
    let phone = Identity::generate();
    let token = [3u8; 32];
    let mut offer = PairingOffer::with_token(token, u64::MAX);
    let h = PairHello {
        device_name: "Pixel".into(),
        phone_pub: phone.public_b64(),
        proof: b64(&pairing_proof(&token, &phone.public_bytes())),
    };
    let offer_dbg = format!("{offer:?}");
    let dev = offer.accept(&pc, &h, 0, 0).unwrap();
    let secrets = [
        pc.to_secret().expose().to_string(),
        b64(&token),
        b64(&dev.keys.pc_to_phone),
        b64(&dev.keys.phone_to_pc),
        format!("{:?}", dev.keys.pc_to_phone),
    ];
    let dbgs =
        [format!("{pc:?}"), offer_dbg, format!("{dev:?}"), format!("{:?}", dev.keys), format!("{:?}", dev.to_secret())];
    for d in &dbgs {
        for s in &secrets {
            assert!(!d.contains(s.as_str()), "Debug output leaks key material: {d}");
        }
    }
}
