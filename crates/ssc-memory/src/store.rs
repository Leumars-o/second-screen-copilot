//! [`SqliteMemory`]: the SQLite-backed [`MemoryStore`].

use crate::db::{self, DbResult};
use crate::index::{VectorIndex, normalize};
use crate::ingest::{self, ChunkConfig, DocKind};
use crate::items::{self, ABOUT_ME_ID};
use rusqlite::{Connection, OptionalExtension, Transaction, params};
use sha2::{Digest, Sha256};
use ssc_core::protocol::{Layer, MemoryEdit, MemoryItem};
use ssc_core::traits::{Clock, Embedder, MemoryStore, SystemClock};
use ssc_core::types::{AboutMe, Chunk, Profile};
use ssc_core::{Result, SscError};
use std::path::Path;
use std::sync::{Arc, Mutex, MutexGuard, RwLock};
use uuid::Uuid;

/// Embedding dimension fixed by `contracts/db/0002_vec.sql`.
pub const EMBED_DIM: usize = 384;
const EMBED_BATCH: usize = 32;

/// A stored document (one upload).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DocInfo {
    pub id: Uuid,
    pub profile_id: String,
    pub name: String,
    pub mime: String,
    pub chunks: usize,
    pub added_at: u64,
}

pub struct SqliteMemory {
    pub(crate) conn: Mutex<Connection>,
    pub(crate) index: RwLock<VectorIndex>,
    pub(crate) embedder: Arc<dyn Embedder>,
    pub(crate) clock: Arc<dyn Clock>,
    pub(crate) chunking: ChunkConfig,
    pub(crate) keep_transcripts: std::sync::atomic::AtomicBool,
    /// Chunks with cosine at or below this are never returned (so "not in your docs" is detectable).
    pub min_score: f32,
}

pub(crate) fn now(clock: &dyn Clock) -> i64 {
    clock.now_ms() as i64
}

pub(crate) fn parse_uuid(s: &str) -> Result<Uuid> {
    Uuid::parse_str(s).map_err(|e| SscError::Invalid(format!("bad id {s:?} in store: {e}")))
}

impl SqliteMemory {
    /// Opens or creates the store at `path`. `embedder.dim()` must be 384.
    pub fn open(path: &Path, embedder: Arc<dyn Embedder>) -> Result<Self> {
        Self::build(db::open(Some(path))?, embedder, Arc::new(SystemClock))
    }

    pub fn open_in_memory(embedder: Arc<dyn Embedder>) -> Result<Self> {
        Self::build(db::open(None)?, embedder, Arc::new(SystemClock))
    }

    /// For tests: a custom clock.
    pub fn open_in_memory_with_clock(embedder: Arc<dyn Embedder>, clock: Arc<dyn Clock>) -> Result<Self> {
        Self::build(db::open(None)?, embedder, clock)
    }

    fn build(conn: Connection, embedder: Arc<dyn Embedder>, clock: Arc<dyn Clock>) -> Result<Self> {
        if embedder.dim() != EMBED_DIM {
            return Err(SscError::Invalid(format!(
                "embedder dim {} != {EMBED_DIM} (fixed by contracts/db/0002_vec.sql)",
                embedder.dim()
            )));
        }
        let index = load_index(&conn)?;
        Ok(Self {
            conn: Mutex::new(conn),
            index: RwLock::new(index),
            embedder,
            clock,
            chunking: ChunkConfig::default(),
            keep_transcripts: false.into(),
            min_score: 0.0,
        })
    }

    pub fn with_chunking(mut self, cfg: ChunkConfig) -> Self {
        self.chunking = cfg;
        self
    }

    /// Mirrors `Config::keep_transcripts`. Off by default: transcripts are dropped, not stored.
    pub fn set_keep_transcripts(&self, keep: bool) {
        self.keep_transcripts.store(keep, std::sync::atomic::Ordering::SeqCst);
    }

    pub(crate) fn conn(&self) -> MutexGuard<'_, Connection> {
        self.conn.lock().unwrap_or_else(|p| p.into_inner())
    }

    pub(crate) fn now(&self) -> i64 {
        now(self.clock.as_ref())
    }

    /// Number of vectors in the in-memory index (tests / debug panel).
    pub fn indexed_chunks(&self) -> usize {
        self.index.read().unwrap_or_else(|p| p.into_inner()).len()
    }

    // ---------- typed accessors used by the context builder ----------

    pub fn about_me(&self) -> Result<Option<AboutMe>> {
        self.conn()
            .query_row(
                "SELECT name, role, company, product, tone, preferred_answers FROM about_me WHERE id = 1",
                [],
                |r| {
                    Ok(AboutMe {
                        name: r.get(0)?,
                        role: r.get(1)?,
                        company: r.get(2)?,
                        product: r.get(3)?,
                        tone: r.get(4)?,
                        preferred_answers: r.get(5)?,
                    })
                },
            )
            .optional()
            .db()
    }

    pub fn set_about_me(&self, a: &AboutMe) -> Result<MemoryItem> {
        let now = self.now();
        self.conn()
            .execute(
                "INSERT INTO about_me (id, name, role, company, product, tone, preferred_answers, updated_at)
                 VALUES (1, ?1, ?2, ?3, ?4, ?5, ?6, ?7)
                 ON CONFLICT(id) DO UPDATE SET name = ?1, role = ?2, company = ?3, product = ?4, tone = ?5,
                   preferred_answers = ?6, updated_at = ?7",
                params![a.name, a.role, a.company, a.product, a.tone, a.preferred_answers, now],
            )
            .db()?;
        Ok(MemoryItem {
            id: ABOUT_ME_ID,
            layer: Layer::AboutMe,
            title: a.name.clone(),
            body: items::about_me_body(a),
            updated_at: now as u64,
        })
    }

    pub fn profile(&self, slug: &str) -> Result<Option<Profile>> {
        self.conn()
            .query_row("SELECT id, name, instructions FROM profiles WHERE id = ?1", [slug], |r| {
                Ok(Profile { id: r.get(0)?, name: r.get(1)?, instructions: r.get(2)? })
            })
            .optional()
            .db()
    }

    pub fn profiles(&self) -> Result<Vec<Profile>> {
        let conn = self.conn();
        let mut st = conn.prepare("SELECT id, name, instructions FROM profiles ORDER BY name").db()?;
        let rows = st
            .query_map([], |r| Ok(Profile { id: r.get(0)?, name: r.get(1)?, instructions: r.get(2)? }))
            .db()?
            .collect::<std::result::Result<Vec<_>, _>>()
            .db()?;
        Ok(rows)
    }

    /// Creates or updates a profile under an explicit slug.
    pub fn put_profile(&self, p: &Profile) -> Result<MemoryItem> {
        let now = self.now();
        self.conn()
            .execute(
                "INSERT INTO profiles (id, name, instructions, created_at, updated_at) VALUES (?1, ?2, ?3, ?4, ?4)
                 ON CONFLICT(id) DO UPDATE SET name = ?2, instructions = ?3, updated_at = ?4",
                params![p.id, p.name, p.instructions, now],
            )
            .db()?;
        Ok(MemoryItem {
            id: items::profile_uuid(&p.id),
            layer: Layer::Profile,
            title: p.name.clone(),
            body: p.instructions.clone(),
            updated_at: now as u64,
        })
    }

    pub(crate) fn profile_slug_for(&self, conn: &Connection, id: Uuid) -> Result<Option<String>> {
        let mut st = conn.prepare("SELECT id FROM profiles").db()?;
        let slugs = st.query_map([], |r| r.get::<_, String>(0)).db()?;
        for s in slugs {
            let s = s.db()?;
            if items::profile_uuid(&s) == id {
                return Ok(Some(s));
            }
        }
        Ok(None)
    }

    // ---------- docs ----------

    /// Uploads a document into a profile: extract text → chunk → embed → store rows + vectors.
    /// The type comes from the file name's extension (pdf, docx, md, txt). Re-uploading identical
    /// bytes to the same profile returns the existing doc.
    pub fn ingest_doc(&self, profile_id: &str, name: &str, bytes: &[u8]) -> Result<DocInfo> {
        let kind = DocKind::from_name(name)
            .ok_or_else(|| SscError::Invalid(format!("unsupported document type: {name} (pdf, docx, md, txt)")))?;
        let sha = hex(&Sha256::digest(bytes));
        if let Some(existing) = self.doc_by_sha(profile_id, &sha)? {
            return Ok(existing);
        }
        if self.profile(profile_id)?.is_none() {
            return Err(SscError::NotFound);
        }
        let text = ingest::extract_text(kind, bytes)?;
        let chunks = ingest::chunk_text(&text, self.chunking);

        // Embedding is the slow part: done before taking the DB lock.
        let mut vecs = Vec::with_capacity(chunks.len());
        for batch in chunks.chunks(EMBED_BATCH) {
            let texts: Vec<String> = batch.iter().map(|c| c.text.clone()).collect();
            let out = self.embedder.embed(&texts)?;
            if out.len() != texts.len() || out.iter().any(|v| v.len() != EMBED_DIM) {
                return Err(SscError::Invalid("embedder returned wrong shape".into()));
            }
            vecs.extend(out);
        }
        vecs.iter_mut().for_each(|v| normalize(v));

        let doc_id = Uuid::new_v4();
        let now = self.now();
        let mut rowids = Vec::with_capacity(chunks.len());
        {
            let mut conn = self.conn();
            let tx = conn.transaction().db()?;
            tx.execute(
                "INSERT INTO docs (id, profile_id, name, mime, sha256, added_at) VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                params![doc_id.to_string(), profile_id, name, kind.mime(), sha, now],
            )
            .db()?;
            {
                let mut ins_chunk = tx
                    .prepare("INSERT INTO chunks (id, doc_id, ord, text, tokens) VALUES (?1, ?2, ?3, ?4, ?5)")
                    .db()?;
                let mut ins_vec = tx.prepare("INSERT INTO vec_chunks (rowid, embedding) VALUES (?1, ?2)").db()?;
                for (i, (c, v)) in chunks.iter().zip(&vecs).enumerate() {
                    ins_chunk
                        .execute(params![
                            Uuid::new_v4().to_string(),
                            doc_id.to_string(),
                            i as i64,
                            c.text,
                            c.tokens as i64
                        ])
                        .db()?;
                    let rowid = tx.last_insert_rowid();
                    ins_vec.execute(params![rowid, f32_bytes(v)]).db()?;
                    rowids.push(rowid);
                }
            }
            tx.commit().db()?;
            // Index updated while still holding the DB lock so a concurrent delete can't interleave.
            let mut ix = self.index.write().unwrap_or_else(|p| p.into_inner());
            for (rowid, v) in rowids.iter().zip(&vecs) {
                ix.insert(profile_id, *rowid, v);
            }
        }
        Ok(DocInfo {
            id: doc_id,
            profile_id: profile_id.into(),
            name: name.into(),
            mime: kind.mime().into(),
            chunks: chunks.len(),
            added_at: now as u64,
        })
    }

    fn doc_by_sha(&self, profile_id: &str, sha: &str) -> Result<Option<DocInfo>> {
        let conn = self.conn();
        let id: Option<String> = conn
            .query_row("SELECT id FROM docs WHERE profile_id = ?1 AND sha256 = ?2", params![profile_id, sha], |r| {
                r.get(0)
            })
            .optional()
            .db()?;
        drop(conn);
        match id {
            Some(id) => Ok(self.docs(profile_id)?.into_iter().find(|d| d.id.to_string() == id)),
            None => Ok(None),
        }
    }

    pub fn docs(&self, profile_id: &str) -> Result<Vec<DocInfo>> {
        let conn = self.conn();
        let mut st = conn
            .prepare(
                "SELECT d.id, d.profile_id, d.name, d.mime, d.added_at, (SELECT COUNT(*) FROM chunks c WHERE c.doc_id = d.id)
                 FROM docs d WHERE d.profile_id = ?1 ORDER BY d.added_at, d.name",
            )
            .db()?;
        let rows = st
            .query_map([profile_id], |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, String>(1)?,
                    r.get::<_, String>(2)?,
                    r.get::<_, String>(3)?,
                    r.get::<_, i64>(4)?,
                    r.get::<_, i64>(5)?,
                ))
            })
            .db()?
            .collect::<std::result::Result<Vec<_>, _>>()
            .db()?;
        rows.into_iter()
            .map(|(id, profile_id, name, mime, added, n)| {
                Ok(DocInfo { id: parse_uuid(&id)?, profile_id, name, mime, chunks: n as usize, added_at: added as u64 })
            })
            .collect()
    }

    /// Deletes a document, its chunks and their vectors.
    pub fn delete_doc(&self, doc_id: Uuid) -> Result<()> {
        let mut conn = self.conn();
        let tx = conn.transaction().db()?;
        let rowids = rowids_where(&tx, "c.doc_id = ?1", &doc_id.to_string())?;
        delete_vectors(&tx, &rowids)?;
        let n = tx.execute("DELETE FROM docs WHERE id = ?1", [doc_id.to_string()]).db()?;
        if n == 0 {
            return Err(SscError::NotFound);
        }
        tx.commit().db()?;
        self.index.write().unwrap_or_else(|p| p.into_inner()).remove(&rowids);
        Ok(())
    }

    /// Deletes a profile with all its docs, chunks and vectors.
    pub(crate) fn delete_profile_slug(&self, conn: &mut Connection, slug: &str) -> Result<()> {
        let tx = conn.transaction().db()?;
        let rowids = rowids_where(&tx, "d.profile_id = ?1", slug)?;
        delete_vectors(&tx, &rowids)?;
        tx.execute("DELETE FROM profiles WHERE id = ?1", [slug]).db()?;
        tx.commit().db()?;
        self.index.write().unwrap_or_else(|p| p.into_inner()).remove_profile(slug);
        Ok(())
    }
}

fn rowids_where(tx: &Transaction<'_>, cond: &str, arg: &str) -> Result<Vec<i64>> {
    let mut st =
        tx.prepare(&format!("SELECT c.rowid FROM chunks c JOIN docs d ON d.id = c.doc_id WHERE {cond}")).db()?;
    let ids = st.query_map([arg], |r| r.get(0)).db()?.collect::<std::result::Result<Vec<i64>, _>>().db()?;
    Ok(ids)
}

fn delete_vectors(tx: &Transaction<'_>, rowids: &[i64]) -> Result<()> {
    let mut st = tx.prepare("DELETE FROM vec_chunks WHERE rowid = ?1").db()?;
    for r in rowids {
        st.execute([r]).db()?;
    }
    Ok(())
}

fn f32_bytes(v: &[f32]) -> Vec<u8> {
    v.iter().flat_map(|x| x.to_le_bytes()).collect()
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

fn load_index(conn: &Connection) -> Result<VectorIndex> {
    let mut ix = VectorIndex::new(EMBED_DIM);
    let mut profile_of = std::collections::HashMap::new();
    {
        let mut st = conn.prepare("SELECT c.rowid, d.profile_id FROM chunks c JOIN docs d ON d.id = c.doc_id").db()?;
        let rows = st.query_map([], |r| Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?))).db()?;
        for row in rows {
            let (rowid, p) = row.db()?;
            profile_of.insert(rowid, p);
        }
    }
    let mut st = conn.prepare("SELECT rowid, embedding FROM vec_chunks").db()?;
    let rows = st.query_map([], |r| Ok((r.get::<_, i64>(0)?, r.get::<_, Vec<u8>>(1)?))).db()?;
    for row in rows {
        let (rowid, blob) = row.db()?;
        let Some(p) = profile_of.get(&rowid) else { continue };
        if blob.len() != EMBED_DIM * 4 {
            return Err(SscError::Invalid(format!("vector {rowid} has {} bytes", blob.len())));
        }
        let v: Vec<f32> = blob.chunks_exact(4).map(|b| f32::from_le_bytes([b[0], b[1], b[2], b[3]])).collect();
        ix.insert(p, rowid, &v);
    }
    Ok(ix)
}

// ---------- MemoryStore ----------

impl SqliteMemory {
    fn item_rows(&self, conn: &Connection, sql: &str, layer: Layer) -> Result<Vec<MemoryItem>> {
        let mut st = conn.prepare(sql).db()?;
        let rows = st
            .query_map([], |r| {
                Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?, r.get::<_, String>(2)?, r.get::<_, i64>(3)?))
            })
            .db()?
            .collect::<std::result::Result<Vec<_>, _>>()
            .db()?;
        rows.into_iter()
            .map(|(id, title, body, t)| {
                let id = if layer == Layer::Profile { items::profile_uuid(&id) } else { parse_uuid(&id)? };
                Ok(MemoryItem { id, layer, title, body, updated_at: t as u64 })
            })
            .collect()
    }

    fn list_layer(&self, conn: &Connection, layer: Layer) -> Result<Vec<MemoryItem>> {
        match layer {
            Layer::AboutMe => {
                let row = conn
                    .query_row(
                        "SELECT name, role, company, product, tone, preferred_answers, updated_at FROM about_me WHERE id = 1",
                        [],
                        |r| {
                            Ok((
                                AboutMe {
                                    name: r.get(0)?,
                                    role: r.get(1)?,
                                    company: r.get(2)?,
                                    product: r.get(3)?,
                                    tone: r.get(4)?,
                                    preferred_answers: r.get(5)?,
                                },
                                r.get::<_, i64>(6)?,
                            ))
                        },
                    )
                    .optional()
                    .db()?;
                Ok(row
                    .map(|(a, t)| MemoryItem {
                        id: ABOUT_ME_ID,
                        layer: Layer::AboutMe,
                        title: a.name.clone(),
                        body: items::about_me_body(&a),
                        updated_at: t as u64,
                    })
                    .into_iter()
                    .collect())
            }
            Layer::Profile => self.item_rows(
                conn,
                "SELECT id, name, instructions, updated_at FROM profiles ORDER BY updated_at DESC, id",
                layer,
            ),
            Layer::Contact => self.item_rows(
                conn,
                "SELECT id, name, notes, updated_at FROM contacts ORDER BY updated_at DESC, id",
                layer,
            ),
            Layer::Account => self.item_rows(
                conn,
                "SELECT id, name, notes, updated_at FROM accounts ORDER BY updated_at DESC, id",
                layer,
            ),
            Layer::Session => {
                let mut st = conn
                    .prepare("SELECT s.id FROM sessions s ORDER BY COALESCE(s.ended_at, s.started_at) DESC, s.id")
                    .db()?;
                let ids = st
                    .query_map([], |r| r.get::<_, String>(0))
                    .db()?
                    .collect::<std::result::Result<Vec<_>, _>>()
                    .db()?;
                ids.iter().map(|id| self.session_item(conn, id)).collect()
            }
        }
    }

    fn upsert_in(&self, conn: &mut Connection, edit: MemoryEdit) -> Result<MemoryItem> {
        let now = self.now();
        let title = edit.title.trim().to_string();
        match edit.layer {
            Layer::AboutMe => {
                if edit.id.is_some_and(|id| id != ABOUT_ME_ID) {
                    return Err(SscError::NotFound);
                }
                let a = items::parse_about_me(&title, &edit.body);
                conn.execute(
                    "INSERT INTO about_me (id, name, role, company, product, tone, preferred_answers, updated_at)
                     VALUES (1, ?1, ?2, ?3, ?4, ?5, ?6, ?7)
                     ON CONFLICT(id) DO UPDATE SET name = ?1, role = ?2, company = ?3, product = ?4, tone = ?5,
                       preferred_answers = ?6, updated_at = ?7",
                    params![a.name, a.role, a.company, a.product, a.tone, a.preferred_answers, now],
                )
                .db()?;
                Ok(MemoryItem {
                    id: ABOUT_ME_ID,
                    layer: Layer::AboutMe,
                    title: a.name.clone(),
                    body: items::about_me_body(&a),
                    updated_at: now as u64,
                })
            }
            Layer::Profile => {
                if title.is_empty() {
                    return Err(SscError::Invalid("profile name is empty".into()));
                }
                let slug = match edit.id {
                    Some(id) => self.profile_slug_for(conn, id)?.ok_or(SscError::NotFound)?,
                    None => {
                        let base = items::slugify(&title);
                        let mut slug = base.clone();
                        let mut n = 2;
                        while conn
                            .query_row("SELECT COUNT(*) FROM profiles WHERE id = ?1", [&slug], |r| r.get::<_, i64>(0))
                            .db()?
                            > 0
                        {
                            slug = format!("{base}-{n}");
                            n += 1;
                        }
                        slug
                    }
                };
                conn.execute(
                    "INSERT INTO profiles (id, name, instructions, created_at, updated_at) VALUES (?1, ?2, ?3, ?4, ?4)
                     ON CONFLICT(id) DO UPDATE SET name = ?2, instructions = ?3, updated_at = ?4",
                    params![slug, title, edit.body, now],
                )
                .db()?;
                Ok(MemoryItem {
                    id: items::profile_uuid(&slug),
                    layer: Layer::Profile,
                    title,
                    body: edit.body,
                    updated_at: now as u64,
                })
            }
            Layer::Contact | Layer::Account => {
                if title.is_empty() {
                    return Err(SscError::Invalid("name is empty".into()));
                }
                let table = if edit.layer == Layer::Contact { "contacts" } else { "accounts" };
                let id = match edit.id {
                    Some(id) => {
                        let n = conn
                            .execute(
                                &format!("UPDATE {table} SET name = ?2, notes = ?3, updated_at = ?4 WHERE id = ?1"),
                                params![id.to_string(), title, edit.body, now],
                            )
                            .db()?;
                        if n == 0 {
                            return Err(SscError::NotFound);
                        }
                        id
                    }
                    None => {
                        let id = Uuid::new_v4();
                        conn.execute(
                            &format!("INSERT INTO {table} (id, name, notes, updated_at) VALUES (?1, ?2, ?3, ?4)"),
                            params![id.to_string(), title, edit.body, now],
                        )
                        .db()?;
                        id
                    }
                };
                Ok(MemoryItem { id, layer: edit.layer, title, body: edit.body, updated_at: now as u64 })
            }
            Layer::Session => {
                let outcome = items::parse_session(&edit.body);
                let tx = conn.transaction().db()?;
                let id = match edit.id {
                    Some(id) => {
                        let exists: i64 = tx
                            .query_row("SELECT COUNT(*) FROM sessions WHERE id = ?1", [id.to_string()], |r| r.get(0))
                            .db()?;
                        if exists == 0 {
                            return Err(SscError::NotFound);
                        }
                        id
                    }
                    None => {
                        // A hand-written note about a past call: no profile, no live capture.
                        let id = Uuid::new_v4();
                        tx.execute(
                            "INSERT INTO sessions (id, profile_id, mode, started_at, ended_at) VALUES (?1, NULL, 'local', ?2, ?2)",
                            params![id.to_string(), now],
                        )
                        .db()?;
                        id
                    }
                };
                crate::session::replace_items(&tx, id, &outcome, now)?;
                tx.commit().db()?;
                self.session_item(conn, &id.to_string())
            }
        }
    }
}

impl MemoryStore for SqliteMemory {
    fn list(&self, layer: Option<Layer>) -> Result<Vec<MemoryItem>> {
        let conn = self.conn();
        match layer {
            Some(l) => self.list_layer(&conn, l),
            None => {
                let mut all = Vec::new();
                for l in [Layer::AboutMe, Layer::Profile, Layer::Contact, Layer::Account, Layer::Session] {
                    all.extend(self.list_layer(&conn, l)?);
                }
                Ok(all)
            }
        }
    }

    fn upsert(&self, edit: MemoryEdit) -> Result<MemoryItem> {
        let mut conn = self.conn();
        self.upsert_in(&mut conn, edit)
    }

    /// Deletes any item by id: About Me, a profile (with its docs and vectors), a contact, an
    /// account, a session (with its items, transcript and answers) or a single doc.
    fn delete(&self, id: Uuid) -> Result<()> {
        let mut conn = self.conn();
        let sid = id.to_string();
        if id == ABOUT_ME_ID {
            let n = conn.execute("DELETE FROM about_me", []).db()?;
            return if n == 0 { Err(SscError::NotFound) } else { Ok(()) };
        }
        for table in ["contacts", "accounts", "sessions"] {
            if conn.execute(&format!("DELETE FROM {table} WHERE id = ?1"), [&sid]).db()? > 0 {
                return Ok(());
            }
        }
        if let Some(slug) = self.profile_slug_for(&conn, id)? {
            return self.delete_profile_slug(&mut conn, &slug);
        }
        let is_doc: i64 = conn.query_row("SELECT COUNT(*) FROM docs WHERE id = ?1", [&sid], |r| r.get(0)).db()?;
        drop(conn);
        if is_doc > 0 {
            return self.delete_doc(id);
        }
        Err(SscError::NotFound)
    }

    fn search(&self, query: &str, profile_id: &str, k: usize) -> Result<Vec<Chunk>> {
        if query.trim().is_empty() || k == 0 {
            return Ok(vec![]);
        }
        let mut q = self
            .embedder
            .embed(&[query.to_string()])?
            .pop()
            .ok_or_else(|| SscError::Invalid("embedder returned nothing".into()))?;
        if q.len() != EMBED_DIM {
            return Err(SscError::Invalid("embedder returned wrong dim".into()));
        }
        normalize(&mut q);
        let hits = self.index.read().unwrap_or_else(|p| p.into_inner()).search(profile_id, &q, k, self.min_score);
        if hits.is_empty() {
            return Ok(vec![]);
        }
        let conn = self.conn();
        let mut st = conn
            .prepare_cached(
                "SELECT c.id, c.doc_id, d.name, c.text FROM chunks c JOIN docs d ON d.id = c.doc_id WHERE c.rowid = ?1",
            )
            .db()?;
        let mut out = Vec::with_capacity(hits.len());
        for (rowid, score) in hits {
            let row = st
                .query_row([rowid], |r| {
                    Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?, r.get::<_, String>(2)?, r.get::<_, String>(3)?))
                })
                .optional()
                .db()?;
            // Missing row: deleted between the index read and now.
            if let Some((id, doc_id, doc_name, text)) = row {
                out.push(Chunk { id: parse_uuid(&id)?, doc_id: parse_uuid(&doc_id)?, doc_name, text, score });
            }
        }
        Ok(out)
    }
}
