//! Opening the database: sqlite-vec registration and the `contracts/db` migrations.

use rusqlite::Connection;
use ssc_core::{Result, SscError};
use std::sync::Once;

const MIGRATION_0001: &str = include_str!("../../../contracts/db/0001_init.sql");
const MIGRATION_0002: &str = include_str!("../../../contracts/db/0002_vec.sql");

/// Schema version after all migrations in `contracts/db/` are applied.
pub const SCHEMA_VERSION: i64 = 2;

pub(crate) fn db_err(e: rusqlite::Error) -> SscError {
    SscError::Other(anyhow::Error::new(e))
}

pub(crate) trait DbResult<T> {
    fn db(self) -> Result<T>;
}

impl<T> DbResult<T> for std::result::Result<T, rusqlite::Error> {
    fn db(self) -> Result<T> {
        self.map_err(db_err)
    }
}

/// Registers sqlite-vec for every connection opened afterwards in this process.
#[allow(unsafe_code)]
fn register_sqlite_vec() {
    static ONCE: Once = Once::new();
    ONCE.call_once(|| {
        // SAFETY: `sqlite3_vec_init` is the extension entry point compiled into this binary by the
        // `sqlite-vec` crate. Its real signature is the one `sqlite3_auto_extension` expects
        // (db, pzErrMsg, pApi) -> c_int; the crate only declares it as `fn()`. SQLite calls it once per
        // new connection, on the thread that opens it.
        unsafe {
            type Init = unsafe extern "C" fn(
                *mut rusqlite::ffi::sqlite3,
                *mut *mut std::os::raw::c_char,
                *const rusqlite::ffi::sqlite3_api_routines,
            ) -> std::os::raw::c_int;
            let init = std::mem::transmute::<*const (), Init>(sqlite_vec::sqlite3_vec_init as *const ());
            rusqlite::ffi::sqlite3_auto_extension(Some(init));
        }
    });
}

/// Opens (or creates) the store at `path` (`None` = in-memory) and brings the schema up to date.
pub(crate) fn open(path: Option<&std::path::Path>) -> Result<Connection> {
    register_sqlite_vec();
    let conn = match path {
        Some(p) => Connection::open(p),
        None => Connection::open_in_memory(),
    }
    .db()?;
    // Per-connection settings: the PRAGMA inside 0001 only affects the connection that ran it.
    conn.execute_batch("PRAGMA foreign_keys = ON; PRAGMA journal_mode = WAL; PRAGMA synchronous = NORMAL;").db()?;
    migrate(&conn)?;
    Ok(conn)
}

fn current_version(conn: &Connection) -> Result<i64> {
    let has_table: bool = conn
        .query_row("SELECT COUNT(*) > 0 FROM sqlite_master WHERE type = 'table' AND name = 'schema_version'", [], |r| {
            r.get(0)
        })
        .db()?;
    if !has_table {
        return Ok(0);
    }
    conn.query_row("SELECT version FROM schema_version", [], |r| r.get(0)).db()
}

fn migrate(conn: &Connection) -> Result<()> {
    let v = current_version(conn)?;
    if v > SCHEMA_VERSION {
        return Err(SscError::Invalid(format!("database schema v{v} is newer than this build (v{SCHEMA_VERSION})")));
    }
    for (target, sql) in [(1, MIGRATION_0001), (2, MIGRATION_0002)] {
        if v < target {
            // execute_batch inside an explicit transaction: a failed migration leaves nothing behind.
            conn.execute_batch(&format!("BEGIN;\n{sql}\nCOMMIT;")).map_err(|e| {
                let _ = conn.execute_batch("ROLLBACK;");
                db_err(e)
            })?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn migrates_fresh_db_and_is_idempotent() {
        let dir = std::env::temp_dir().join(format!("ssc-mem-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("m.sqlite");
        {
            let c = open(Some(&path)).unwrap();
            assert_eq!(current_version(&c).unwrap(), 2);
        }
        // Reopen: no re-application (would fail on CREATE TABLE).
        let c = open(Some(&path)).unwrap();
        assert_eq!(current_version(&c).unwrap(), 2);
        let fk: i64 = c.query_row("PRAGMA foreign_keys", [], |r| r.get(0)).unwrap();
        assert_eq!(fk, 1);
        let vec_version: String = c.query_row("SELECT vec_version()", [], |r| r.get(0)).unwrap();
        assert!(vec_version.starts_with('v'));
        drop(c);
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
