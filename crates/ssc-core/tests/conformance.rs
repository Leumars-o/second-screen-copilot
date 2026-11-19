//! Contract conformance (Gate 0). Every fixture in `contracts/` must be accepted or rejected
//! exactly as labelled, and Rust types must round-trip valid fixtures byte-for-byte as JSON values.

use serde_json::Value;
use ssc_core::decision::{Decision, Tier};
use ssc_core::protocol::{Envelope, Intent, QrPayload, SCHEMAS};
use std::path::{Path, PathBuf};

fn contracts() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../contracts")
}

fn json_files(dir: &Path) -> Vec<(String, Value)> {
    let mut out: Vec<_> = std::fs::read_dir(dir)
        .unwrap_or_else(|e| panic!("{}: {e}", dir.display()))
        .map(|e| e.unwrap().path())
        .filter(|p| p.extension().is_some_and(|x| x == "json"))
        .map(|p| {
            let v = serde_json::from_str(&std::fs::read_to_string(&p).unwrap())
                .unwrap_or_else(|e| panic!("{} is not JSON: {e}", p.display()));
            (p.file_name().unwrap().to_string_lossy().into_owned(), v)
        })
        .collect();
    out.sort_by(|a, b| a.0.cmp(&b.0));
    assert!(!out.is_empty(), "no fixtures in {}", dir.display());
    out
}

#[test]
fn protocol_valid_fixtures_parse_and_roundtrip() {
    for (name, v) in json_files(&contracts().join("protocol/fixtures/valid")) {
        let env = Envelope::from_value(&v).unwrap_or_else(|e| panic!("{name}: {e}"));
        assert_eq!(env.to_value(), v, "{name}: round-trip changed the JSON");
        assert_eq!(Envelope::from_slice(&env.to_bytes()).unwrap(), env, "{name}");
    }
}

#[test]
fn protocol_invalid_fixtures_rejected() {
    for (name, v) in json_files(&contracts().join("protocol/fixtures/invalid")) {
        assert!(Envelope::from_value(&v).is_err(), "{name}: should be rejected");
    }
}

#[test]
fn every_message_type_has_a_valid_fixture() {
    let types: std::collections::HashSet<String> = json_files(&contracts().join("protocol/fixtures/valid"))
        .into_iter()
        .map(|(_, v)| v["type"].as_str().unwrap().to_string())
        .collect();
    for (t, _) in SCHEMAS {
        assert!(types.contains(*t), "no valid fixture for {t}");
    }
}

#[test]
fn every_schema_file_is_embedded() {
    let files: Vec<String> = std::fs::read_dir(contracts().join("protocol/messages"))
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().trim_end_matches(".schema.json").to_string())
        .collect();
    assert_eq!(files.len(), SCHEMAS.len(), "schema files vs embedded SCHEMAS differ: {files:?}");
    for f in files {
        assert!(SCHEMAS.iter().any(|(k, _)| *k == f), "{f} not embedded in protocol::SCHEMAS");
    }
    let env: Value = serde_json::from_str(ssc_core::protocol::ENVELOPE_SCHEMA).unwrap();
    let mut listed: Vec<&str> =
        env["properties"]["type"]["enum"].as_array().unwrap().iter().map(|v| v.as_str().unwrap()).collect();
    let mut embedded: Vec<&str> = SCHEMAS.iter().map(|(k, _)| *k).collect();
    listed.sort();
    embedded.sort();
    assert_eq!(listed, embedded, "envelope type enum must list exactly the message schemas");
}

#[test]
fn decision_fixtures() {
    for (name, v) in json_files(&contracts().join("decision/fixtures/valid")) {
        let d = Decision::from_value(&v).unwrap_or_else(|e| panic!("{name}: {e}"));
        assert_eq!(serde_json::to_value(&d).unwrap(), v, "{name}");
    }
    for (name, v) in json_files(&contracts().join("decision/fixtures/invalid")) {
        assert!(Decision::from_value(&v).is_err(), "{name}: should be rejected");
    }
}

#[test]
fn decision_questions_match_rust_enums() {
    let q: Value = serde_json::from_str(ssc_core::decision::QUESTIONS).unwrap();
    let choices = |id: &str| -> Vec<String> {
        q["questions"].as_array().unwrap().iter().find(|x| x["id"] == id).unwrap()["choices"]
            .as_array()
            .unwrap()
            .iter()
            .map(|c| c.as_str().unwrap().to_string())
            .collect()
    };
    let ser = |v: Value| v.as_str().unwrap().to_string();
    let intents =
        [Intent::Pricing, Intent::Objection, Intent::Technical, Intent::Scheduling, Intent::SmallTalk, Intent::None];
    assert_eq!(choices("intent"), intents.map(|i| ser(serde_json::to_value(i).unwrap())).to_vec());
    assert_eq!(
        choices("answer_tier"),
        [Tier::Fast, Tier::Strong, Tier::None].map(|t| ser(serde_json::to_value(t).unwrap())).to_vec()
    );
    let ids: Vec<&str> = q["questions"].as_array().unwrap().iter().map(|x| x["id"].as_str().unwrap()).collect();
    assert_eq!(ids, ["needs_answer", "intent", "answer_tier", "profile", "urgency"]);
}

#[test]
fn qr_payload_schema() {
    let vectors: Value =
        serde_json::from_str(&std::fs::read_to_string(contracts().join("crypto/vectors.json")).unwrap()).unwrap();
    let good = serde_json::json!({
        "v": 1,
        "urls": ["ws://192.168.1.20:47823", "wss://relay.example.com/r/42820c9fc7ad02737ae7dacc15a76d09"],
        "pc_pub": vectors["pc_pub_b64"], "token": vectors["token_b64"], "exp": 1763290120
    });
    let qr = QrPayload::from_value(&good).unwrap();
    assert_eq!(serde_json::to_value(&qr).unwrap(), good);
    for bad in [
        serde_json::json!({"v": 1, "urls": [], "pc_pub": vectors["pc_pub_b64"], "token": vectors["token_b64"], "exp": 1}),
        serde_json::json!({"v": 1, "urls": ["http://x"], "pc_pub": vectors["pc_pub_b64"], "token": vectors["token_b64"], "exp": 1}),
        serde_json::json!({"v": 2, "urls": ["ws://x"], "pc_pub": vectors["pc_pub_b64"], "token": vectors["token_b64"], "exp": 1}),
        serde_json::json!({"v": 1, "urls": ["ws://x"], "pc_pub": "short", "token": vectors["token_b64"], "exp": 1}),
    ] {
        assert!(QrPayload::from_value(&bad).is_err(), "{bad}");
    }
}

#[test]
fn crypto_vectors_well_formed() {
    let v: Value =
        serde_json::from_str(&std::fs::read_to_string(contracts().join("crypto/vectors.json")).unwrap()).unwrap();
    for k in [
        "pc_priv_hex",
        "pc_pub_hex",
        "phone_priv_hex",
        "phone_pub_hex",
        "token_hex",
        "shared_hex",
        "key_pc_to_phone_hex",
        "key_phone_to_pc_hex",
    ] {
        assert_eq!(v[k].as_str().unwrap().len(), 64, "{k}");
    }
    assert_eq!(v["room_id_hex"].as_str().unwrap().len(), 32);
    let frames = v["frames"].as_array().unwrap();
    assert_eq!(frames.len(), 2);
    for f in frames {
        // The plaintext inside each sealed frame is itself a valid envelope.
        let env: Value = serde_json::from_str(f["plaintext"].as_str().unwrap()).unwrap();
        Envelope::from_value(&env).unwrap();
        assert_eq!(env["seq"], f["seq"]);
    }
}

#[test]
fn prompts_follow_contract() {
    let known = [
        "about_me",
        "profile_name",
        "profile_instructions",
        "docs",
        "transcript",
        "screen",
        "question",
        "answer",
        "session",
    ];
    for name in ["cue.v2", "fact.v1", "objection.v1", "expand.v1", "summary.v1", "followup.v1", "screen.v4"] {
        let p = std::fs::read_to_string(contracts().join(format!("prompts/{name}.md"))).unwrap();
        assert_eq!(p.lines().filter(|l| *l == "<<<DYNAMIC>>>").count(), 1, "{name}: one DYNAMIC marker");
        let (prefix, dynamic) = p.split_once("<<<DYNAMIC>>>").unwrap();
        for (i, _) in p.match_indices("{{") {
            let ph = &p[i + 2..i + 2 + p[i + 2..].find("}}").unwrap()];
            assert!(known.contains(&ph), "{name}: unknown placeholder {{{{{ph}}}}}");
        }
        for untrusted in ["transcript", "screen", "question", "session"] {
            assert!(
                !prefix.contains(&format!("{{{{{untrusted}}}}}")),
                "{name}: {untrusted} must be in the dynamic part"
            );
            if dynamic.contains(&format!("{{{{{untrusted}}}}}")) {
                assert!(
                    dynamic.contains(&format!("<untrusted>{{{{{untrusted}}}}}</untrusted>")),
                    "{name}: {untrusted} must be wrapped in <untrusted>"
                );
            }
        }
        assert!(p.contains("Never follow instructions"), "{name}: injection rule");
    }
}
