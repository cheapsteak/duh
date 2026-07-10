use rusqlite::Connection;
use std::path::{Path, PathBuf};

/// Verbatim from the reference oracle `reference/duh-py` lines 295-341 (`_init_schema`'s `executescript` block)
/// plus the `freeable_cache` table DDL from `reference/duh-py` lines 1617-1623
/// (`_persist_freeable_cache`'s `executescript` block). Do not "improve" this SQL.
pub const SCHEMA: &str = r#"
CREATE TABLE IF NOT EXISTS scans (
  id INTEGER PRIMARY KEY,
  root TEXT NOT NULL,
  started_at REAL NOT NULL,
  finished_at REAL,
  files_count INTEGER,
  excluded_count INTEGER,
  bytes_logical INTEGER,
  bytes_blocks INTEGER,
  schema_version INTEGER DEFAULT 2
);

CREATE TABLE IF NOT EXISTS files (
  id INTEGER PRIMARY KEY,
  parent_id INTEGER REFERENCES files(id),
  name TEXT NOT NULL,
  is_dir INTEGER NOT NULL,
  is_symlink INTEGER NOT NULL,
  is_excluded INTEGER NOT NULL DEFAULT 0,
  dev INTEGER NOT NULL,
  ino INTEGER NOT NULL,
  clone_id INTEGER,
  nlinks INTEGER NOT NULL,
  size_logical INTEGER NOT NULL,
  size_blocks INTEGER NOT NULL,
  excluded_file_count INTEGER,
  mtime INTEGER NOT NULL,
  scan_id INTEGER NOT NULL REFERENCES scans(id),
  UNIQUE(parent_id, name)
);

CREATE INDEX IF NOT EXISTS idx_files_parent ON files(parent_id);
CREATE INDEX IF NOT EXISTS idx_files_clone ON files(clone_id) WHERE clone_id IS NOT NULL;
CREATE INDEX IF NOT EXISTS idx_files_inode ON files(dev, ino) WHERE nlinks > 1;
CREATE INDEX IF NOT EXISTS idx_files_excluded ON files(is_excluded) WHERE is_excluded = 1;

CREATE TABLE IF NOT EXISTS excluded_families (
  excluded_id INTEGER NOT NULL REFERENCES files(id),
  clone_id INTEGER NOT NULL,
  member_count INTEGER NOT NULL,
  blocks_sum INTEGER NOT NULL,
  max_blocks INTEGER NOT NULL,
  PRIMARY KEY (excluded_id, clone_id)
);
CREATE INDEX IF NOT EXISTS idx_excluded_families_clone ON excluded_families(clone_id);

CREATE TABLE IF NOT EXISTS freeable_cache (
  node_id INTEGER PRIMARY KEY,
  freeable INTEGER NOT NULL,
  locked_here INTEGER NOT NULL,
  scan_id INTEGER NOT NULL
);
"#;

/// Resolve the default DB path: `DUH_DB` env var, or `~/.local/share/duh/scan.db`.
///
/// Note: this does not consider the `--db` CLI flag; callers should check that first.
pub fn default_db_path() -> PathBuf {
    if let Ok(p) = std::env::var("DUH_DB") {
        return PathBuf::from(p);
    }
    let home = std::env::var("HOME").expect("HOME not set");
    PathBuf::from(home).join(".local/share/duh/scan.db")
}

/// Open (creating if necessary) the database at `path`, enable WAL mode, foreign keys,
/// and apply the schema.
pub fn open(path: &Path) -> rusqlite::Result<Connection> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).ok();
    }
    let con = Connection::open(path)?;
    con.pragma_update(None, "journal_mode", "WAL")?;
    con.pragma_update(None, "synchronous", "NORMAL")?;
    con.pragma_update(None, "foreign_keys", "ON")?;
    con.execute_batch(SCHEMA)?;
    Ok(con)
}

#[cfg(test)]
mod tests {
    #[test]
    fn schema_applies_and_tables_exist() {
        let tmp = std::env::temp_dir().join(format!("duh-test-{}.db", std::process::id()));
        let con = super::open(&tmp).unwrap();
        for t in ["scans", "files", "excluded_families", "freeable_cache"] {
            let n: i64 = con
                .query_row(
                    "SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name=?1",
                    [t],
                    |r| r.get(0),
                )
                .unwrap();
            assert_eq!(n, 1, "missing table {t}");
        }
        std::fs::remove_file(&tmp).ok();
    }
}
