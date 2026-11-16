//! Normalised decision-model result (Build Sheet §4.5, `contracts/decision/`).
//!
//! Safety property: [`Decision`] has no field that can change mode, redaction or where data goes.
//! Those rules live in code (`ssc-router`, `ssc-redact`) and never read from a model.

use crate::protocol::{Intent, ProtocolError, Urgency};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::sync::OnceLock;

pub const DECISION_SCHEMA: &str = include_str!("../../../contracts/decision/decision.schema.json");
pub const QUESTIONS: &str = include_str!("../../../contracts/decision/questions.json");

/// A chosen value with the model's probability for it (0..=1).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Scored<T> {
    pub value: T,
    pub p: f64,
}

impl<T> Scored<T> {
    pub fn new(value: T, p: f64) -> Self {
        Self { value, p }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Tier {
    Fast,
    Strong,
    None,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Decision {
    pub needs_answer: Scored<bool>,
    pub intent: Scored<Intent>,
    pub answer_tier: Scored<Tier>,
    /// Profile id whose docs to search.
    pub profile: Scored<String>,
    pub urgency: Scored<Urgency>,
}

impl Decision {
    /// Validate against `decision.schema.json`, then deserialise. Adapters call this on their
    /// mapped output so a malformed provider response can never reach the trigger engine.
    pub fn from_value(value: &Value) -> Result<Self, ProtocolError> {
        static V: OnceLock<jsonschema::Validator> = OnceLock::new();
        let v = V.get_or_init(|| crate::protocol::compile(DECISION_SCHEMA));
        v.validate(value).map_err(|e| ProtocolError::Schema {
            schema: "decision".into(),
            detail: format!("{} at {}", e, e.instance_path()),
        })?;
        Ok(serde_json::from_value(value.clone())?)
    }
}

/// What the trigger engine sends to the decision model. Keep under ~500 tokens in total.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DecisionInput {
    /// Last "them" question (or utterance).
    pub question: String,
    /// Screen diff text, already trimmed to fit the budget.
    pub screen_diff: String,
    /// Allowed profile ids (fills `$profiles` in `questions.json`).
    pub profiles: Vec<String>,
}
