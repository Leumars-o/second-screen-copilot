//! Session lifecycle: start, transcript (opt-in), answers, session-end persistence, and contact
//! suggestions that wait in `pending_contact_suggestions` until the user confirms them.

use crate::db::DbResult;
use crate::items::{self, SessionOutcome};
use crate::store::{SqliteMemory, parse_uuid};
use rusqlite::{Connection, OptionalExtension, Transaction, params};
use ssc_core::protocol::{
    AnswerEnd, AnswerFormat, ContactSuggestion, Intent, Layer, MemoryItem, Mode, Rating, SessionSummary, Speaker,
    Trigger,
};
use ssc_core::types::TranscriptSegment;
use ssc_core::{Result, SscError};
use std::sync::atomic::Ordering;
use uuid::Uuid;

/// A contact suggestion waiting for the user's one-tap confirmation.
#[derive(Debug, Clone, PartialEq)]
pub struct PendingSuggestion {
    pub session_id: Uuid,
    pub suggestion: ContactSuggestion,
    pub created_at: u64,
}

/// An answer to persist in the session history.
#[derive(Debug, Clone, PartialEq)]
pub struct AnswerRecord {
    pub id: Uuid,
    pub session_id: Uuid,
    pub trigger: Trigger,
    pub format: AnswerFormat,
    pub intent: Intent,
    pub model: String,
    pub question: String,
    /// The final `answer.end` payload, stored as JSON.
    pub end: AnswerEnd,
}

fn enum_str<T: serde::Serialize>(v: &T) -> String {
    serde_json::to_value(v).ok().and_then(|v| v.as_str().map(str::to_string)).unwrap_or_default()
}

pub(crate) fn replace_items(tx: &Transaction<'_>, session: Uuid, o: &SessionOutcome, now: i64) -> Result<()> {
    let sid = session.to_string();
    tx.execute("DELETE FROM session_items WHERE session_id = ?1", [&sid]).db()?;
    let mut ins = tx
        .prepare("INSERT INTO session_items (id, session_id, kind, body, created_at) VALUES (?1, ?2, ?3, ?4, ?5)")
        .db()?;
    let mut put = |kind: &str, body: &str| -> Result<()> {
        ins.execute(params![Uuid::new_v4().to_string(), sid, kind, body, now]).db()?;
        Ok(())
    };
    if !o.summary.is_empty() {
        put("summary", &o.summary)?;
    }
    for a in &o.action_items {
        put("action_item", a)?;
    }
    for (q, a) in &o.key_qa {
        put("qa", &serde_json::json!({ "q": q, "a": a }).to_string())?;
    }
    if !o.followup_email.is_empty() {
        put("followup_email", &o.followup_email)?;
    }
    Ok(())
}

impl SqliteMemory {
    pub fn start_session(&self, id: Uuid, profile_id: Option<&str>, mode: Mode) -> Result<()> {
        self.conn()
            .execute(
                "INSERT INTO sessions (id, profile_id, mode, started_at) VALUES (?1, ?2, ?3, ?4)",
                params![id.to_string(), profile_id, enum_str(&mode), self.now()],
            )
            .db()?;
        Ok(())
    }

    /// Stores a final transcript segment if (and only if) the user turned "keep transcripts" on.
    /// Returns whether it was stored. Partial segments are never stored.
    pub fn record_transcript(&self, session_id: Uuid, seg: &TranscriptSegment) -> Result<bool> {
        if !self.keep_transcripts.load(Ordering::SeqCst) || !seg.is_final || seg.text.trim().is_empty() {
            return Ok(false);
        }
        self.conn()
            .execute(
                "INSERT INTO transcripts (session_id, speaker, text, t0, t1) VALUES (?1, ?2, ?3, ?4, ?5)",
                params![session_id.to_string(), enum_str(&seg.speaker), seg.text, seg.t0_ms as i64, seg.t1_ms as i64],
            )
            .db()?;
        Ok(true)
    }

    pub fn transcript(&self, session_id: Uuid) -> Result<Vec<TranscriptSegment>> {
        let conn = self.conn();
        let mut st =
            conn.prepare("SELECT speaker, text, t0, t1 FROM transcripts WHERE session_id = ?1 ORDER BY t0, id").db()?;
        let rows = st
            .query_map([session_id.to_string()], |r| {
                Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?, r.get::<_, i64>(2)?, r.get::<_, i64>(3)?))
            })
            .db()?
            .collect::<std::result::Result<Vec<_>, _>>()
            .db()?;
        Ok(rows
            .into_iter()
            .map(|(sp, text, t0, t1)| TranscriptSegment {
                speaker: if sp == "me" { Speaker::Me } else { Speaker::Them },
                text,
                is_final: true,
                t0_ms: t0 as u64,
                t1_ms: t1 as u64,
                ends_utterance: false,
            })
            .collect())
    }

    pub fn record_answer(&self, a: &AnswerRecord) -> Result<()> {
        let body = serde_json::to_string(&a.end).map_err(|e| SscError::Invalid(e.to_string()))?;
        self.conn()
            .execute(
                "INSERT INTO answers (id, session_id, trigger, format, intent, model, question, body, created_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
                params![
                    a.id.to_string(),
                    a.session_id.to_string(),
                    enum_str(&a.trigger),
                    enum_str(&a.format),
                    enum_str(&a.intent),
                    a.model,
                    a.question,
                    body,
                    self.now()
                ],
            )
            .db()?;
        Ok(())
    }

    pub fn pin_answer(&self, answer_id: Uuid, pinned: bool) -> Result<()> {
        let n = self
            .conn()
            .execute("UPDATE answers SET pinned = ?2 WHERE id = ?1", params![answer_id.to_string(), pinned])
            .db()?;
        if n == 0 { Err(SscError::NotFound) } else { Ok(()) }
    }

    pub fn rate_answer(&self, answer_id: Uuid, rating: Rating) -> Result<()> {
        self.conn()
            .execute(
                "INSERT INTO answer_feedback (answer_id, rating, created_at) VALUES (?1, ?2, ?3)
                 ON CONFLICT(answer_id) DO UPDATE SET rating = ?2, created_at = ?3",
                params![answer_id.to_string(), enum_str(&rating), self.now()],
            )
            .map_err(|e| match e {
                rusqlite::Error::SqliteFailure(f, _) if f.code == rusqlite::ErrorCode::ConstraintViolation => {
                    SscError::NotFound
                }
                e => crate::db::db_err(e),
            })?;
        Ok(())
    }

    /// Session end: stores summary, action items, key Q&A and the follow-up draft, parks contact
    /// suggestions as pending (never saved as contacts here), and returns the `session.summary`
    /// payload for the phone.
    pub fn end_session(
        &self,
        session_id: Uuid,
        outcome: &SessionOutcome,
        suggestions: &[ContactSuggestion],
    ) -> Result<SessionSummary> {
        let now = self.now();
        let sid = session_id.to_string();
        let mut conn = self.conn();
        let tx = conn.transaction().db()?;
        let n = tx.execute("UPDATE sessions SET ended_at = ?2 WHERE id = ?1", params![sid, now]).db()?;
        if n == 0 {
            return Err(SscError::NotFound);
        }
        replace_items(&tx, session_id, outcome, now)?;
        {
            let mut ins = tx
                .prepare(
                    "INSERT OR REPLACE INTO pending_contact_suggestions (id, session_id, name, company, notes, created_at)
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                )
                .db()?;
            for s in suggestions.iter().filter(|s| !s.name.trim().is_empty()) {
                ins.execute(params![
                    s.id.to_string(),
                    sid,
                    s.name.trim(),
                    s.company.as_deref().map(str::trim).filter(|c| !c.is_empty()),
                    s.notes.as_deref().unwrap_or(""),
                    now
                ])
                .db()?;
            }
        }
        tx.commit().db()?;
        Ok(SessionSummary {
            session_id,
            summary: outcome.summary.clone(),
            action_items: outcome.action_items.clone(),
            followup_email: outcome.followup_email.clone(),
            contact_suggestions: suggestions.iter().filter(|s| !s.name.trim().is_empty()).cloned().collect(),
        })
    }

    pub fn pending_contact_suggestions(&self) -> Result<Vec<PendingSuggestion>> {
        let conn = self.conn();
        let mut st = conn
            .prepare(
                "SELECT id, session_id, name, company, notes, created_at FROM pending_contact_suggestions
                 ORDER BY created_at, name",
            )
            .db()?;
        let rows = st
            .query_map([], |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, String>(1)?,
                    r.get::<_, String>(2)?,
                    r.get::<_, Option<String>>(3)?,
                    r.get::<_, String>(4)?,
                    r.get::<_, i64>(5)?,
                ))
            })
            .db()?
            .collect::<std::result::Result<Vec<_>, _>>()
            .db()?;
        rows.into_iter()
            .map(|(id, sid, name, company, notes, t)| {
                Ok(PendingSuggestion {
                    session_id: parse_uuid(&sid)?,
                    suggestion: ContactSuggestion {
                        id: parse_uuid(&id)?,
                        name,
                        company,
                        notes: (!notes.is_empty()).then_some(notes),
                    },
                    created_at: t as u64,
                })
            })
            .collect()
    }

    /// The `contact.confirm` command. `accept = false` discards the suggestion. `accept = true`
    /// writes the contact (merging notes into an existing contact with the same name and account)
    /// and its account if a company was given. Returns the saved contact.
    pub fn confirm_contact(&self, suggestion_id: Uuid, accept: bool) -> Result<Option<MemoryItem>> {
        let now = self.now();
        let mut conn = self.conn();
        let tx = conn.transaction().db()?;
        let row: Option<(String, Option<String>, String)> = tx
            .query_row(
                "SELECT name, company, notes FROM pending_contact_suggestions WHERE id = ?1",
                [suggestion_id.to_string()],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .optional()
            .db()?;
        let (name, company, notes) = row.ok_or(SscError::NotFound)?;
        tx.execute("DELETE FROM pending_contact_suggestions WHERE id = ?1", [suggestion_id.to_string()]).db()?;
        if !accept {
            tx.commit().db()?;
            return Ok(None);
        }
        let account_id = match company {
            Some(c) => Some(find_or_create_account(&tx, &c, now)?),
            None => None,
        };
        let existing: Option<(String, String)> = tx
            .query_row(
                "SELECT id, notes FROM contacts WHERE lower(name) = lower(?1) AND account_id IS ?2",
                params![name, account_id],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()
            .db()?;
        let (id, body) = match existing {
            Some((id, old)) => {
                let body = match (old.trim().is_empty(), notes.trim().is_empty()) {
                    (_, true) => old,
                    (true, false) => notes,
                    (false, false) => format!("{old}\n{notes}"),
                };
                tx.execute("UPDATE contacts SET notes = ?2, updated_at = ?3 WHERE id = ?1", params![id, body, now])
                    .db()?;
                (id, body)
            }
            None => {
                let id = Uuid::new_v4().to_string();
                tx.execute(
                    "INSERT INTO contacts (id, account_id, name, notes, updated_at) VALUES (?1, ?2, ?3, ?4, ?5)",
                    params![id, account_id, name, notes, now],
                )
                .db()?;
                (id, notes)
            }
        };
        tx.commit().db()?;
        Ok(Some(MemoryItem { id: parse_uuid(&id)?, layer: Layer::Contact, title: name, body, updated_at: now as u64 }))
    }

    /// The Session-layer item: title "<profile> · <date> UTC", body from `session_items`.
    pub(crate) fn session_item(&self, conn: &Connection, id: &str) -> Result<MemoryItem> {
        let (profile, started, ended): (Option<String>, i64, Option<i64>) = conn
            .query_row(
                "SELECT p.name, s.started_at, s.ended_at FROM sessions s LEFT JOIN profiles p ON p.id = s.profile_id
                 WHERE s.id = ?1",
                [id],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .optional()
            .db()?
            .ok_or(SscError::NotFound)?;
        let mut st = conn
            .prepare(
                "SELECT kind, body, created_at FROM session_items WHERE session_id = ?1 ORDER BY created_at, rowid",
            )
            .db()?;
        let rows = st
            .query_map([id], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?, r.get::<_, i64>(2)?)))
            .db()?
            .collect::<std::result::Result<Vec<_>, _>>()
            .db()?;
        let mut o = SessionOutcome::default();
        let mut updated = ended.unwrap_or(started);
        for (kind, body, t) in rows {
            updated = updated.max(t);
            match kind.as_str() {
                "summary" => o.summary = body,
                "action_item" => o.action_items.push(body),
                "qa" => {
                    let v: serde_json::Value = serde_json::from_str(&body).unwrap_or_default();
                    let s = |k: &str| v.get(k).and_then(|x| x.as_str()).unwrap_or_default().to_string();
                    o.key_qa.push((s("q"), s("a")));
                }
                "followup_email" => o.followup_email = body,
                _ => {}
            }
        }
        Ok(MemoryItem {
            id: parse_uuid(id)?,
            layer: Layer::Session,
            title: format!("{} · {} UTC", profile.as_deref().unwrap_or("Session"), items::format_utc(started as u64)),
            body: items::session_body(&o),
            updated_at: updated as u64,
        })
    }
}

fn find_or_create_account(tx: &Transaction<'_>, name: &str, now: i64) -> Result<String> {
    let found: Option<String> =
        tx.query_row("SELECT id FROM accounts WHERE lower(name) = lower(?1)", [name], |r| r.get(0)).optional().db()?;
    if let Some(id) = found {
        return Ok(id);
    }
    let id = Uuid::new_v4().to_string();
    tx.execute("INSERT INTO accounts (id, name, notes, updated_at) VALUES (?1, ?2, '', ?3)", params![id, name, now])
        .db()?;
    Ok(id)
}
