//! Wire protocol v1 (Build Sheet §4.1–4.2). The JSON Schemas in `contracts/protocol/` are the
//! source of truth: [`Envelope::from_value`] validates against them before deserialising, so
//! Rust and TypeScript accept exactly the same frames.
//!
//! Encryption is not done here; `ssc-crypto` seals the bytes from [`Envelope::to_bytes`].

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use std::collections::HashMap;
use std::sync::OnceLock;
use uuid::Uuid;

pub const PROTOCOL_VERSION: u8 = 1;

// ---------- shared enums ----------

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Mode {
    Cloud,
    Local,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Speaker {
    /// Microphone: the user.
    Me,
    /// System audio: the other side of the call.
    Them,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SessionEndReason {
    User,
    Timeout,
    Error,
    Disconnect,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Trigger {
    Hotkey,
    Question,
    Screen,
    Timer,
    /// Regenerate / shorter / number / follow-up from the phone.
    Command,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AnswerFormat {
    Cue,
    Fact,
    Objection,
    /// CR-20: screen hotkey. Free-text answer (block `cue`), then working lines (block `detail`).
    Screen,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Intent {
    Pricing,
    Objection,
    Technical,
    Scheduling,
    SmallTalk,
    None,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Urgency {
    Low,
    Medium,
    High,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Block {
    Cue,
    Detail,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AnswerEndReason {
    Done,
    Cancelled,
    Error,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Layer {
    AboutMe,
    Profile,
    Contact,
    Account,
    Session,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Rating {
    Up,
    Down,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PhoneStage {
    PhoneRendered,
}

// ---------- payloads ----------

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PairHello {
    pub device_name: String,
    /// X25519 public key, standard base64 with padding.
    pub phone_pub: String,
    /// HMAC-SHA256(token, raw phone_pub), base64.
    pub proof: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SessionStart {
    pub session_id: Uuid,
    pub profile_id: String,
    pub mode: Mode,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SessionEnd {
    pub session_id: Uuid,
    pub reason: SessionEndReason,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AnswerStart {
    pub answer_id: Uuid,
    pub trigger: Trigger,
    pub format: AnswerFormat,
    pub intent: Intent,
    pub urgency: Urgency,
    pub model: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AnswerDelta {
    pub answer_id: Uuid,
    pub block: Block,
    pub text: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Cue {
    pub text: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expand: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AnswerEnd {
    pub answer_id: Uuid,
    pub reason: AnswerEndReason,
    pub cues: Vec<Cue>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fact: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_doc: Option<String>,
    pub not_in_docs: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TranscriptPartial {
    pub speaker: Speaker,
    pub text: String,
    #[serde(rename = "final")]
    pub is_final: bool,
    pub t0: u64,
    pub t1: u64,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Heartbeat {
    /// `ts` of the peer heartbeat being answered (round-trip measurement).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub echo: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ContactSuggestion {
    pub id: Uuid,
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub company: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub notes: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SessionSummary {
    pub session_id: Uuid,
    pub summary: String,
    pub action_items: Vec<String>,
    pub followup_email: String,
    pub contact_suggestions: Vec<ContactSuggestion>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MemoryItem {
    pub id: Uuid,
    pub layer: Layer,
    pub title: String,
    pub body: String,
    pub updated_at: u64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MemorySnapshot {
    pub items: Vec<MemoryItem>,
}

/// CR-23: agent health for the phone.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AgentStatus {
    pub health: Health,
    /// One line for the phone's main screen ("All good · …" or the first problem).
    pub summary: String,
    pub checks: Vec<HealthCheck>,
    pub capturing: bool,
    /// PC clock, unix ms.
    pub t: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Health {
    Ok,
    Warn,
    Error,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HealthCheck {
    /// `[a-z_]{1,32}`, e.g. "screen_recording".
    pub id: String,
    pub label: String,
    pub state: CheckState,
    pub detail: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CheckState {
    Ok,
    Warn,
    Error,
    /// Not in use (e.g. capture stopped): not a problem.
    Off,
}

/// CR-23: agent log lines for the phone's Logs tab.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AgentLog {
    pub entries: Vec<LogEntry>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LogEntry {
    /// PC clock, unix ms.
    pub t: u64,
    pub level: LogLevel,
    pub source: String,
    pub text: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LogLevel {
    Info,
    Warn,
    Error,
}

/// Create (no `id`) or edit a memory item.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MemoryEdit {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<Uuid>,
    pub layer: Layer,
    pub title: String,
    pub body: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum FeedbackItem {
    Rating {
        value: Rating,
    },
    /// First `answer.delta` received → first paint, measured on the phone.
    Timing {
        stage: PhoneStage,
        delta_ms: u32,
    },
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "action", deny_unknown_fields)]
pub enum Command {
    #[serde(rename = "pin")]
    Pin { answer_id: Uuid },
    #[serde(rename = "regenerate")]
    Regenerate { answer_id: Uuid },
    #[serde(rename = "shorter")]
    Shorter { answer_id: Uuid },
    #[serde(rename = "number")]
    Number { answer_id: Uuid },
    #[serde(rename = "expand_all")]
    ExpandAll { answer_id: Uuid },
    /// CR-21: the screen hotkey, from the phone.
    #[serde(rename = "screen")]
    Screen,
    /// CR-23: send `agent.status` and the `agent.log` backlog now.
    #[serde(rename = "agent.sync")]
    AgentSync,
    #[serde(rename = "followup")]
    Followup {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        answer_id: Option<Uuid>,
        text: String,
    },
    #[serde(rename = "feedback")]
    Feedback { answer_id: Uuid, item: FeedbackItem },
    #[serde(rename = "memory.list")]
    MemoryList {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        layer: Option<Layer>,
    },
    #[serde(rename = "memory.update")]
    MemoryUpdate { item: MemoryEdit },
    #[serde(rename = "memory.delete")]
    MemoryDelete { id: Uuid },
    #[serde(rename = "contact.confirm")]
    ContactConfirm { suggestion_id: Uuid, accept: bool },
}

/// Every message type. Serialises adjacently as `{"type": ..., "payload": ...}`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", content = "payload")]
pub enum Message {
    #[serde(rename = "pair.hello")]
    PairHello(PairHello),
    #[serde(rename = "session.start")]
    SessionStart(SessionStart),
    #[serde(rename = "session.end")]
    SessionEnd(SessionEnd),
    #[serde(rename = "answer.start")]
    AnswerStart(AnswerStart),
    #[serde(rename = "answer.delta")]
    AnswerDelta(AnswerDelta),
    #[serde(rename = "answer.end")]
    AnswerEnd(AnswerEnd),
    #[serde(rename = "transcript.partial")]
    TranscriptPartial(TranscriptPartial),
    #[serde(rename = "command")]
    Command(Command),
    #[serde(rename = "heartbeat")]
    Heartbeat(Heartbeat),
    #[serde(rename = "session.summary")]
    SessionSummary(SessionSummary),
    #[serde(rename = "memory.snapshot")]
    MemorySnapshot(MemorySnapshot),
    #[serde(rename = "agent.status")]
    AgentStatus(AgentStatus),
    #[serde(rename = "agent.log")]
    AgentLog(AgentLog),
}

impl Message {
    /// The wire `type` string.
    pub fn type_name(&self) -> &'static str {
        match self {
            Message::PairHello(_) => "pair.hello",
            Message::SessionStart(_) => "session.start",
            Message::SessionEnd(_) => "session.end",
            Message::AnswerStart(_) => "answer.start",
            Message::AnswerDelta(_) => "answer.delta",
            Message::AnswerEnd(_) => "answer.end",
            Message::TranscriptPartial(_) => "transcript.partial",
            Message::Command(_) => "command",
            Message::Heartbeat(_) => "heartbeat",
            Message::SessionSummary(_) => "session.summary",
            Message::MemorySnapshot(_) => "memory.snapshot",
            Message::AgentStatus(_) => "agent.status",
            Message::AgentLog(_) => "agent.log",
        }
    }
}

// ---------- envelope ----------

/// The plaintext frame. `seq` increases by one per direction; receivers drop `seq <= last seen`.
#[derive(Debug, Clone, PartialEq)]
pub struct Envelope {
    pub seq: u64,
    /// Sender clock, unix ms.
    pub ts: u64,
    pub session_id: Option<Uuid>,
    pub message: Message,
}

#[derive(Debug, thiserror::Error)]
pub enum ProtocolError {
    #[error("not JSON: {0}")]
    Json(#[from] serde_json::Error),
    #[error("schema violation in {schema}: {detail}")]
    Schema { schema: String, detail: String },
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawEnvelope {
    #[allow(dead_code)]
    v: u8,
    #[serde(rename = "type")]
    kind: String,
    seq: u64,
    ts: u64,
    session_id: Option<Uuid>,
    payload: Value,
}

impl Envelope {
    pub fn new(seq: u64, ts: u64, session_id: Option<Uuid>, message: Message) -> Self {
        Self { seq, ts, session_id, message }
    }

    /// Serialise to the wire JSON object.
    pub fn to_value(&self) -> Value {
        let tagged = serde_json::to_value(&self.message).expect("protocol types always serialise");
        let mut obj = Map::new();
        obj.insert("v".into(), Value::from(PROTOCOL_VERSION));
        obj.insert("type".into(), tagged["type"].clone());
        obj.insert("seq".into(), Value::from(self.seq));
        obj.insert("ts".into(), Value::from(self.ts));
        obj.insert("session_id".into(), self.session_id.map_or(Value::Null, |s| Value::String(s.to_string())));
        obj.insert("payload".into(), tagged["payload"].clone());
        Value::Object(obj)
    }

    pub fn to_bytes(&self) -> Vec<u8> {
        serde_json::to_vec(&self.to_value()).expect("Value always serialises")
    }

    /// Validate against the schemas, then deserialise.
    pub fn from_value(value: &Value) -> Result<Self, ProtocolError> {
        validate(value)?;
        let raw: RawEnvelope = serde_json::from_value(value.clone())?;
        let message: Message = serde_json::from_value(serde_json::json!({ "type": raw.kind, "payload": raw.payload }))?;
        Ok(Self { seq: raw.seq, ts: raw.ts, session_id: raw.session_id, message })
    }

    pub fn from_slice(bytes: &[u8]) -> Result<Self, ProtocolError> {
        Self::from_value(&serde_json::from_slice(bytes)?)
    }
}

/// Validate a wire envelope (and its payload) against the frozen JSON Schemas.
pub fn validate(value: &Value) -> Result<(), ProtocolError> {
    let v = validators();
    check(&v.envelope, "envelope", value)?;
    let kind = value["type"].as_str().unwrap_or_default();
    let payload_validator = v
        .payloads
        .get(kind)
        .ok_or_else(|| ProtocolError::Schema { schema: "envelope".into(), detail: format!("unknown type {kind}") })?;
    check(payload_validator, kind, &value["payload"])
}

fn check(validator: &jsonschema::Validator, name: &str, value: &Value) -> Result<(), ProtocolError> {
    validator.validate(value).map_err(|e| ProtocolError::Schema {
        schema: name.to_string(),
        detail: format!("{} at {}", e, e.instance_path()),
    })
}

struct Validators {
    envelope: jsonschema::Validator,
    payloads: HashMap<&'static str, jsonschema::Validator>,
}

/// Schema sources, embedded at compile time from `contracts/protocol/`.
pub const SCHEMAS: &[(&str, &str)] = &[
    ("pair.hello", include_str!("../../../contracts/protocol/messages/pair.hello.schema.json")),
    ("session.start", include_str!("../../../contracts/protocol/messages/session.start.schema.json")),
    ("session.end", include_str!("../../../contracts/protocol/messages/session.end.schema.json")),
    ("answer.start", include_str!("../../../contracts/protocol/messages/answer.start.schema.json")),
    ("answer.delta", include_str!("../../../contracts/protocol/messages/answer.delta.schema.json")),
    ("answer.end", include_str!("../../../contracts/protocol/messages/answer.end.schema.json")),
    ("transcript.partial", include_str!("../../../contracts/protocol/messages/transcript.partial.schema.json")),
    ("command", include_str!("../../../contracts/protocol/messages/command.schema.json")),
    ("heartbeat", include_str!("../../../contracts/protocol/messages/heartbeat.schema.json")),
    ("session.summary", include_str!("../../../contracts/protocol/messages/session.summary.schema.json")),
    ("memory.snapshot", include_str!("../../../contracts/protocol/messages/memory.snapshot.schema.json")),
    ("agent.status", include_str!("../../../contracts/protocol/messages/agent.status.schema.json")),
    ("agent.log", include_str!("../../../contracts/protocol/messages/agent.log.schema.json")),
];
pub const ENVELOPE_SCHEMA: &str = include_str!("../../../contracts/protocol/envelope.schema.json");
pub const QR_SCHEMA: &str = include_str!("../../../contracts/protocol/qr.schema.json");

pub(crate) fn compile(src: &str) -> jsonschema::Validator {
    let schema: Value = serde_json::from_str(src).expect("embedded schema is JSON");
    jsonschema::validator_for(&schema).expect("embedded schema compiles")
}

fn validators() -> &'static Validators {
    static V: OnceLock<Validators> = OnceLock::new();
    V.get_or_init(|| Validators {
        envelope: compile(ENVELOPE_SCHEMA),
        payloads: SCHEMAS.iter().map(|(k, src)| (*k, compile(src))).collect(),
    })
}

// ---------- pairing QR ----------

/// Contents of the pairing QR code (`contracts/crypto/README.md`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct QrPayload {
    pub v: u8,
    /// LAN URL(s) first, relay URL last.
    pub urls: Vec<String>,
    pub pc_pub: String,
    pub token: String,
    /// Unix seconds.
    pub exp: u64,
}

impl QrPayload {
    pub fn from_value(value: &Value) -> Result<Self, ProtocolError> {
        static V: OnceLock<jsonschema::Validator> = OnceLock::new();
        check(V.get_or_init(|| compile(QR_SCHEMA)), "qr", value)?;
        Ok(serde_json::from_value(value.clone())?)
    }
}
