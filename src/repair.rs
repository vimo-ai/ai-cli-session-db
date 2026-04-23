//! Database repair — `vimo-agent --repair`
//!
//! Handles WAL mismatch (backup restore) and B-tree corruption.
//! Must be run with no other processes accessing the DB.

use std::path::Path;

use anyhow::{Context, Result, bail};

use crate::DbConfig;

pub fn run_repair() -> Result<()> {
    let config = DbConfig::from_env();
    let db_path = std::path::PathBuf::from(&config.url);

    eprintln!("🔧 vimo-agent --repair");
    eprintln!("   DB: {}", db_path.display());

    if !db_path.exists() {
        bail!("Database file not found: {}", db_path.display());
    }

    // Check no other processes have the DB open
    if has_other_db_users(&db_path)? {
        bail!(
            "Other processes are using the database.\n\
             Please stop ETerm and memex first, then retry."
        );
    }

    // Try opening the DB
    let open_result = rusqlite::Connection::open(&db_path);

    match open_result {
        Ok(conn) => {
            eprintln!("   DB opened successfully, checking integrity...");
            repair_with_connection(&conn, &db_path)?;
        }
        Err(e) if e.to_string().to_lowercase().contains("malformed") => {
            eprintln!("   DB malformed: {}", e);
            repair_malformed(&db_path)?;
        }
        Err(e) => bail!("Cannot open DB: {}", e),
    }

    // Clear repair marker
    let marker = db_path.with_extension("db-repair-needed");
    if marker.exists() {
        std::fs::remove_file(&marker)?;
        eprintln!("   Cleared repair marker");
    }

    eprintln!("✅ Repair complete");
    Ok(())
}

fn repair_with_connection(conn: &rusqlite::Connection, db_path: &Path) -> Result<()> {
    let result: String = conn
        .query_row("PRAGMA quick_check;", [], |row| row.get(0))
        .context("quick_check failed")?;

    if result == "ok" {
        eprintln!("   Integrity check: OK");
        // Checkpoint to flush WAL
        conn.execute_batch("PRAGMA wal_checkpoint(TRUNCATE);")?;
        eprintln!("   WAL checkpoint: done");
        return Ok(());
    }

    eprintln!("   Integrity check failed: {}", result);
    eprintln!("   Attempting dump & rebuild...");

    dump_and_rebuild(conn, db_path)
}

fn repair_malformed(db_path: &Path) -> Result<()> {
    let wal_path = db_path.with_extension("db-wal");
    let shm_path = db_path.with_extension("db-shm");

    if wal_path.exists() {
        eprintln!("   Removing stale WAL ({:.1}MB)...",
            std::fs::metadata(&wal_path).map(|m| m.len() as f64 / 1_048_576.0).unwrap_or(0.0));
        let backup = wal_path.with_extension("db-wal.repair-backup");
        std::fs::rename(&wal_path, &backup)
            .context("Failed to backup WAL")?;
        eprintln!("   WAL backed up to: {}", backup.display());
    }
    if shm_path.exists() {
        std::fs::remove_file(&shm_path).context("Failed to remove SHM")?;
    }

    // Retry open
    match rusqlite::Connection::open(db_path) {
        Ok(conn) => {
            eprintln!("   DB opened after WAL cleanup");
            repair_with_connection(&conn, db_path)
        }
        Err(e) => {
            bail!(
                "DB still malformed after WAL cleanup: {}\n\
                 The database file itself is corrupted and needs manual recovery.",
                e
            );
        }
    }
}

fn dump_and_rebuild(conn: &rusqlite::Connection, db_path: &Path) -> Result<()> {
    let tmp_path = db_path.with_extension("db-repair-tmp");

    // Use SQLite's built-in .recover equivalent: dump to SQL then reimport
    let backup_conn = rusqlite::Connection::open(&tmp_path)
        .context("Failed to create temp DB")?;

    backup_conn.execute_batch(
        "PRAGMA journal_mode=WAL;
         PRAGMA synchronous=NORMAL;",
    )?;

    // Copy schema
    let mut stmt = conn.prepare(
        "SELECT sql FROM sqlite_master WHERE sql IS NOT NULL ORDER BY rowid",
    )?;
    let sqls: Vec<String> = stmt
        .query_map([], |row| row.get(0))?
        .filter_map(|r| r.ok())
        .collect();

    for sql in &sqls {
        if let Err(e) = backup_conn.execute_batch(sql) {
            eprintln!("   Schema warning (skipped): {}", e);
        }
    }

    // Copy data table by table
    let tables: Vec<String> = conn
        .prepare("SELECT name FROM sqlite_master WHERE type='table' AND name NOT LIKE 'sqlite_%' AND name NOT LIKE '%_fts%'")?
        .query_map([], |row| row.get(0))?
        .filter_map(|r| r.ok())
        .collect();

    for table in &tables {
        match copy_table(conn, &backup_conn, table) {
            Ok(count) => eprintln!("   {}: {} rows", table, count),
            Err(e) => eprintln!("   {}: FAILED ({})", table, e),
        }
    }

    drop(backup_conn);

    // Swap files
    let corrupt_backup = db_path.with_extension("db-pre-repair");
    std::fs::rename(db_path, &corrupt_backup)
        .context("Failed to backup corrupt DB")?;
    std::fs::rename(&tmp_path, db_path)
        .context("Failed to move repaired DB")?;

    eprintln!("   Corrupt DB saved to: {}", corrupt_backup.display());
    Ok(())
}

fn copy_table(
    src: &rusqlite::Connection,
    dst: &rusqlite::Connection,
    table: &str,
) -> Result<usize> {
    let cols: Vec<String> = src
        .prepare(&format!("PRAGMA table_info(\"{}\")", table))?
        .query_map([], |row| row.get::<_, String>(1))?
        .filter_map(|r| r.ok())
        .collect();

    if cols.is_empty() {
        return Ok(0);
    }

    let col_list = cols.iter().map(|c| format!("\"{}\"", c)).collect::<Vec<_>>().join(", ");
    let placeholders = cols.iter().enumerate().map(|(i, _)| format!("?{}", i + 1)).collect::<Vec<_>>().join(", ");

    let select = format!("SELECT {} FROM \"{}\"", col_list, table);
    let insert = format!("INSERT OR IGNORE INTO \"{}\" ({}) VALUES ({})", table, col_list, placeholders);

    let mut read_stmt = src.prepare(&select)?;
    let col_count = cols.len();

    let mut count = 0usize;
    let tx = dst.unchecked_transaction()?;

    let mut rows = read_stmt.query([])?;
    while let Some(row) = rows.next().unwrap_or(None) {
        let values: Vec<rusqlite::types::Value> = (0..col_count)
            .map(|i| row.get_unwrap(i))
            .collect();
        let params: Vec<&dyn rusqlite::types::ToSql> = values
            .iter()
            .map(|v| v as &dyn rusqlite::types::ToSql)
            .collect();
        if dst.execute(&insert, params.as_slice()).is_ok() {
            count += 1;
        }
    }

    tx.commit()?;
    Ok(count)
}

fn has_other_db_users(db_path: &Path) -> Result<bool> {
    let shm_path = db_path.with_extension("db-shm");
    if !shm_path.exists() {
        return Ok(false);
    }

    use std::os::unix::io::AsRawFd;
    let file = std::fs::File::open(&shm_path)
        .context("Failed to open SHM for lock check")?;
    let fd = file.as_raw_fd();

    let result = unsafe { libc::flock(fd, libc::LOCK_EX | libc::LOCK_NB) };
    if result == 0 {
        unsafe { libc::flock(fd, libc::LOCK_UN) };
        Ok(false)
    } else {
        Ok(true)
    }
}
