//! SQLite schema for the durable extraction journal.

pub const SCHEMA_VERSION: i64 = 1;

pub const SCHEMA_SQL: &str = r#"
CREATE TABLE IF NOT EXISTS meta (
    key   TEXT PRIMARY KEY,
    value TEXT NOT NULL
);

-- Source volumes (archive parts), indexed by position in the volume list.
CREATE TABLE IF NOT EXISTS volumes (
    id              INTEGER PRIMARY KEY,
    path            TEXT NOT NULL UNIQUE,
    identity_json   TEXT,
    allocated_before INTEGER NOT NULL DEFAULT 0,
    logical_size    INTEGER NOT NULL DEFAULT 0,
    is_first        INTEGER NOT NULL DEFAULT 0
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
    id                INTEGER PRIMARY KEY,
    index_in_archive  INTEGER NOT NULL UNIQUE,
    name              TEXT NOT NULL,
    packed_size       INTEGER NOT NULL,
    unpacked_size     INTEGER NOT NULL,
    crc32             INTEGER,
    is_directory      INTEGER NOT NULL,
    is_solid          INTEGER NOT NULL,
    split_before      INTEGER NOT NULL DEFAULT 0,
    split_after       INTEGER NOT NULL DEFAULT 0,
    encrypted         INTEGER NOT NULL DEFAULT 0,
    recovery_unit     INTEGER NOT NULL REFERENCES recovery_units(id),
    final_path        TEXT,
    partial_path      TEXT,
    blake3            TEXT,
    status            TEXT NOT NULL
);
CREATE INDEX IF NOT EXISTS idx_entries_unit ON entries(recovery_unit);

-- Packed source ranges (the bytes the source file allocates for each unit).
CREATE TABLE IF NOT EXISTS packed_ranges (
    id              INTEGER PRIMARY KEY,
    volume_index    INTEGER NOT NULL REFERENCES volumes(id),
    start           INTEGER NOT NULL,
    len             INTEGER NOT NULL,
    state           TEXT NOT NULL,
    recovery_unit   INTEGER REFERENCES recovery_units(id)
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

/// Run migrations (currently: set schema version marker).
pub fn migrate(conn: &rusqlite::Connection) -> rusqlite::Result<()> {
    conn.execute_batch(SCHEMA_SQL)?;
    conn.execute(
        "INSERT OR REPLACE INTO meta(key, value) VALUES ('schema_version', ?1)",
        rusqlite::params![SCHEMA_VERSION.to_string()],
    )?;
    Ok(())
}
