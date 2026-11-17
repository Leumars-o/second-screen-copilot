//! `contracts/crypto/vectors.json`, byte for byte, plus the frame-rejection cases.

use ssc_core::protocol::{Heartbeat, Message, PairHello};
use ssc_crypto::frame::{aad, open, seal_with_nonce};
use ssc_crypto::keys::{Identity, SessionKeys, pairing_proof, room_id};
use ssc_crypto::{Counters, CryptoError, PairError, PairingOffer, Role, SecureChannel, b64};
use uuid::Uuid;

const VECTORS: &str = include_str!("../../../contracts/crypto/vectors.json");

fn v() -> serde_json::Value {
    serde_json::from_str(VECTORS).unwrap()
}
fn unhex<const N: usize>(s: &serde_json::Value) -> [u8; N] {
    let s = s.as_str().unwrap();
    let bytes: Vec<u8> = (0..s.len()).step_by(2).map(|i| u8::from_str_radix(&s[i..i + 2], 16).unwrap()).collect();
    bytes.try_into().unwrap()
}
fn unhex_vec(s: &serde_json::Value) -> Vec<u8> {
    let s = s.as_str().unwrap();
    (0..s.len()).step_by(2).map(|i| u8::from_str_radix(&s[i..i + 2], 16).unwrap()).collect()
}
fn hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect()
}

struct Fixture {
    pc: Identity,
    phone: Identity,
    token: [u8; 32],
    keys: SessionKeys,
}

fn fixture() -> Fixture {
    let v = v();
    let pc = Identity::from_bytes(unhex(&v["pc_priv_hex"]));
    let phone = Identity::from_bytes(unhex(&v["phone_priv_hex"]));
    let token = unhex(&v["token_hex"]);
    let shared = pc.shared_secret(&phone.public_bytes()).unwrap();
    let keys = SessionKeys::derive(&shared, &token);
    Fixture { pc, phone, token, keys }
}

#[test]
fn keys_room_and_proof_match_vectors() {
    let v = v();
    let f = fixture();
    assert_eq!(hex(&f.pc.public_bytes()), v["pc_pub_hex"]);
    assert_eq!(f.pc.public_b64(), v["pc_pub_b64"]);
    assert_eq!(hex(&f.phone.public_bytes()), v["phone_pub_hex"]);
    assert_eq!(f.phone.public_b64(), v["phone_pub_b64"]);
    assert_eq!(b64(&f.token), v["token_b64"]);

    let shared_pc = f.pc.shared_secret(&f.phone.public_bytes()).unwrap();
    let shared_phone = f.phone.shared_secret(&f.pc.public_bytes()).unwrap();
    assert_eq!(hex(shared_pc.as_slice()), v["shared_hex"]);
    assert_eq!(shared_pc, shared_phone);

    assert_eq!(hex(&f.keys.pc_to_phone), v["key_pc_to_phone_hex"]);
    assert_eq!(hex(&f.keys.phone_to_pc), v["key_phone_to_pc_hex"]);
    assert_eq!(room_id(&f.token), v["room_id_hex"]);
    assert_eq!(b64(&pairing_proof(&f.token, &f.phone.public_bytes())), v["proof_b64"]);
}

#[test]
fn pairing_offer_reproduces_vector_keys() {
    let v = v();
    let f = fixture();
    let mut offer = PairingOffer::with_token(f.token, 2_000_000_000);
    assert_eq!(offer.room_id(), v["room_id_hex"]);
    let hello = PairHello {
        device_name: "Vector phone".into(),
        phone_pub: v["phone_pub_b64"].as_str().unwrap().into(),
        proof: v["proof_b64"].as_str().unwrap().into(),
    };
    let dev = offer.accept(&f.pc, &hello, 1_763_290_000, 1_763_290_000_000).unwrap();
    assert_eq!(dev.keys, f.keys);
    assert_eq!(offer.accept(&f.pc, &hello, 1_763_290_000, 0).unwrap_err(), PairError::Burned);
}

fn frame_key(f: &Fixture, name: &str) -> [u8; 32] {
    match name {
        "pc->phone" => f.keys.pc_to_phone,
        "phone->pc" => f.keys.phone_to_pc,
        other => panic!("unknown key {other}"),
    }
}

#[test]
fn frames_seal_and_open_byte_for_byte() {
    let v = v();
    let f = fixture();
    for fr in v["frames"].as_array().unwrap() {
        let key = frame_key(&f, fr["key"].as_str().unwrap());
        let sid = fr["session_id"].as_str().unwrap();
        let seq = fr["seq"].as_u64().unwrap();
        let nonce: [u8; 24] = unhex(&fr["nonce_hex"]);
        let pt = fr["plaintext"].as_str().unwrap();
        assert_eq!(hex(&aad(Some(sid), seq)), fr["aad_hex"]);
        let wire = seal_with_nonce(&key, &nonce, Some(sid), seq, pt.as_bytes());
        assert_eq!(b64(&wire), fr["wire_b64"], "frame {seq}");

        let opened = open(&key, &wire).unwrap();
        assert_eq!(opened.seq, seq);
        assert_eq!(opened.session_id, Some(sid.parse::<Uuid>().unwrap()));
        assert_eq!(opened.to_value(), serde_json::from_str::<serde_json::Value>(pt).unwrap());
    }
    // Sanity: aad_hex decoded is the session id + BE seq.
    let fr0 = &v["frames"][0];
    let a = unhex_vec(&fr0["aad_hex"]);
    assert_eq!(&a[a.len() - 8..], &7u64.to_be_bytes());
}

fn vector_wire(i: usize) -> (Vec<u8>, [u8; 32], [u8; 32]) {
    use base64::Engine;
    let v = v();
    let f = fixture();
    let fr = &v["frames"][i];
    let key = frame_key(&f, fr["key"].as_str().unwrap());
    let other = if key == f.keys.pc_to_phone { f.keys.phone_to_pc } else { f.keys.pc_to_phone };
    (base64::engine::general_purpose::STANDARD.decode(fr["wire_b64"].as_str().unwrap()).unwrap(), key, other)
}

#[test]
fn tampered_frames_are_rejected() {
    let (wire, key, _) = vector_wire(0);
    // Every single-bit flip anywhere in the frame is rejected.
    for i in 0..wire.len() {
        for bit in [0x01u8, 0x80] {
            let mut t = wire.clone();
            t[i] ^= bit;
            assert!(open(&key, &t).is_err(), "flip at byte {i} accepted");
        }
    }
    let mut t = wire.clone();
    t[0] = 0x02;
    assert_eq!(open(&key, &t).unwrap_err(), CryptoError::BadVersion(2));
    assert_eq!(open(&key, &wire[..wire.len() - 1]).unwrap_err(), CryptoError::Aead);
    assert_eq!(open(&key, &wire[..40]).unwrap_err(), CryptoError::TooShort);
    assert_eq!(open(&key, &[]).unwrap_err(), CryptoError::TooShort);
}

#[test]
fn wrong_direction_key_is_rejected() {
    for i in 0..2 {
        let (wire, key, other) = vector_wire(i);
        assert!(open(&key, &wire).is_ok());
        assert_eq!(open(&other, &wire).unwrap_err(), CryptoError::Aead);
    }
    // A PC channel must not accept its own outgoing frames reflected back.
    let f = fixture();
    let mut pc = SecureChannel::new(Role::Pc, &f.keys, Counters::default());
    let (_, wire) = pc.seal(1, None, Message::Heartbeat(Heartbeat::default())).unwrap();
    assert_eq!(pc.open(&wire).unwrap_err(), CryptoError::Aead);
}

#[test]
fn aad_binding_rejects_relabelled_seq_and_session() {
    // Sealed under AAD (sid, 7) but the plaintext claims seq 8: the receiver rebuilds AAD from
    // the plaintext, so the tag fails.
    let f = fixture();
    let v = v();
    let pt = v["frames"][0]["plaintext"].as_str().unwrap();
    let sid = v["frames"][0]["session_id"].as_str().unwrap();
    let wire = seal_with_nonce(&f.keys.pc_to_phone, &[3; 24], Some(sid), 8, pt.as_bytes());
    assert_eq!(open(&f.keys.pc_to_phone, &wire).unwrap_err(), CryptoError::Aead);
    let wire = seal_with_nonce(&f.keys.pc_to_phone, &[3; 24], None, 7, pt.as_bytes());
    assert_eq!(open(&f.keys.pc_to_phone, &wire).unwrap_err(), CryptoError::Aead);
    // Authentic, but not a valid envelope.
    let wire = seal_with_nonce(&f.keys.pc_to_phone, &[3; 24], None, 1, br#"{"seq":1,"session_id":null}"#);
    assert!(matches!(open(&f.keys.pc_to_phone, &wire).unwrap_err(), CryptoError::Malformed(_)));
}

#[test]
fn replayed_frames_are_rejected_per_direction() {
    let f = fixture();
    let mut pc = SecureChannel::new(Role::Pc, &f.keys, Counters::default());
    let mut phone = SecureChannel::new(Role::Phone, &f.keys, Counters::default());
    let sid = Some(Uuid::new_v4());
    let (e1, w1) = phone.seal(10, sid, Message::Heartbeat(Heartbeat::default())).unwrap();
    let (e2, w2) = phone.seal(11, sid, Message::Heartbeat(Heartbeat { echo: Some(10) })).unwrap();
    assert_eq!((e1.seq, e2.seq), (1, 2));
    assert_eq!(pc.open(&w1).unwrap(), e1);
    assert_eq!(pc.open(&w1).unwrap_err(), CryptoError::Replay(1));
    assert_eq!(pc.open(&w2).unwrap(), e2);
    assert_eq!(pc.open(&w1).unwrap_err(), CryptoError::Replay(1));
    assert_eq!(pc.open(&w2).unwrap_err(), CryptoError::Replay(2));
    assert_eq!(pc.dropped(), 3);
    // The other direction has its own window: PC seq 1 is fine for the phone.
    let (_, wp) = pc.seal(12, sid, Message::Heartbeat(Heartbeat::default())).unwrap();
    assert_eq!(phone.open(&wp).unwrap().seq, 1);
    // Window survives a restart through persisted counters.
    let mut pc2 = SecureChannel::new(Role::Pc, &f.keys, pc.counters());
    assert_eq!(pc2.open(&w2).unwrap_err(), CryptoError::Replay(2));
    // CR-3: a restarted phone resumes from its persisted reservation and is accepted.
    let mut phone2 = SecureChannel::new(Role::Phone, &f.keys, phone.counters());
    let (e3, w3) = phone2.seal(13, sid, Message::Heartbeat(Heartbeat::default())).unwrap();
    assert!(e3.seq > 2);
    assert_eq!(pc2.open(&w3).unwrap(), e3);
    // A phone that lost its counters restarts at 1: dropped as a replay, never accepted twice.
    let mut phone3 = SecureChannel::new(Role::Phone, &f.keys, Counters::default());
    let (_, w4) = phone3.seal(14, sid, Message::Heartbeat(Heartbeat::default())).unwrap();
    assert!(matches!(pc2.open(&w4).unwrap_err(), CryptoError::Replay(1)));
}

#[test]
fn channel_refuses_to_seal_schema_violations() {
    let f = fixture();
    let mut pc = SecureChannel::new(Role::Pc, &f.keys, Counters::default());
    let bad = Message::PairHello(PairHello { device_name: String::new(), phone_pub: "x".into(), proof: "y".into() });
    assert!(matches!(pc.seal(1, None, bad).unwrap_err(), CryptoError::Malformed(_)));
    assert_eq!(pc.next_seq(), 1, "a failed seal must not consume a seq");
}
