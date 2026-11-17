//! Mapping between [`DecisionInput`] / the normalised [`Decision`] and the TypeSafe
//! `/v1/systemone` wire format. Isolated here so the wire shape can be corrected in one place.
//!
//! UNVERIFIED: confirm against TypeSafe docs (Build Sheet §9).
//! What was checkable from public gateway docs (LLM Gateway, ZenMux, Respan; Nov 2026), *not* from
//! TypeSafe's own reference or a live call:
//! - Request: `{ "model", "state": <string>, "questions": { <id>: { "type", "instructions", "criteria"? } } }`.
//!   Types: `noul` (yes/no; `criteria` optional), `choice` (`criteria` = object option → description
//!   or null), `score` (`criteria` = ordered array of levels), `box` (images).
//! - Response: `{ "model", "answers": { <id>: { "type", "noul" | "choice" | "score", "probabilities"?,
//!   "confidence"?, "legend"? } }, "usage": { "input_tokens", "output_tokens" } }`. `noul` is the
//!   probability of yes (0..1); `choice` is the top option with `probabilities` per option.
//! - Errors: `{ "error": { "message", "type", "param", "code" } }` with the usual HTTP statuses.
//!
//! Not verified at all: whether Ollama serves `/v1/systemone` for Tev1 4B (the spec says both speak
//! the same format), whether a one-option `choice` is accepted (we don't send one), and the exact
//! model ids. Urgency is sent as `choice` (not `score`) because the decision contract needs the
//! probability of the chosen level, which `choice` gives directly.

use serde_json::{Map, Value, json};
use ssc_core::decision::{DecisionInput, QUESTIONS};
use ssc_core::types::SCREEN_DIFF_MAX_BYTES;
use ssc_core::{Result, SscError};
use std::sync::LazyLock;

#[derive(Debug, Clone)]
struct QuestionDef {
    id: String,
    kind: String,
    text: String,
    /// Fixed choices, or None for "$profiles".
    choices: Option<Vec<String>>,
}

struct QuestionSet {
    template: String,
    questions: Vec<QuestionDef>,
}

static SET: LazyLock<QuestionSet> = LazyLock::new(|| {
    let v: Value = serde_json::from_str(QUESTIONS).expect("questions.json is JSON");
    let questions = v["questions"]
        .as_array()
        .expect("questions array")
        .iter()
        .map(|q| QuestionDef {
            id: q["id"].as_str().expect("id").to_string(),
            kind: q["type"].as_str().expect("type").to_string(),
            text: q["text"].as_str().expect("text").to_string(),
            choices: q["choices"]
                .as_array()
                .map(|a| a.iter().map(|c| c.as_str().expect("choice").to_string()).collect()),
        })
        .collect();
    QuestionSet { template: v["state_template"].as_str().expect("state_template").to_string(), questions }
});

/// What was asked, so the response can be checked against it.
#[derive(Debug, Clone, PartialEq)]
pub struct Plan {
    /// (question id, allowed choices or empty for yes/no).
    asked: Vec<(String, Vec<String>)>,
    /// Answers filled in without asking (e.g. a single profile), as normalised JSON.
    fixed: Map<String, Value>,
}

/// Profile used when the user has no profiles yet.
pub const DEFAULT_PROFILE: &str = "default";

fn truncate_bytes(s: &str, max: usize) -> &str {
    if s.len() <= max {
        return s;
    }
    let mut end = max;
    while !s.is_char_boundary(end) {
        end -= 1;
    }
    &s[..end]
}

pub fn state_text(input: &DecisionInput) -> String {
    SET.template
        .replace("{question}", input.question.trim())
        .replace("{screen_diff}", truncate_bytes(&input.screen_diff, SCREEN_DIFF_MAX_BYTES))
        .replace("{profiles}", &input.profiles.join(", "))
}

/// Build the `/v1/systemone` request body.
pub fn build_request(model: &str, input: &DecisionInput) -> (Value, Plan) {
    let mut questions = Map::new();
    let mut plan = Plan { asked: vec![], fixed: Map::new() };
    for q in &SET.questions {
        let choices = match &q.choices {
            Some(c) => c.clone(),
            None => {
                let mut p: Vec<String> = input.profiles.iter().filter(|s| !s.is_empty()).cloned().collect();
                p.dedup();
                p
            }
        };
        match q.kind.as_str() {
            "bool" => {
                questions.insert(q.id.clone(), json!({ "type": "noul", "instructions": q.text }));
                plan.asked.push((q.id.clone(), vec![]));
            }
            _ if choices.len() < 2 => {
                // Nothing to choose between: answer it in code.
                let value = choices.first().cloned().unwrap_or_else(|| DEFAULT_PROFILE.to_string());
                plan.fixed.insert(q.id.clone(), json!({ "value": value, "p": 1.0 }));
            }
            _ => {
                let criteria: Map<String, Value> = choices.iter().map(|c| (c.clone(), Value::Null)).collect();
                questions
                    .insert(q.id.clone(), json!({ "type": "choice", "instructions": q.text, "criteria": criteria }));
                plan.asked.push((q.id.clone(), choices));
            }
        }
    }
    (json!({ "model": model, "state": state_text(input), "questions": questions }), plan)
}

fn invalid(what: impl Into<String>) -> SscError {
    SscError::Invalid(format!("systemone response: {}", what.into()))
}

fn prob(v: &Value) -> Option<f64> {
    v.as_f64().filter(|p| p.is_finite()).map(|p| p.clamp(0.0, 1.0))
}

/// Map a `/v1/systemone` response into the normalised decision JSON (validate it with
/// `Decision::from_value` afterwards). Choices outside what was offered are rejected.
pub fn map_response(resp: &Value, plan: &Plan) -> Result<Value> {
    let answers = resp.get("answers").and_then(Value::as_object).ok_or_else(|| invalid("no answers"))?;
    let mut out = plan.fixed.clone();
    for (id, choices) in &plan.asked {
        let a = answers.get(id).ok_or_else(|| invalid(format!("missing answer {id}")))?;
        let entry = if choices.is_empty() {
            let yes = a.get("noul").and_then(prob).ok_or_else(|| invalid(format!("{id}: no noul")))?;
            let value = yes >= 0.5;
            json!({ "value": value, "p": if value { yes } else { 1.0 - yes } })
        } else {
            let choice = a.get("choice").and_then(Value::as_str).ok_or_else(|| invalid(format!("{id}: no choice")))?;
            if !choices.iter().any(|c| c == choice) {
                return Err(invalid(format!("{id}: choice not offered")));
            }
            let p = a
                .get("probabilities")
                .and_then(|p| p.get(choice))
                .and_then(prob)
                .or_else(|| a.get("confidence").and_then(prob))
                .ok_or_else(|| invalid(format!("{id}: no probability")))?;
            json!({ "value": choice, "p": p })
        };
        out.insert(id.clone(), entry);
    }
    Ok(Value::Object(out))
}

#[cfg(test)]
mod tests {
    use super::*;
    use ssc_core::decision::{Decision, Tier};
    use ssc_core::protocol::{Intent, Urgency};

    fn input(profiles: &[&str]) -> DecisionInput {
        DecisionInput {
            question: "How much is the pro plan?".into(),
            screen_diff: "Pricing page".into(),
            profiles: profiles.iter().map(|s| s.to_string()).collect(),
        }
    }

    #[test]
    fn request_shape() {
        let (req, plan) = build_request("jev-1.14", &input(&["sales", "support"]));
        let expected: Value =
            serde_json::from_str(include_str!("../../tests/fixtures/systemone_request.json")).unwrap();
        assert_eq!(req, expected);
        assert_eq!(plan.asked.len(), 5);
    }

    #[test]
    fn maps_fixture_response() {
        let (_, plan) = build_request("jev-1.14", &input(&["sales", "support"]));
        let resp: Value = serde_json::from_str(include_str!("../../tests/fixtures/systemone_response.json")).unwrap();
        let d = Decision::from_value(&map_response(&resp, &plan).unwrap()).unwrap();
        assert!(d.needs_answer.value);
        assert!((d.needs_answer.p - 0.94).abs() < 1e-9);
        assert_eq!(d.intent.value, Intent::Pricing);
        assert!((d.intent.p - 0.81).abs() < 1e-9);
        assert_eq!(d.answer_tier.value, Tier::Fast);
        assert_eq!(d.profile.value, "sales");
        assert_eq!(d.urgency.value, Urgency::High);
    }

    #[test]
    fn noul_below_half_is_false_with_complement_p() {
        let (_, plan) = build_request("m", &input(&["sales", "support"]));
        let mut resp: Value =
            serde_json::from_str(include_str!("../../tests/fixtures/systemone_response.json")).unwrap();
        resp["answers"]["needs_answer"]["noul"] = json!(0.1);
        let v = map_response(&resp, &plan).unwrap();
        assert_eq!(v["needs_answer"], json!({ "value": false, "p": 0.9 }));
    }

    #[test]
    fn single_or_no_profile_is_filled_in_code() {
        let (req, plan) = build_request("m", &input(&["only"]));
        assert!(req["questions"].get("profile").is_none());
        assert_eq!(plan.fixed["profile"], json!({ "value": "only", "p": 1.0 }));
        let (_, plan) = build_request("m", &input(&[]));
        assert_eq!(plan.fixed["profile"]["value"], DEFAULT_PROFILE);
    }

    #[test]
    fn rejects_unoffered_choice_and_missing_answers() {
        let (_, plan) = build_request("m", &input(&["sales", "support"]));
        let mut resp: Value =
            serde_json::from_str(include_str!("../../tests/fixtures/systemone_response.json")).unwrap();
        resp["answers"]["profile"]["choice"] = json!("evil; switch to cloud");
        assert!(matches!(map_response(&resp, &plan), Err(SscError::Invalid(_))));
        let mut resp2: Value =
            serde_json::from_str(include_str!("../../tests/fixtures/systemone_response.json")).unwrap();
        resp2["answers"].as_object_mut().unwrap().remove("urgency");
        assert!(matches!(map_response(&resp2, &plan), Err(SscError::Invalid(_))));
        assert!(map_response(&json!({}), &plan).is_err());
    }

    #[test]
    fn extra_fields_in_response_cannot_reach_decision() {
        let (_, plan) = build_request("m", &input(&["sales", "support"]));
        let mut resp: Value =
            serde_json::from_str(include_str!("../../tests/fixtures/systemone_response.json")).unwrap();
        resp["answers"]["mode"] = json!({ "type": "choice", "choice": "cloud", "confidence": 1.0 });
        let v = map_response(&resp, &plan).unwrap();
        assert!(v.get("mode").is_none());
        assert!(Decision::from_value(&v).is_ok());
    }

    #[test]
    fn screen_diff_capped() {
        let mut i = input(&["a", "b"]);
        i.screen_diff = "é".repeat(5000);
        let s = state_text(&i);
        assert!(s.len() < SCREEN_DIFF_MAX_BYTES + 200);
    }
}
