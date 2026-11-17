//! `answer.delta` coalescing: one frame per 50 ms or per 32 characters, whichever comes first.
//! The first delta of each answer goes out at once (time to first cue matters more than frame
//! count); only the ones after it are coalesced. Any other message flushes the pending delta
//! first, so ordering is preserved.

use ssc_core::protocol::{AnswerDelta, Block, Message};
use std::collections::VecDeque;
use std::time::Duration;
use tokio::time::Instant;
use uuid::Uuid;

/// `answer.delta.text` maxLength in the schema.
const MAX_TEXT_CHARS: usize = 4096;
/// Answers remembered as "first delta already sent" (answers are sequential; a few suffice).
const SEEN_CAP: usize = 64;

pub(crate) type Outgoing = (Option<Uuid>, Message);

struct Pending {
    session_id: Option<Uuid>,
    answer_id: Uuid,
    block: Block,
    text: String,
    chars: usize,
    deadline: Instant,
}

impl Pending {
    fn into_out(self) -> Outgoing {
        (
            self.session_id,
            Message::AnswerDelta(AnswerDelta { answer_id: self.answer_id, block: self.block, text: self.text }),
        )
    }
}

pub(crate) struct Batcher {
    window: Duration,
    max_chars: usize,
    cur: Option<Pending>,
    /// Answer ids whose first delta has gone out (oldest first).
    seen: VecDeque<Uuid>,
}

impl Batcher {
    pub(crate) fn new(window: Duration, max_chars: usize) -> Self {
        Self { window, max_chars, cur: None, seen: VecDeque::new() }
    }

    /// When the pending delta must go out.
    pub(crate) fn deadline(&self) -> Option<Instant> {
        self.cur.as_ref().map(|p| p.deadline)
    }

    pub(crate) fn flush(&mut self) -> Option<Outgoing> {
        self.cur.take().map(Pending::into_out)
    }

    /// Queue a message; returns what must be sent now, in order.
    pub(crate) fn push(&mut self, session_id: Option<Uuid>, msg: Message, now: Instant) -> Vec<Outgoing> {
        let mut out = Vec::new();
        let Message::AnswerDelta(d) = msg else {
            out.extend(self.flush());
            out.push((session_id, msg));
            return out;
        };
        if !self.seen.contains(&d.answer_id) {
            // First delta of this answer: no batching delay.
            if self.seen.len() == SEEN_CAP {
                self.seen.pop_front();
            }
            self.seen.push_back(d.answer_id);
            out.extend(self.flush());
            out.push((session_id, Message::AnswerDelta(d)));
            return out;
        }
        let n = d.text.chars().count();
        let joins = self.cur.as_ref().is_some_and(|p| {
            p.session_id == session_id
                && p.answer_id == d.answer_id
                && p.block == d.block
                && p.chars + n <= MAX_TEXT_CHARS
        });
        if joins {
            let p = self.cur.as_mut().expect("checked");
            p.text.push_str(&d.text);
            p.chars += n;
        } else {
            out.extend(self.flush());
            self.cur = Some(Pending {
                session_id,
                answer_id: d.answer_id,
                block: d.block,
                text: d.text,
                chars: n,
                deadline: now + self.window,
            });
        }
        if self.cur.as_ref().is_some_and(|p| p.chars >= self.max_chars) {
            out.extend(self.flush());
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ssc_core::protocol::Heartbeat;

    fn delta(id: Uuid, block: Block, t: &str) -> Message {
        Message::AnswerDelta(AnswerDelta { answer_id: id, block, text: t.into() })
    }
    fn text(o: &Outgoing) -> &str {
        match &o.1 {
            Message::AnswerDelta(d) => &d.text,
            _ => panic!("not a delta"),
        }
    }

    /// Push the first delta of `id` (sent at once) so later ones are batched.
    fn prime(b: &mut Batcher, id: Uuid) {
        let out = b.push(None, delta(id, Block::Cue, "first"), Instant::now());
        assert_eq!(out.len(), 1);
        assert_eq!(text(&out[0]), "first");
    }

    #[test]
    fn first_delta_of_each_answer_is_immediate() {
        let mut b = Batcher::new(Duration::from_millis(50), 32);
        let (a1, a2) = (Uuid::new_v4(), Uuid::new_v4());
        let now = Instant::now();
        let out = b.push(None, delta(a1, Block::Cue, "a"), now);
        assert_eq!(out.len(), 1, "first delta is not held");
        assert!(b.deadline().is_none());
        // The second one is batched.
        assert!(b.push(None, delta(a1, Block::Cue, "b"), now).is_empty());
        assert!(b.deadline().is_some());
        // A new answer's first delta flushes the pending one and goes out right behind it.
        let out = b.push(None, delta(a2, Block::Cue, "c"), now);
        assert_eq!(out.iter().map(text).collect::<Vec<_>>(), vec!["b", "c"]);
        assert!(b.deadline().is_none());
        // A second block of a known answer is not "first" again.
        assert!(b.push(None, delta(a2, Block::Detail, "d"), now).is_empty());
    }

    #[test]
    fn coalesces_until_32_chars() {
        let mut b = Batcher::new(Duration::from_millis(50), 32);
        let id = Uuid::new_v4();
        prime(&mut b, id);
        let t0 = Instant::now();
        for _ in 0..3 {
            assert!(b.push(None, delta(id, Block::Cue, "0123456789"), t0).is_empty());
        }
        assert_eq!(b.deadline(), Some(t0 + Duration::from_millis(50)));
        let out = b.push(None, delta(id, Block::Cue, "abc"), t0);
        assert_eq!(out.len(), 1);
        assert_eq!(text(&out[0]), "012345678901234567890123456789abc");
        assert!(b.deadline().is_none());
    }

    #[test]
    fn counts_chars_not_bytes() {
        let mut b = Batcher::new(Duration::from_millis(50), 32);
        let id = Uuid::new_v4();
        prime(&mut b, id);
        // 31 two-byte chars: not yet 32 chars although 62 bytes.
        assert!(b.push(None, delta(id, Block::Cue, &"é".repeat(31)), Instant::now()).is_empty());
        assert_eq!(b.push(None, delta(id, Block::Cue, "é"), Instant::now()).len(), 1);
    }

    #[test]
    fn other_messages_and_new_answers_flush_first() {
        let mut b = Batcher::new(Duration::from_millis(50), 32);
        let (a1, a2) = (Uuid::new_v4(), Uuid::new_v4());
        let now = Instant::now();
        prime(&mut b, a1);
        prime(&mut b, a2);
        assert!(b.push(None, delta(a1, Block::Cue, "x"), now).is_empty());
        let out = b.push(None, delta(a1, Block::Detail, "y"), now);
        assert_eq!(text(&out[0]), "x");
        let out = b.push(None, delta(a2, Block::Detail, "z"), now);
        assert_eq!(text(&out[0]), "y");
        let out = b.push(None, Message::Heartbeat(Heartbeat::default()), now);
        assert_eq!(out.len(), 2);
        assert_eq!(text(&out[0]), "z");
        assert!(matches!(out[1].1, Message::Heartbeat(_)));
        // Different session id does not merge either.
        assert!(b.push(None, delta(a1, Block::Cue, "s"), now).is_empty());
        assert_eq!(b.push(Some(Uuid::new_v4()), delta(a1, Block::Cue, "t"), now).len(), 1);
    }

    #[test]
    fn never_exceeds_schema_max_length() {
        let mut b = Batcher::new(Duration::from_millis(50), 100_000);
        let id = Uuid::new_v4();
        let now = Instant::now();
        prime(&mut b, id);
        assert!(b.push(None, delta(id, Block::Cue, &"a".repeat(4000)), now).is_empty());
        let out = b.push(None, delta(id, Block::Cue, &"b".repeat(200)), now);
        assert_eq!(text(&out[0]).len(), 4000);
        assert_eq!(text(&b.flush().unwrap()).len(), 200);
    }
}
