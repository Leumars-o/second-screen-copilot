//! `ssc-pipeline`: the PC agent's data flow (Build Sheet §7 W2-INTEGRATE, spec "Data flow").
//!
//! 1. **Capture**: audio sources → STT ([`Parts::audio`], [`Parts::stt`]); screen polling
//!    ([`Parts::screen`]).
//! 2. **Diff**: `ssc_screen::diff` against the previous snapshot.
//! 3. **Trigger**: `ssc_trigger::TriggerEngine` (hotkey, question, screen change, timer).
//! 4. **Context**: `ssc_context::retrieve_and_assemble` over the memory store.
//! 5. **Route**: `ssc_router::AnswerRouter` (failover, strong tier, redaction, local-only).
//! 6. **Push**: `answer.start` / `answer.delta` / `answer.end` over [`Parts::transport`].
//! 7. (phone renders)
//! 8. **Commands**: regenerate, shorter, number, follow-up, expand all, pin, feedback, memory
//!    edits, contact confirmation ([`pipeline`] command loop).
//! 9. **Store**: answers, opt-in transcripts, session items, pending contact suggestions.
//! 10. **Follow-up**: `summary.v1` + `followup.v1` on the post-call model → `session.summary`.
//!
//! Every stage records its [`Stage`](ssc_core::timing::Stage) in [`Parts::timing`].
//!
//! [`system`] builds the real components from `Config` + keychain; tests build [`Parts`] from
//! `ssc_core::mock`.

pub mod clock;
pub mod decision;
pub mod limits;
pub mod loopback_phone;
pub mod pipeline;
pub mod postcall;
pub mod replay;
pub mod stats;
pub mod stt_select;
pub mod system;
pub mod timing_log;
pub mod transport;

pub use clock::TokioClock;
pub use decision::RoutedDecisions;
pub use pipeline::{Parts, Pipeline, Settings};
pub use stats::{StageStat, stage_stats};
pub use stt_select::{SttChoice, choose_stt};
