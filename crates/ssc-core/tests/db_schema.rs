//! The SQLite migration applies cleanly and never stores raw audio or images (spec: Memory rules).

use std::path::Path;

fn migration() -> String {
    std::fs::read_to_string(Path::new(env!("CARGO_MANIFEST_DIR")).join("../../contracts/db/0001_init.sql")).unwrap()
}

#[test]
fn migration_applies_and_has_expected_tables() {
    let db = rusqlite::Connection::open_in_memory().unwrap();
    db.execute_batch(&migration()).unwrap();
    let mut stmt = db.prepare("SELECT name FROM sqlite_master WHERE type = 'table' ORDER BY name").unwrap();
    let tables: Vec<String> = stmt.query_map([], |r| r.get(0)).unwrap().map(Result::unwrap).collect();
    assert_eq!(
        tables,
        [
            "about_me",
            "accounts",
            "answer_feedback",
            "answers",
            "chunks",
            "contacts",
            "docs",
            "pending_contact_suggestions",
            "profiles",
            "schema_version",
            "session_items",
            "sessions",
            "transcripts",
        ]
    );
}

#[test]
fn no_audio_or_image_columns() {
    let db = rusqlite::Connection::open_in_memory().unwrap();
    db.execute_batch(&migration()).unwrap();
    let mut stmt = db.prepare("SELECT name FROM sqlite_master WHERE type = 'table'").unwrap();
    let tables: Vec<String> = stmt.query_map([], |r| r.get(0)).unwrap().map(Result::unwrap).collect();
    for t in tables {
        let mut cols = db.prepare(&format!("PRAGMA table_info({t})")).unwrap();
        let rows: Vec<(String, String)> =
            cols.query_map([], |r| Ok((r.get(1)?, r.get(2)?))).unwrap().map(Result::unwrap).collect();
        for (name, ty) in rows {
            assert_ne!(ty.to_uppercase(), "BLOB", "{t}.{name} is a BLOB");
            for bad in ["audio", "image", "screenshot", "wav", "png", "pcm"] {
                assert!(!name.to_lowercase().contains(bad), "{t}.{name} looks like raw media");
            }
        }
    }
}

#[test]
fn foreign_keys_cascade_session_delete() {
    let db = rusqlite::Connection::open_in_memory().unwrap();
    db.execute_batch(&migration()).unwrap();
    db.execute_batch(
        "INSERT INTO sessions (id, mode, started_at) VALUES ('s1', 'cloud', 1);
         INSERT INTO session_items (id, session_id, kind, body, created_at) VALUES ('i1', 's1', 'summary', 'x', 1);
         INSERT INTO transcripts (session_id, speaker, text, t0, t1) VALUES ('s1', 'them', 'hi', 1, 2);
         DELETE FROM sessions WHERE id = 's1';",
    )
    .unwrap();
    let n: i64 = db
        .query_row("SELECT (SELECT COUNT(*) FROM session_items) + (SELECT COUNT(*) FROM transcripts)", [], |r| r.get(0))
        .unwrap();
    assert_eq!(n, 0, "deleting a session must delete its items (user can delete everything)");
}
