//! SQLite schema for the durable extraction journal.

pub const SCHEMA_VERSION: i64 = 3;

pub const SCHEMA_SQL: &str = r#"
CREATE TABLE IF NOT EXISTS meta (
    key   TEXT PRIMARY KEY,
    value TEXT NOT NULL
);

-- Source volumes (archive parts), indexed by position in the volume list.
CREATE TABLE IF NOT EXISTS volumes (
    id                INTEGER PRIMARY KEY,
    path              TEXT NOT NULL UNIQUE,
    identity_json     TEXT,
    allocated_before  INTEGER NOT NULL DEFAULT 0,
    logical_size      INTEGER NOT NULL DEFAULT 0,
    is_first          INTEGER NOT NULL DEFAULT 0,
    structural_digest TEXT
);

-- Recovery units in processing order.
CREATE TABLE IF NOT EXISTS recovery_units (
    id          INTEGER PRIMARY KEY,
    seq         INTEGER NOT NULL UNIQUE,
    state       TEXT NOT NULL,
    first_entry INTEGER NOT NULL,
    last_entry  INTEGER NOT NULL,
    error       TEXT,
    updated_at  TEXT NOT NULL
);

-- Archive entries.
CREATE TABLE IF NOT EXISTS entries (
    id                    INTEGER PRIMARY KEY,
    index_in_archive      INTEGER NOT NULL UNIQUE,
    name                  TEXT NOT NULL,
    packed_size           INTEGER NOT NULL,
    unpacked_size         INTEGER NOT NULL,
    crc32                 INTEGER,
    is_directory          INTEGER NOT NULL,
    is_solid              INTEGER NOT NULL,
    split_before          INTEGER NOT NULL DEFAULT 0,
    split_after           INTEGER NOT NULL DEFAULT 0,
    encrypted             INTEGER NOT NULL DEFAULT 0,
    recovery_unit         INTEGER NOT NULL REFERENCES recovery_units(id),
    final_path            TEXT,
    partial_path          TEXT,
    blake3                TEXT,
    status                TEXT NOT NULL,
    actual_committed_path TEXT,
    existed_before_job    INTEGER NOT NULL DEFAULT 0,
    expected_digest       TEXT,
    is_redirection        INTEGER NOT NULL DEFAULT 0,
    redirection_kind      TEXT
);
CREATE INDEX IF NOT EXISTS idx_entries_unit ON entries(recovery_unit);

-- Packed source ranges (the bytes the source file allocates for each unit).
CREATE TABLE IF NOT EXISTS packed_ranges (
    id                        INTEGER PRIMARY KEY,
    volume_index              INTEGER NOT NULL REFERENCES volumes(id),
    start                     INTEGER NOT NULL,
    len                       INTEGER NOT NULL,
    state                     TEXT NOT NULL,
    recovery_unit             INTEGER REFERENCES recovery_units(id),
    physically_released_bytes INTEGER NOT NULL DEFAULT 0,
    blake3_digest             TEXT
);
CREATE INDEX IF NOT EXISTS idx_ranges_vol ON packed_ranges(volume_index);
CREATE INDEX IF NOT EXISTS idx_ranges_unit ON packed_ranges(recovery_unit);

-- State transitions (audit trail).
CREATE TABLE IF NOT EXISTS state_transitions (
    id         INTEGER PRIMARY KEY,
    unit_seq   INTEGER NOT NULL,
    from_state TEXT NOT NULL,
    to_state   TEXT NOT NULL,
    at         TEXT NOT NULL
);

-- Recorded errors.
CREATE TABLE IF NOT EXISTS errors (
    id                 INTEGER PRIMARY KEY,
    at                 TEXT NOT NULL,
    operation          TEXT NOT NULL,
    message            TEXT NOT NULL,
    os_error           INTEGER,
    recovery_state     TEXT NOT NULL,
    recommended_action TEXT NOT NULL
);

-- Job-level single-row metadata.
CREATE TABLE IF NOT EXISTS job_meta (
    id                 INTEGER PRIMARY KEY CHECK (id = 1),
    job_id             TEXT NOT NULL,
    created_at         TEXT NOT NULL,
    updated_at         TEXT NOT NULL,
    archive_path       TEXT NOT NULL,
    destination        TEXT NOT NULL,
    archive_fingerprint TEXT,
    safety_mode        TEXT NOT NULL,
    settings_json      TEXT NOT NULL,
    current_unit       INTEGER NOT NULL DEFAULT 0,
    job_state          TEXT NOT NULL
);
"#;

/// Run migrations (applies base schema and adds provenance / physical reclamation columns).
pub fn migrate(conn: &rusqlite::Connection) -> rusqlite::Result<()> {
    conn.execute_batch(SCHEMA_SQL)?;

    // Check for provenance columns in entries for backward compatibility with v1 journals
    let mut stmt = conn.prepare("PRAGMA table_info(entries)")?;
    let mut rows = stmt.query([])?;
    let mut has_actual_committed = false;
    let mut has_existed_before = false;
    let mut has_expected_digest = false;
    let mut has_is_redirection = false;
    let mut has_redirection_kind = false;

    while let Some(row) = rows.next()? {
        let col_name: String = row.get(1)?;
        match col_name.as_str() {
            "actual_committed_path" => has_actual_committed = true,
            "existed_before_job" => has_existed_before = true,
            "expected_digest" => has_expected_digest = true,
            "is_redirection" => has_is_redirection = true,
            "redirection_kind" => has_redirection_kind = true,
            _ => {}
        }
    }
    drop(rows);
    drop(stmt);

    if !has_actual_committed {
        conn.execute(
            "ALTER TABLE entries ADD COLUMN actual_committed_path TEXT",
            [],
        )?;
    }
    if !has_existed_before {
        conn.execute(
            "ALTER TABLE entries ADD COLUMN existed_before_job INTEGER NOT NULL DEFAULT 0",
            [],
        )?;
    }
    if !has_expected_digest {
        conn.execute("ALTER TABLE entries ADD COLUMN expected_digest TEXT", [])?;
    }
    if !has_is_redirection {
        conn.execute(
            "ALTER TABLE entries ADD COLUMN is_redirection INTEGER NOT NULL DEFAULT 0",
            [],
        )?;
    }
    if !has_redirection_kind {
        conn.execute("ALTER TABLE entries ADD COLUMN redirection_kind TEXT", [])?;
    }

    // Check for physical reclamation and blake3_digest columns in packed_ranges
    let mut stmt_r = conn.prepare("PRAGMA table_info(packed_ranges)")?;
    let mut rows_r = stmt_r.query([])?;
    let mut has_physically_released = false;
    let mut has_blake3_digest = false;
    while let Some(row) = rows_r.next()? {
        let col_name: String = row.get(1)?;
        if col_name == "physically_released_bytes" {
            has_physically_released = true;
        } else if col_name == "blake3_digest" {
            has_blake3_digest = true;
        }
    }
    drop(rows_r);
    drop(stmt_r);

    if !has_physically_released {
        conn.execute(
            "ALTER TABLE packed_ranges ADD COLUMN physically_released_bytes INTEGER NOT NULL DEFAULT 0",
            [],
        )?;
    }
    if !has_blake3_digest {
        conn.execute(
            "ALTER TABLE packed_ranges ADD COLUMN blake3_digest TEXT",
            [],
        )?;
    }

    // Check for structural_digest column in volumes
    let mut stmt_v = conn.prepare("PRAGMA table_info(volumes)")?;
    let mut rows_v = stmt_v.query([])?;
    let mut has_structural_digest = false;
    while let Some(row) = rows_v.next()? {
        let col_name: String = row.get(1)?;
        if col_name == "structural_digest" {
            has_structural_digest = true;
            break;
        }
    }
    drop(rows_v);
    drop(stmt_v);

    if !has_structural_digest {
        conn.execute("ALTER TABLE volumes ADD COLUMN structural_digest TEXT", [])?;
    }

    conn.execute(
        "INSERT OR REPLACE INTO meta(key, value) VALUES ('schema_version', ?1)",
        rusqlite::params![SCHEMA_VERSION.to_string()],
    )?;
    Ok(())
}
