//! CRUD, delete (rows + vectors), ingest of every doc type, session persistence, contact
//! confirmation, transcript opt-in and the no-media schema rule.

use ssc_core::SscError;
use ssc_core::mock::{MockClock, MockEmbedder};
use ssc_core::protocol::{
    AnswerEnd, AnswerFormat, ContactSuggestion, Intent, Layer, MemoryEdit, Mode, Rating, Speaker, Trigger,
};
use ssc_core::traits::MemoryStore;
use ssc_core::types::{Profile, TranscriptSegment};
use ssc_memory::{ABOUT_ME_ID, AnswerRecord, SessionOutcome, SqliteMemory, profile_uuid};
use std::io::Write;
use std::sync::Arc;
use uuid::Uuid;

fn store() -> (SqliteMemory, MockClock) {
    let clock = MockClock::new(1_731_650_000_000);
    let m =
        SqliteMemory::open_in_memory_with_clock(Arc::new(MockEmbedder::default()), Arc::new(clock.clone())).unwrap();
    (m, clock)
}

fn profile(m: &SqliteMemory, slug: &str) {
    m.put_profile(&Profile { id: slug.into(), name: slug.into(), instructions: String::new() }).unwrap();
}

/// A file-backed store plus a raw connection for row counts.
struct FileStore {
    mem: SqliteMemory,
    path: std::path::PathBuf,
    dir: std::path::PathBuf,
}

impl FileStore {
    fn new() -> Self {
        let dir = std::env::temp_dir().join(format!("ssc-mem-test-{}", Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("memory.sqlite");
        let mem = SqliteMemory::open(&path, Arc::new(MockEmbedder::default())).unwrap();
        Self { mem, path, dir }
    }
    fn count(&self, sql: &str) -> i64 {
        // A second connection (sqlite-vec is auto-registered for every connection in this process).
        let c = rusqlite::Connection::open(&self.path).unwrap();
        c.query_row(sql, [], |r| r.get(0)).unwrap()
    }
}

impl Drop for FileStore {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

/// Smallest valid single-page PDF with one line of Helvetica text; xref offsets computed here.
fn make_pdf(text: &str) -> Vec<u8> {
    let content = format!("BT /F1 12 Tf 72 720 Td ({text}) Tj ET");
    let objs = [
        "<< /Type /Catalog /Pages 2 0 R >>".to_string(),
        "<< /Type /Pages /Kids [3 0 R] /Count 1 >>".to_string(),
        "<< /Type /Page /Parent 2 0 R /MediaBox [0 0 612 792] /Contents 4 0 R /Resources << /Font << /F1 5 0 R >> >> >>"
            .to_string(),
        format!("<< /Length {} >>\nstream\n{content}\nendstream", content.len()),
        "<< /Type /Font /Subtype /Type1 /BaseFont /Helvetica /Encoding /WinAnsiEncoding >>".to_string(),
    ];
    let mut out = b"%PDF-1.4\n".to_vec();
    let mut offsets = vec![];
    for (i, o) in objs.iter().enumerate() {
        offsets.push(out.len());
        out.extend_from_slice(format!("{} 0 obj\n{o}\nendobj\n", i + 1).as_bytes());
    }
    let xref = out.len();
    out.extend_from_slice(format!("xref\n0 {}\n0000000000 65535 f \n", objs.len() + 1).as_bytes());
    for off in offsets {
        out.extend_from_slice(format!("{off:010} 00000 n \n").as_bytes());
    }
    out.extend_from_slice(
        format!("trailer\n<< /Size {} /Root 1 0 R >>\nstartxref\n{xref}\n%%EOF\n", objs.len() + 1).as_bytes(),
    );
    out
}

fn make_docx(paragraphs: &[&str]) -> Vec<u8> {
    let body: String =
        paragraphs.iter().map(|p| format!("<w:p><w:r><w:t xml:space=\"preserve\">{p}</w:t></w:r></w:p>")).collect();
    let xml = format!(
        "<?xml version=\"1.0\" encoding=\"UTF-8\" standalone=\"yes\"?>\
         <w:document xmlns:w=\"http://schemas.openxmlformats.org/wordprocessingml/2006/main\"><w:body>{body}</w:body></w:document>"
    );
    let mut buf = std::io::Cursor::new(Vec::new());
    {
        let mut z = zip::ZipWriter::new(&mut buf);
        let opts = zip::write::SimpleFileOptions::default().compression_method(zip::CompressionMethod::Deflated);
        z.start_file("[Content_Types].xml", opts).unwrap();
        z.write_all(b"<Types/>").unwrap();
        z.start_file("word/document.xml", opts).unwrap();
        z.write_all(xml.as_bytes()).unwrap();
        z.finish().unwrap();
    }
    buf.into_inner()
}

#[test]
fn about_me_crud() {
    let (m, clock) = store();
    assert!(m.list(Some(Layer::AboutMe)).unwrap().is_empty());
    let item = m
        .upsert(MemoryEdit {
            id: None,
            layer: Layer::AboutMe,
            title: "Ada Lovelace".into(),
            body: "Role: AE\nCompany: Acme\nProduct: Rockets\nTone: direct".into(),
        })
        .unwrap();
    assert_eq!(item.id, ABOUT_ME_ID);
    assert_eq!(m.about_me().unwrap().unwrap().company, "Acme");
    clock.advance(1000);
    // Editing through the listed body round-trips.
    let listed = m.list(Some(Layer::AboutMe)).unwrap().pop().unwrap();
    let edited = m
        .upsert(MemoryEdit {
            id: Some(listed.id),
            layer: Layer::AboutMe,
            title: listed.title,
            body: listed.body.replace("direct", "warm"),
        })
        .unwrap();
    assert_eq!(edited.updated_at, 1_731_650_001_000);
    let a = m.about_me().unwrap().unwrap();
    assert_eq!((a.name.as_str(), a.tone.as_str(), a.product.as_str()), ("Ada Lovelace", "warm", "Rockets"));
    assert!(matches!(
        m.upsert(MemoryEdit {
            id: Some(Uuid::new_v4()),
            layer: Layer::AboutMe,
            title: "x".into(),
            body: String::new()
        }),
        Err(SscError::NotFound)
    ));
    m.delete(ABOUT_ME_ID).unwrap();
    assert!(m.about_me().unwrap().is_none());
    assert!(matches!(m.delete(ABOUT_ME_ID), Err(SscError::NotFound)));
}

#[test]
fn profile_contact_account_crud() {
    let (m, _) = store();
    let p = m
        .upsert(MemoryEdit {
            id: None,
            layer: Layer::Profile,
            title: "Sales: Product X".into(),
            body: "Be brief".into(),
        })
        .unwrap();
    assert_eq!(p.id, profile_uuid("sales-product-x"));
    let p2 = m
        .upsert(MemoryEdit { id: None, layer: Layer::Profile, title: "Sales: Product X".into(), body: String::new() })
        .unwrap();
    assert_eq!(p2.id, profile_uuid("sales-product-x-2"), "slug collision gets a suffix");
    m.upsert(MemoryEdit { id: Some(p.id), layer: Layer::Profile, title: "Sales X".into(), body: "Be briefer".into() })
        .unwrap();
    assert_eq!(m.profile("sales-product-x").unwrap().unwrap().instructions, "Be briefer");

    let acct = m
        .upsert(MemoryEdit { id: None, layer: Layer::Account, title: "Globex".into(), body: "Q3 renewal".into() })
        .unwrap();
    let c = m
        .upsert(MemoryEdit { id: None, layer: Layer::Contact, title: "Hank".into(), body: "Wants SSO".into() })
        .unwrap();
    let c = m
        .upsert(MemoryEdit {
            id: Some(c.id),
            layer: Layer::Contact,
            title: "Hank Scorpio".into(),
            body: "SSO + SCIM".into(),
        })
        .unwrap();
    let contacts = m.list(Some(Layer::Contact)).unwrap();
    assert_eq!(contacts.len(), 1);
    assert_eq!((contacts[0].title.as_str(), contacts[0].body.as_str()), ("Hank Scorpio", "SSO + SCIM"));
    // Wrong layer for an existing id is not silently moved.
    assert!(matches!(
        m.upsert(MemoryEdit { id: Some(c.id), layer: Layer::Account, title: "x".into(), body: String::new() }),
        Err(SscError::NotFound)
    ));
    assert!(matches!(
        m.upsert(MemoryEdit { id: None, layer: Layer::Contact, title: "  ".into(), body: String::new() }),
        Err(SscError::Invalid(_))
    ));
    assert_eq!(m.list(None).unwrap().len(), 4);

    for id in [c.id, acct.id, p.id, p2.id] {
        m.delete(id).unwrap();
        assert!(matches!(m.delete(id), Err(SscError::NotFound)));
    }
    assert!(m.list(None).unwrap().is_empty());
}

#[test]
fn ingest_every_type_and_search() {
    let (m, _) = store();
    profile(&m, "sales");
    let pdf = m.ingest_doc("sales", "pricing.pdf", &make_pdf("Enterprise plan costs 45000 dollars per year")).unwrap();
    let docx = m
        .ingest_doc(
            "sales",
            "security.docx",
            &make_docx(&["We support SAML single sign-on.", "Data is encrypted at rest."]),
        )
        .unwrap();
    let md = m.ingest_doc("sales", "faq.md", b"# FAQ\nSupport hours are nine to five on weekdays.").unwrap();
    let txt = m.ingest_doc("sales", "notes.txt", b"Onboarding takes two weeks with a named engineer.").unwrap();
    assert_eq!(pdf.mime, "application/pdf");
    for d in [&pdf, &docx, &md, &txt] {
        assert_eq!(d.chunks, 1, "{}", d.name);
    }
    assert_eq!(m.indexed_chunks(), 4);

    let hits = m.search("how much does the enterprise plan cost", "sales", 5).unwrap();
    assert_eq!(hits[0].doc_name, "pricing.pdf");
    assert!(hits[0].text.contains("45000"), "PDF text extracted: {:?}", hits[0].text);
    assert_eq!(m.search("SAML sign-on", "sales", 1).unwrap()[0].doc_name, "security.docx");
    assert!(m.search("SAML sign-on", "sales", 1).unwrap()[0].text.contains("encrypted at rest"));
    assert_eq!(m.search("support hours", "sales", 1).unwrap()[0].doc_name, "faq.md");
    assert_eq!(m.search("onboarding engineer", "sales", 1).unwrap()[0].doc_name, "notes.txt");
    // Scores descend; nothing unrelated is returned.
    let hits = m.search("enterprise plan support", "sales", 5).unwrap();
    assert!(hits.windows(2).all(|w| w[0].score >= w[1].score));
    assert!(m.search("zebra xylophone", "sales", 5).unwrap().is_empty(), "no overlap → empty (not-in-docs)");
    assert!(m.search("enterprise", "other-profile", 5).unwrap().is_empty());
    assert!(m.search("", "sales", 5).unwrap().is_empty());

    // Identical re-upload is deduplicated.
    let again = m.ingest_doc("sales", "notes-copy.txt", b"Onboarding takes two weeks with a named engineer.").unwrap();
    assert_eq!(again.id, txt.id);
    assert_eq!(m.docs("sales").unwrap().len(), 4);

    assert!(matches!(m.ingest_doc("sales", "x.exe", b"MZ"), Err(SscError::Invalid(_))));
    assert!(matches!(m.ingest_doc("nope", "x.txt", b"hello"), Err(SscError::NotFound)));
}

#[test]
fn long_doc_is_chunked_and_searchable() {
    let (m, _) = store();
    profile(&m, "p");
    let text: String = (0..200).map(|i| format!("Paragraph {i} covers widget number w{i} in detail. ")).collect();
    let d = m.ingest_doc("p", "big.txt", text.as_bytes()).unwrap();
    assert!(d.chunks >= 4, "{} chunks", d.chunks);
    let hits = m.search("widget w137", "p", 3).unwrap();
    assert!(hits[0].text.contains("w137"));
}

#[test]
fn delete_removes_rows_and_vectors() {
    let fs = FileStore::new();
    let m = &fs.mem;
    profile(m, "a");
    profile(m, "b");
    let d1 = m.ingest_doc("a", "one.txt", "alpha one. ".repeat(400).as_bytes()).unwrap();
    m.ingest_doc("a", "two.txt", b"beta two").unwrap();
    m.ingest_doc("b", "three.txt", b"gamma three").unwrap();
    let total = d1.chunks as i64 + 2;
    assert!(d1.chunks > 1);
    assert_eq!(fs.count("SELECT COUNT(*) FROM chunks"), total);
    assert_eq!(fs.count("SELECT COUNT(*) FROM vec_chunks"), total);

    // Delete one doc through the MemoryStore trait.
    m.delete(d1.id).unwrap();
    assert_eq!(fs.count("SELECT COUNT(*) FROM chunks"), 2);
    assert_eq!(fs.count("SELECT COUNT(*) FROM vec_chunks"), 2);
    assert_eq!(fs.count("SELECT COUNT(*) FROM docs"), 2);
    assert!(m.search("alpha one", "a", 5).unwrap().is_empty());
    assert_eq!(m.indexed_chunks(), 2);

    // Delete a whole profile: docs, chunks and vectors go with it.
    m.delete(profile_uuid("a")).unwrap();
    assert_eq!(fs.count("SELECT COUNT(*) FROM chunks"), 1);
    assert_eq!(fs.count("SELECT COUNT(*) FROM vec_chunks"), 1);
    assert_eq!(fs.count("SELECT COUNT(*) FROM docs"), 1);
    assert!(m.search("beta two", "a", 5).unwrap().is_empty());
    assert_eq!(m.search("gamma three", "b", 5).unwrap().len(), 1);

    // Index survives reopen (loaded from vec_chunks).
    let reopened = SqliteMemory::open(&fs.path, Arc::new(MockEmbedder::default())).unwrap();
    assert_eq!(reopened.indexed_chunks(), 1);
    assert_eq!(reopened.search("gamma", "b", 5).unwrap()[0].doc_name, "three.txt");
}

#[test]
fn session_end_persists_and_suggestions_wait_for_confirmation() {
    let fs = FileStore::new();
    let m = &fs.mem;
    profile(m, "sales");
    let sid = Uuid::new_v4();
    m.start_session(sid, Some("sales"), Mode::Cloud).unwrap();

    let outcome = SessionOutcome {
        summary: "Globex wants SSO.".into(),
        action_items: vec!["Send pricing".into()],
        key_qa: vec![("SAML?".into(), "Yes".into())],
        followup_email: "Subject: Thanks".into(),
    };
    let s1 = ContactSuggestion {
        id: Uuid::new_v4(),
        name: "Hank".into(),
        company: Some("Globex".into()),
        notes: Some("Wants SSO".into()),
    };
    let s2 = ContactSuggestion { id: Uuid::new_v4(), name: "Marge".into(), company: None, notes: None };
    let summary = m.end_session(sid, &outcome, &[s1.clone(), s2.clone()]).unwrap();
    assert_eq!(summary.contact_suggestions.len(), 2);
    assert_eq!(fs.count("SELECT COUNT(*) FROM session_items"), 4);

    // Nothing saved silently.
    assert_eq!(fs.count("SELECT COUNT(*) FROM contacts"), 0);
    assert_eq!(m.pending_contact_suggestions().unwrap().len(), 2);

    let saved = m.confirm_contact(s1.id, true).unwrap().unwrap();
    assert_eq!((saved.title.as_str(), saved.body.as_str()), ("Hank", "Wants SSO"));
    assert_eq!(fs.count("SELECT COUNT(*) FROM accounts WHERE name = 'Globex'"), 1);
    assert_eq!(m.confirm_contact(s2.id, false).unwrap(), None);
    assert_eq!(fs.count("SELECT COUNT(*) FROM contacts"), 1);
    assert!(m.pending_contact_suggestions().unwrap().is_empty());
    assert!(matches!(m.confirm_contact(s1.id, true), Err(SscError::NotFound)));

    // Same person again in a later call: notes merge into the existing contact.
    let sid2 = Uuid::new_v4();
    m.start_session(sid2, Some("sales"), Mode::Local).unwrap();
    let s3 = ContactSuggestion {
        id: Uuid::new_v4(),
        name: "hank".into(),
        company: Some("globex".into()),
        notes: Some("Asked for SCIM".into()),
    };
    m.end_session(sid2, &SessionOutcome::default(), std::slice::from_ref(&s3)).unwrap();
    let merged = m.confirm_contact(s3.id, true).unwrap().unwrap();
    assert_eq!(merged.id, saved.id);
    assert_eq!(merged.body, "Wants SSO\nAsked for SCIM");
    assert_eq!(fs.count("SELECT COUNT(*) FROM accounts"), 1);

    // Session layer item round-trips and can be edited.
    let items = m.list(Some(Layer::Session)).unwrap();
    let it = items.iter().find(|i| i.id == sid).unwrap();
    assert!(it.title.starts_with("sales · "), "{}", it.title);
    assert!(
        it.body.contains("Globex wants SSO.") && it.body.contains("- Send pricing") && it.body.contains("Q: SAML?")
    );
    m.upsert(MemoryEdit {
        id: Some(sid),
        layer: Layer::Session,
        title: it.title.clone(),
        body: it.body.replace("Send pricing", "Send quote"),
    })
    .unwrap();
    assert!(m.list(Some(Layer::Session)).unwrap().iter().any(|i| i.body.contains("- Send quote")));

    // Answers + feedback, then deleting the session removes everything under it.
    let aid = Uuid::new_v4();
    m.record_answer(&AnswerRecord {
        id: aid,
        session_id: sid,
        trigger: Trigger::Hotkey,
        format: AnswerFormat::Cue,
        intent: Intent::Pricing,
        model: "mock".into(),
        question: "price?".into(),
        end: AnswerEnd {
            answer_id: aid,
            reason: ssc_core::protocol::AnswerEndReason::Done,
            cues: vec![],
            fact: None,
            source_doc: None,
            not_in_docs: true,
        },
    })
    .unwrap();
    m.pin_answer(aid, true).unwrap();
    m.rate_answer(aid, Rating::Up).unwrap();
    m.rate_answer(aid, Rating::Down).unwrap();
    assert_eq!(fs.count("SELECT COUNT(*) FROM answer_feedback WHERE rating = 'down'"), 1);
    assert!(matches!(m.rate_answer(Uuid::new_v4(), Rating::Up), Err(SscError::NotFound)));
    m.delete(sid).unwrap();
    assert_eq!(fs.count("SELECT COUNT(*) FROM answers"), 0);
    assert_eq!(fs.count("SELECT COUNT(*) FROM answer_feedback"), 0);
    assert_eq!(fs.count(&format!("SELECT COUNT(*) FROM session_items WHERE session_id = '{sid}'")), 0);
    // The confirmed contact outlives the session.
    assert_eq!(fs.count("SELECT COUNT(*) FROM contacts"), 1);
}

#[test]
fn transcripts_only_stored_when_kept() {
    let (m, _) = store();
    let sid = Uuid::new_v4();
    m.start_session(sid, None, Mode::Local).unwrap();
    let seg = |text: &str, fin: bool| TranscriptSegment {
        speaker: Speaker::Them,
        text: text.into(),
        is_final: fin,
        t0_ms: 1,
        t1_ms: 2,
        ends_utterance: false,
    };
    assert!(!m.record_transcript(sid, &seg("not kept", true)).unwrap());
    m.set_keep_transcripts(true);
    assert!(!m.record_transcript(sid, &seg("partial", false)).unwrap());
    assert!(m.record_transcript(sid, &seg("kept", true)).unwrap());
    let t = m.transcript(sid).unwrap();
    assert_eq!(t.len(), 1);
    assert_eq!(t[0].text, "kept");
}

#[test]
fn schema_has_no_audio_or_image_columns() {
    let fs = FileStore::new();
    let c = rusqlite::Connection::open(&fs.path).unwrap();
    let mut st = c.prepare("SELECT name FROM sqlite_master WHERE type = 'table'").unwrap();
    let tables: Vec<String> = st.query_map([], |r| r.get(0)).unwrap().map(Result::unwrap).collect();
    assert!(tables.contains(&"vec_chunks".to_string()));
    for t in tables {
        let mut cols = c.prepare(&format!("PRAGMA table_info(\"{t}\")")).unwrap();
        let rows: Vec<(String, String)> =
            cols.query_map([], |r| Ok((r.get(1)?, r.get(2)?))).unwrap().map(Result::unwrap).collect();
        for (name, ty) in rows {
            for bad in ["audio", "image", "screenshot", "wav", "png", "jpg", "pcm"] {
                assert!(!name.to_lowercase().contains(bad), "{t}.{name} looks like raw media");
            }
            // The only BLOBs allowed are sqlite-vec's own shadow tables holding float32 embeddings.
            if ty.eq_ignore_ascii_case("BLOB") {
                assert!(t.starts_with("vec_chunks_"), "{t}.{name} is a BLOB outside the vector index");
            }
        }
    }
}

#[test]
fn rejects_wrong_embedding_dim() {
    let r = SqliteMemory::open_in_memory(Arc::new(MockEmbedder { dim: 128 }));
    assert!(matches!(r, Err(SscError::Invalid(_))));
}
