"""Reference generator for contracts/crypto/vectors.json.

Independent of the Rust (RustCrypto) and TS (libsodium/WebCrypto) code under test:
X25519, HKDF and HMAC come from `cryptography` (OpenSSL); XChaCha20-Poly1305 from PyNaCl (libsodium).
Run once; the output is committed and frozen with contracts-v1.

    uv venv .venv && VIRTUAL_ENV=.venv uv pip install pynacl cryptography
    .venv/bin/python -I gen_vectors.py > vectors.json
"""
import base64, hashlib, hmac, json, struct
from cryptography.hazmat.primitives.asymmetric.x25519 import X25519PrivateKey
from cryptography.hazmat.primitives.kdf.hkdf import HKDF
from cryptography.hazmat.primitives import hashes, serialization
from nacl.bindings import crypto_aead_xchacha20poly1305_ietf_encrypt as xseal

def b64(b): return base64.b64encode(b).decode()
def hx(b): return b.hex()
def raw_pub(priv): return priv.public_key().public_bytes(serialization.Encoding.Raw, serialization.PublicFormat.Raw)
def hkdf(ikm, salt, info, n): return HKDF(algorithm=hashes.SHA256(), length=n, salt=salt, info=info).derive(ikm)

pc_priv_b = bytes(range(1, 33))
phone_priv_b = bytes(range(101, 133))
token = bytes(range(200, 232))
pc_priv = X25519PrivateKey.from_private_bytes(pc_priv_b)
phone_priv = X25519PrivateKey.from_private_bytes(phone_priv_b)
pc_pub, phone_pub = raw_pub(pc_priv), raw_pub(phone_priv)
shared = pc_priv.exchange(X25519PrivateKey.from_private_bytes(phone_priv_b).public_key())
assert shared == phone_priv.exchange(pc_priv.public_key())

k_pc_phone = hkdf(shared, token, b"ssc/v1/pc->phone", 32)
k_phone_pc = hkdf(shared, token, b"ssc/v1/phone->pc", 32)
room = hkdf(token, b"", b"ssc/v1/room", 16)
proof = hmac.new(token, phone_pub, hashlib.sha256).digest()

SID = "3f2b8c1e-9a4d-4e7b-8c21-5d6f7a8b9c0d"
def frame(key, nonce, seq, plaintext):
    aad = SID.encode() + struct.pack(">Q", seq)
    ct = xseal(plaintext.encode(), aad, nonce, key)
    return {"key": "pc->phone" if key == k_pc_phone else "phone->pc", "session_id": SID, "seq": seq,
            "nonce_hex": hx(nonce), "aad_hex": hx(aad), "plaintext": plaintext,
            "wire_b64": b64(b"\x01" + nonce + ct)}

frames = [
    frame(k_pc_phone, bytes(range(0, 24)), 7,
          json.dumps({"v": 1, "type": "answer.delta", "seq": 7, "ts": 1763290000123, "session_id": SID,
                      "payload": {"answer_id": "a1b2c3d4-e5f6-4a7b-8c9d-0e1f2a3b4c5d", "block": "cue", "text": "1. Requirements"}},
                     separators=(",", ":"))),
    frame(k_phone_pc, bytes(range(24, 48)), 3,
          json.dumps({"v": 1, "type": "heartbeat", "seq": 3, "ts": 1763290000999, "session_id": SID, "payload": {}},
                     separators=(",", ":"))),
]
print(json.dumps({
    "description": "Frozen with contracts-v1. See README.md for the algorithm. Keys are test-only.",
    "pc_priv_hex": hx(pc_priv_b), "pc_pub_hex": hx(pc_pub), "pc_pub_b64": b64(pc_pub),
    "phone_priv_hex": hx(phone_priv_b), "phone_pub_hex": hx(phone_pub), "phone_pub_b64": b64(phone_pub),
    "token_hex": hx(token), "token_b64": b64(token),
    "shared_hex": hx(shared),
    "key_pc_to_phone_hex": hx(k_pc_phone), "key_phone_to_pc_hex": hx(k_phone_pc),
    "room_id_hex": hx(room),
    "proof_b64": b64(proof),
    "frames": frames,
}, indent=2))
