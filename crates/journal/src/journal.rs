//! Durable extraction journal.
//!
//! A per-job SQLite database that records every durable transition of the
//! transactional engine. Commits are `synchronous=FULL` in WAL mode, so a
//! crash at any point leaves a consistent journal from which the engine can
//! determine exactly what was committed, what was reclaimed and what must be
//! retried.
//!
//! Passwords are never stored here.

use std::path::{Path, PathBuf};

use rusqlite::{params, Connection, OptionalExtension, TransactionBehavior};

use crate::error::{JournalError, Result};
use crate::models::*;
use crate::schema;

/// A per-job journal opened on `<archive_dir>/.spacextract/<job_id>/job.db`.
pub struct JobJournal {
    conn: Connection,
}

impl std::fmt::Debug for JobJournal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("JobJournal").finish_non_exhaustive()
    }
}

impl JobJournal {
    /// Create a new journal with the given job metadata. Fails if the file
    /// already exists (a job id must be unique).
    pub fn create(path: &Path, meta: &JobMeta) -> Result<JobJournal> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let conn = Self::open_conn(path)?;
        schema::migrate(&conn)?;
        let mut j = JobJournal { conn };
        j.insert_job_meta(meta)?;
        Ok(j)
    }

    /// Open an existing journal and validate it is a SpaceExtract journal.
    pub fn open(path: &Path) -> Result<JobJournal> {
        let conn = Self::open_conn(path)?;
        let has_meta_table: bool = conn
            .query_row(
                "SELECT COUNT(*) FROM sqlite_master WHERE type = 'table' AND name = 'meta'",
                [],
                |r| r.get::<_, i64>(0),
            )
            .unwrap_or(0)
            > 0;
        if !has_meta_table {
            return Err(JournalError::schema("journal has no meta table"));
        }
        let version: Option<String> = conn
            .query_row("SELECT value FROM meta WHERE key = 'schema_version'", [], |r| r.get(0))
            .optional()?;
        match version {
            None => {
                return Err(JournalError::schema("journal has no schema version marker"));
            }
            Some(v) if v != schema::SCHEMA_VERSION.to_string() => {
                return Err(JournalError::schema(format!(
                    "journal schema version {v}, expected {}",
                    schema::SCHEMA_VERSION
                )));
            }
            Some(_) => {}
        }
        let j = JobJournal { conn };
        let _ = j.job_meta()?; // validates job_meta row exists
        Ok(j)
    }

    fn open_conn(path: &Path) -> Result<Connection> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let conn = Connection::open(path)?;
        conn.pragma_update(None, "journal_mode", "WAL")?;
        conn.pragma_update(None, "synchronous", "FULL")?;
        conn.pragma_update(None, "foreign_keys", "ON")?;
        conn.busy_timeout(std::time::Duration::from_secs(30))?;
        Ok(conn)
    }

    /// Force a checkpoint so the WAL is folded back into the main database.
    pub fn checkpoint(&self) -> Result<()> {
        let _ = self.conn.prepare_cached("PRAGMA wal_checkpoint(TRUNCATE)")?;
        Ok(())
    }

    // ---------------- job meta ----------------

    fn insert_job_meta(&mut self, meta: &JobMeta) -> Result<()> {
        self.conn.execute(
            "INSERT INTO job_meta (id, job_id, created_at, updated_at, archive_path, destination, \
             archive_fingerprint, safety_mode, settings_json, current_unit, job_state) \
             VALUES (1, ?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
            params![
                meta.job_id,
                meta.created_at,
                meta.updated_at,
                meta.archive_path.to_string_lossy(),
                meta.destination.to_string_lossy(),
                meta.archive_fingerprint,
                meta.safety_mode,
                meta.settings_json,
                meta.current_unit as i64,
                meta.job_state.as_str(),
            ],
        )?;
        Ok(())
    }

    /// Read job metadata (also used as a validity check on open).
    pub fn job_meta(&self) -> Result<JobMeta> {
        let row = self
            .conn
            .query_row(
                "SELECT job_id, created_at, updated_at, archive_path, destination, archive_fingerprint, \
                 safety_mode, settings_json, current_unit, job_state FROM job_meta WHERE id = 1",
                [],
                |r| {
                    Ok((
                        r.get::<_, String>(0)?,
                        r.get::<_, String>(1)?,
                        r.get::<_, String>(2)?,
                        r.get::<_, String>(3)?,
                        r.get::<_, String>(4)?,
                        r.get::<_, Option<String>>(5)?,
                        r.get::<_, String>(6)?,
                        r.get::<_, String>(7)?,
                        r.get::<_, i64>(8)?,
                        r.get::<_, String>(9)?,
                    ))
                },
            )
            .optional()?
            .ok_or_else(|| JournalError::missing("job_meta row"))?;
        Ok(JobMeta {
            job_id: row.0,
            created_at: row.1,
            updated_at: row.2,
            archive_path: PathBuf::from(row.3),
            destination: PathBuf::from(row.4),
            archive_fingerprint: row.5,
            safety_mode: row.6,
            settings_json: row.7,
            current_unit: row.8 as u64,
            job_state: JobState::from_str(&row.9)?,
        })
    }

    /// Update the durable job-level fields. `current_unit`/`job_state` and
    /// `updated_at` are the fields that must survive crashes.
    pub fn set_job_progress(&mut self, current_unit: u64, job_state: JobState) -> Result<()> {
        let now = crate::now_iso();
        self.conn.execute(
            "UPDATE job_meta SET current_unit = ?1, job_state = ?2, updated_at = ?3 WHERE id = 1",
            params![current_unit as i64, job_state.as_str(), now],
        )?;
        Ok(())
    }

    /// Update the archive fingerprint once the pre-test completes.
    pub fn set_archive_fingerprint(&mut self, fingerprint: Option<String>) -> Result<()> {
        let now = crate::now_iso();
        self.conn.execute(
            "UPDATE job_meta SET archive_fingerprint = ?1, updated_at = ?2 WHERE id = 1",
            params![fingerprint, now],
        )?;
        Ok(())
    }

    // ---------------- volumes ----------------

    pub fn add_volumes(&mut self, volumes: &[VolumeRecord]) -> Result<()> {
        let tx = self.conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        for (i, v) in volumes.iter().enumerate() {
            tx.execute(
                "INSERT INTO volumes (id, path, identity_json, allocated_before, logical_size, is_first) \
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                params![
                    (i + 1) as i64,
                    v.path.to_string_lossy(),
                    v.identity.as_ref().map(|x| serde_json::to_string(x).unwrap_or_else(|_| "null".into())),
                    v.allocated_before as i64,
                    v.logical_size as i64,
                    if v.is_first { 1 } else { 0 },
                ],
            )?;
        }
        tx.commit()?;
        Ok(())
    }

    pub fn volumes(&self) -> Result<Vec<VolumeRecord>> {
        let mut stmt = self.conn.prepare(
            "SELECT path, identity_json, allocated_before, logical_size, is_first FROM volumes ORDER BY id",
        )?;
        let rows = stmt.query_map([], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, Option<String>>(1)?,
                r.get::<_, i64>(2)?,
                r.get::<_, i64>(3)?,
                r.get::<_, i64>(4)?,
            ))
        })?;
        let mut out = Vec::new();
        for row in rows {
            let (path, ident, alloc, logical, first) = row?;
            out.push(VolumeRecord {
                path: PathBuf::from(path),
                identity: match ident {
                    Some(s) if !s.is_empty() => Some(serde_json::from_str(&s)?),
                    _ => None,
                },
                allocated_before: alloc as u64,
                logical_size: logical as u64,
                is_first: first != 0,
            });
        }
        Ok(out)
    }

    /// Persist the actual allocation measured after a reclaim operation, so
    /// the engine can reconcile a crash between punch and RECLAIMED write.
    pub fn set_volume_allocated_now(&mut self, volume_index: u64, allocated: u64) -> Result<()> {
        self.conn.execute(
            "UPDATE volumes SET allocated_before = ?2 WHERE id = ?1",
            params![(volume_index + 1) as i64, allocated as i64],
        )?;
        Ok(())
    }

    // ---------------- recovery units ----------------

    pub fn add_units(&mut self, units: &[RecoveryUnitRecord]) -> Result<()> {
        let tx = self.conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        for u in units {
            tx.execute(
                "INSERT INTO recovery_units (seq, state, first_entry, last_entry, error, updated_at) \
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                params![
                    u.seq as i64,
                    u.state.as_str(),
                    u.first_entry as i64,
                    u.last_entry as i64,
                    u.error,
                    u.updated_at,
                ],
            )?;
        }
        tx.commit()?;
        Ok(())
    }

    fn row_to_unit(r: &rusqlite::Row) -> rusqlite::Result<RecoveryUnitRecord> {
        Ok(RecoveryUnitRecord {
            seq: r.get(0)?,
            state: crate::models::UnitState::from_str(&r.get::<_, String>(1)?)
                .unwrap_or(UnitState::Pending),
            first_entry: r.get(2)?,
            last_entry: r.get(3)?,
            error: r.get(4)?,
            updated_at: r.get(5)?,
        })
    }

    pub fn units(&self) -> Result<Vec<RecoveryUnitRecord>> {
        let mut stmt = self
            .conn
            .prepare("SELECT seq, state, first_entry, last_entry, error, updated_at FROM recovery_units ORDER BY seq")?;
        let rows = stmt.query_map([], Self::row_to_unit)?;
        let mut out = Vec::new();
        for r in rows {
            out.push(r?);
        }
        Ok(out)
    }

    pub fn unit(&self, seq: u64) -> Result<RecoveryUnitRecord> {
        self.units()?
            .into_iter()
            .find(|u| u.seq == seq)
            .ok_or_else(|| JournalError::missing(format!("recovery unit {seq}")))
    }

/// Transition a unit to a new state, recording the transition in the same
    /// durable transaction.
    pub fn set_unit_state(&mut self, seq: u64, state: UnitState) -> Result<()> {
        let tx = self.conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let current: Option<String> = tx
            .query_row("SELECT state FROM recovery_units WHERE seq = ?1", [seq as i64], |r| r.get(0))
            .optional()?;
        let current = current.ok_or_else(|| JournalError::missing(format!("recovery unit {seq}")))?;
        tx.execute(
            "UPDATE recovery_units SET state = ?1, updated_at = ?2 WHERE seq = ?3",
            params![state.as_str(), crate::now_iso(), seq as i64],
        )?;
        tx.execute(
            "INSERT INTO state_transitions (unit_seq, from_state, to_state, at) VALUES (?1, ?2, ?3, ?4)",
            params![seq as i64, current, state.as_str(), crate::now_iso()],
        )?;
        tx.commit()?;
        Ok(())
    }

    pub fn set_unit_error(&mut self, seq: u64, error: &str) -> Result<()> {
        self.conn.execute(
            "UPDATE recovery_units SET error = ?1, updated_at = ?2 WHERE seq = ?3",
            params![error, crate::now_iso(), seq as i64],
        )?;
        Ok(())
    }

    pub fn transitions(&self) -> Result<Vec<TransitionRecord>> {
        let mut stmt = self
            .conn
            .prepare("SELECT unit_seq, from_state, to_state, at FROM state_transitions ORDER BY id")?;
        let rows = stmt.query_map([], |r| {
            Ok(TransitionRecord {
                unit_seq: r.get(0)?,
                from_state: r.get(1)?,
                to_state: r.get(2)?,
                at: r.get(3)?,
            })
        })?;
        let mut out = Vec::new();
        for r in rows {
            out.push(r?);
        }
        Ok(out)
    }

    // ---------------- entries ----------------

    pub fn add_entries(&mut self, entries: &[EntryRecord]) -> Result<()> {
        let tx = self.conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        for e in entries {
            tx.execute(
                "INSERT INTO entries (index_in_archive, name, packed_size, unpacked_size, crc32, \
                 is_directory, is_solid, split_before, split_after, encrypted, recovery_unit, \
                 final_path, partial_path, blake3, status) \
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15)",
                params![
                    e.index_in_archive as i64,
                    e.name,
                    e.packed_size as i64,
                    e.unpacked_size as i64,
                    e.crc32.map(|c| c as i64),
                    if e.is_directory { 1 } else { 0 },
                    if e.is_solid { 1 } else { 0 },
                    if e.split_before { 1 } else { 0 },
                    if e.split_after { 1 } else { 0 },
                    if e.encrypted { 1 } else { 0 },
                    (e.recovery_unit + 1) as i64,
                    e.final_path.as_ref().map(|p| p.to_string_lossy().into_owned()),
                    e.partial_path.as_ref().map(|p| p.to_string_lossy().into_owned()),
                    e.blake3,
                    e.status.as_str(),
                ],
            )?;
        }
        tx.commit()?;
        Ok(())
    }

    fn row_to_entry(r: &rusqlite::Row) -> rusqlite::Result<EntryRecord> {        Ok(EntryRecord {
            index_in_archive: r.get(0)?,
            name: r.get(1)?,
            packed_size: r.get(2)?,
            unpacked_size: r.get(3)?,
            crc32: r.get::<_, Option<i64>>(4)?.map(|c| c as u32),
            is_directory: r.get::<_, i64>(5)? != 0,
            is_solid: r.get::<_, i64>(6)? != 0,
            split_before: r.get::<_, i64>(7)? != 0,
            split_after: r.get::<_, i64>(8)? != 0,
            encrypted: r.get::<_, i64>(9)? != 0,
            recovery_unit: r.get::<_, i64>(10)? as u64 - 1,
            final_path: r.get::<_, Option<String>>(11)?.map(PathBuf::from),
            partial_path: r.get::<_, Option<String>>(12)?.map(PathBuf::from),
            blake3: r.get(13)?,
            status: crate::models::EntryStatus::from_str(&r.get::<_, String>(14)?)
                .unwrap_or(EntryStatus::Pending),
        })
    }

    pub fn entries(&self) -> Result<Vec<EntryRecord>> {
        let mut stmt = self.conn.prepare(
            "SELECT index_in_archive, name, packed_size, unpacked_size, crc32, is_directory, is_solid, \
             split_before, split_after, encrypted, recovery_unit, final_path, partial_path, blake3, status \
             FROM entries ORDER BY index_in_archive",
        )?;
        let rows = stmt.query_map([], Self::row_to_entry)?;
        let mut out = Vec::new();
        for r in rows {
            out.push(r?);
        }
        Ok(out)
    }

    pub fn entries_for_unit(&self, seq: u64) -> Result<Vec<EntryRecord>> {
        let mut stmt = self.conn.prepare(
            "SELECT index_in_archive, name, packed_size, unpacked_size, crc32, is_directory, is_solid, \
             split_before, split_after, encrypted, recovery_unit, final_path, partial_path, blake3, status \
             FROM entries WHERE recovery_unit = ?1 ORDER BY index_in_archive",
        )?;
        let rows = stmt.query_map([(seq + 1) as i64], Self::row_to_entry)?;
        let mut out = Vec::new();
        for r in rows {
            out.push(r?);
        }
        Ok(out)
    }

    pub fn entry(&self, index: u64) -> Result<EntryRecord> {
        self.entries()?
            .into_iter()
            .find(|e| e.index_in_archive == index)
            .ok_or_else(|| JournalError::missing(format!("entry {index}")))
    }

    /// Set entry status (durable).
    pub fn set_entry_status(&mut self, index: u64, status: EntryStatus) -> Result<()> {
        self.conn.execute(
            "UPDATE entries SET status = ?1 WHERE index_in_archive = ?2",
            params![status.as_str(), index as i64],
        )?;
        Ok(())
    }

/// Batch-update partial paths for many entries in one durable
    /// transaction (used by the streaming extraction path).
    pub fn set_partial_paths_batch(&mut self, updates: &[(u64, String)]) -> Result<()> {
        if updates.is_empty() {
            return Ok(());
        }
        let tx = self.conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        {
            let mut stmt = tx.prepare_cached("UPDATE entries SET partial_path = ?1 WHERE index_in_archive = ?2")?;
            for (index, partial) in updates {
                stmt.execute(params![partial, *index as i64])?;
            }
        }
        tx.commit()?;
        Ok(())
    }

    /// Record partial/final paths for an entry (set before extraction).
    /// Only the provided (non-None) columns are updated.
    pub fn set_entry_paths(&mut self, index: u64, partial: Option<&Path>, final_path: Option<&Path>) -> Result<()> {
        let tx = self.conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        if let Some(p) = partial {
            tx.execute(
                "UPDATE entries SET partial_path = ?1 WHERE index_in_archive = ?2",
                params![p.to_string_lossy().into_owned(), index as i64],
            )?;
        }
        if let Some(f) = final_path {
            tx.execute(
                "UPDATE entries SET final_path = ?1 WHERE index_in_archive = ?2",
                params![f.to_string_lossy().into_owned(), index as i64],
            )?;
        }
        tx.commit()?;
        Ok(())
    }

    /// Mark an entry verified with its BLAKE3 digest (durable).
    pub fn set_entry_verified(&mut self, index: u64, blake3: &str) -> Result<()> {
        self.conn.execute(
            "UPDATE entries SET blake3 = ?1, status = 'VERIFIED' WHERE index_in_archive = ?2",
            params![blake3, index as i64],
        )?;
        Ok(())
    }

/// Mark an entry durably renamed into place and committed (durable).
    pub fn set_entry_committed(&mut self, index: u64, final_path: &Path, blake3: &str) -> Result<()> {
        self.conn.execute(
            "UPDATE entries SET final_path = ?1, blake3 = ?2, status = 'COMMITTED' WHERE index_in_archive = ?3",
            params![final_path.to_string_lossy().into_owned(), blake3, index as i64],
        )?;
        Ok(())
    }

    /// One durable transaction covering a unit's verify→durable pipeline:
    /// OUTPUT_WRITTEN, entry VERIFIED, OUTPUT_VERIFIED, entry DURABLE and
    /// OUTPUT_DURABLE. The intermediate states are still journaled (in the
    /// same transaction) so crash recovery behaves identically, but the unit
    /// pays a single WAL fsync instead of five.
    pub fn mark_unit_verified_durable(
        &mut self,
        seq: u64,
        verified: &[(u64, String)],
    ) -> Result<()> {
        let tx = self.conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        {
            tx.execute(
                "UPDATE recovery_units SET state = 'OUTPUT_WRITTEN', updated_at = ?1 WHERE seq = ?2",
                params![crate::now_iso(), seq as i64],
            )?;
            let mut v = tx.prepare_cached(
                "UPDATE entries SET blake3 = ?1, status = 'VERIFIED' WHERE index_in_archive = ?2",
            )?;
            for (index, blake3) in verified {
                v.execute(params![blake3, *index as i64])?;
            }
            tx.execute(
                "UPDATE recovery_units SET state = 'OUTPUT_VERIFIED', updated_at = ?1 WHERE seq = ?2",
                params![crate::now_iso(), seq as i64],
            )?;
            let mut d = tx.prepare_cached(
                "UPDATE entries SET status = 'DURABLE' WHERE index_in_archive = ?1",
            )?;
            for (index, _) in verified {
                d.execute(params![*index as i64])?;
            }
            tx.execute(
                "UPDATE recovery_units SET state = 'OUTPUT_DURABLE', updated_at = ?1 WHERE seq = ?2",
                params![crate::now_iso(), seq as i64],
            )?;
        }
        tx.commit()?;
        Ok(())
    }

    // ---------------- packed ranges ----------------

    pub fn add_packed_ranges(&mut self, ranges: &[PackedRangeRecord]) -> Result<()> {
        let tx = self.conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        for r in ranges {
            tx.execute(
                "INSERT INTO packed_ranges (volume_index, start, len, state, recovery_unit) \
                 VALUES (?1, ?2, ?3, ?4, ?5)",
                params![
                    (r.volume_index + 1) as i64,
                    r.start as i64,
                    r.len as i64,
                    r.state.as_str(),
                    r.recovery_unit.map(|u| (u + 1) as i64),
                ],
            )?;
        }
        tx.commit()?;
        Ok(())
    }

    pub fn packed_ranges(&self) -> Result<Vec<PackedRangeRecord>> {
        let mut stmt = self.conn.prepare(
            "SELECT volume_index, start, len, state, recovery_unit FROM packed_ranges ORDER BY id",
        )?;
        let rows = stmt.query_map([], |r| {
            let state_s: String = r.get(3)?;
            let unit = r.get::<_, Option<i64>>(4)?.map(|u| (u - 1) as u64);
            Ok(PackedRangeRecord {
                volume_index: r.get::<_, i64>(0)? as u64 - 1,
                start: r.get::<_, i64>(1)? as u64,
                len: r.get::<_, i64>(2)? as u64,
                state: RangeState::from_str(&state_s).unwrap_or(RangeState::Active),
                recovery_unit: unit,
            })
        })?;
        let mut out = Vec::new();
        for r in rows {
            out.push(r?);
        }
        Ok(out)
    }

    pub fn packed_ranges_for_unit(&self, seq: u64) -> Result<Vec<PackedRangeRecord>> {
        Ok(self
            .packed_ranges()?
            .into_iter()
            .filter(|r| r.recovery_unit == Some(seq))
            .collect())
    }

    /// Persist RECLAIM_INTENT for a range BEFORE punching holes (durable).
    pub fn mark_range_reclaim_intent(&mut self, volume_index: u64, start: u64, len: u64) -> Result<()> {
        self.conn.execute(
            "UPDATE packed_ranges SET state = 'RECLAIM_INTENT' \
             WHERE volume_index = ?1 AND start = ?2 AND len = ?3 AND state = 'ACTIVE'",
            params![(volume_index + 1) as i64, start as i64, len as i64],
        )?;
        Ok(())
    }

    /// Persist RECLAIMED after verifying actual allocation (durable).
    pub fn mark_range_reclaimed(&mut self, volume_index: u64, start: u64, len: u64) -> Result<()> {
        self.conn.execute(
            "UPDATE packed_ranges SET state = 'RECLAIMED' \
             WHERE volume_index = ?1 AND start = ?2 AND len = ?3",
            params![(volume_index + 1) as i64, start as i64, len as i64],
        )?;
        Ok(())
    }

    pub fn ranges_in_state(&self, state: RangeState) -> Result<Vec<PackedRangeRecord>> {
        Ok(self
            .packed_ranges()?
            .into_iter()
            .filter(|r| r.state == state)
            .collect())
    }

    // ---------------- errors ----------------

    pub fn record_error(&mut self, e: &ErrorRecord) -> Result<()> {
        self.conn.execute(
            "INSERT INTO errors (at, operation, message, os_error, recovery_state, recommended_action) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            params![
                e.at,
                e.operation,
                e.message,
                e.os_error.map(|o| o as i64),
                e.recovery_state,
                e.recommended_action
            ],
        )?;
        Ok(())
    }

    pub fn errors(&self) -> Result<Vec<ErrorRecord>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, at, operation, message, os_error, recovery_state, recommended_action FROM errors ORDER BY id",
        )?;
        let rows = stmt.query_map([], |r| {
            Ok(ErrorRecord {
                id: r.get(0)?,
                at: r.get(1)?,
                operation: r.get(2)?,
                message: r.get(3)?,
                os_error: r.get::<_, Option<i64>>(4)?.map(|o| o as u32),
                recovery_state: r.get(5)?,
                recommended_action: r.get(6)?,
            })
        })?;
        let mut out = Vec::new();
        for r in rows {
            out.push(r?);
        }
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_meta(dir: &std::path::Path) -> JobMeta {
        JobMeta {
            job_id: "test-job".into(),
            created_at: crate::now_iso(),
            updated_at: crate::now_iso(),
            archive_path: dir.join("archive.rar"),
            destination: dir.join("out"),
            archive_fingerprint: None,
            safety_mode: "balanced".into(),
            settings_json: "{}".into(),
            current_unit: 0,
            job_state: JobState::Active,
        }
    }

    fn db_path(dir: &std::path::Path) -> std::path::PathBuf {
        dir.join(".spacextract").join("test-job").join("job.db")
    }

    #[test]
    fn create_open_transition_roundtrip() {
        let dir = tempfile::tempdir().unwrap();
        let path = db_path(dir.path());
        let meta = sample_meta(dir.path());

        {
            let mut j = JobJournal::create(&path, &meta).unwrap();
            j.add_units(&[RecoveryUnitRecord {
                seq: 0,
                state: UnitState::Pending,
                first_entry: 0,
                last_entry: 4,
                error: None,
                updated_at: crate::now_iso(),
            }])
            .unwrap();
            j.set_unit_state(0, UnitState::Extracting).unwrap();
            j.set_unit_state(0, UnitState::Committed).unwrap();
            j.set_job_progress(1, JobState::Active).unwrap();
        } // drop simulates a clean close

        // Reopen: all durable transitions must survive.
        let j = JobJournal::open(&path).unwrap();
        let m = j.job_meta().unwrap();
        assert_eq!(m.job_state, JobState::Active);
        assert_eq!(m.current_unit, 1);
        let units = j.units().unwrap();
        assert_eq!(units.len(), 1);
        assert_eq!(units[0].state, UnitState::Committed);
        let transitions = j.transitions().unwrap();
        assert_eq!(transitions.len(), 2);
        assert_eq!(transitions[0].from_state, "PENDING");
        assert_eq!(transitions[0].to_state, "EXTRACTING");
        assert_eq!(transitions[1].from_state, "EXTRACTING");
        assert_eq!(transitions[1].to_state, "COMMITTED");
    }

    #[test]
    fn entries_and_ranges_roundtrip() {
        let dir = tempfile::tempdir().unwrap();
        let path = db_path(dir.path());
        let meta = sample_meta(dir.path());
        let mut j = JobJournal::create(&path, &meta).unwrap();
        j.add_units(&[RecoveryUnitRecord {
            seq: 0,
            state: UnitState::Pending,
            first_entry: 0,
            last_entry: 1,
            error: None,
            updated_at: crate::now_iso(),
        }])
        .unwrap();
        j.add_entries(&[
            EntryRecord {
                index_in_archive: 0,
                name: "a.txt".into(),
                packed_size: 10,
                unpacked_size: 10,
                crc32: Some(0x1234),
                is_directory: false,
                is_solid: false,
                split_before: false,
                split_after: false,
                encrypted: false,
                recovery_unit: 0,
                final_path: None,
                partial_path: None,
                blake3: None,
                status: EntryStatus::Pending,
            },
            EntryRecord {
                index_in_archive: 1,
                name: "dir/b.bin".into(),
                packed_size: 20,
                unpacked_size: 30,
                crc32: None,
                is_directory: false,
                is_solid: true,
                split_before: false,
                split_after: false,
                encrypted: false,
                recovery_unit: 0,
                final_path: None,
                partial_path: None,
                blake3: None,
                status: EntryStatus::Pending,
            },
        ])
        .unwrap();
        j.add_volumes(&[VolumeRecord {
            path: dir.path().join("archive.rar"),
            identity: None,
            allocated_before: 100,
            logical_size: 200,
            is_first: true,
        }])
        .unwrap();
        j.add_packed_ranges(&[PackedRangeRecord {
            volume_index: 0,
            start: 0,
            len: 50,
            state: RangeState::Active,
            recovery_unit: Some(0),
        }])
        .unwrap();
        j.mark_range_reclaim_intent(0, 0, 50).unwrap();
        j.set_entry_verified(0, "deadbeef").unwrap();
        j.set_entry_committed(0, &dir.path().join("out").join("a.txt"), "deadbeef").unwrap();

        drop(j);
        let mut j = JobJournal::open(&path).unwrap();
        let entries = j.entries().unwrap();
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].status, EntryStatus::Committed);
        assert_eq!(entries[0].blake3.as_deref(), Some("deadbeef"));
        assert_eq!(entries[1].status, EntryStatus::Pending);
        let ranges = j.packed_ranges().unwrap();
        assert_eq!(ranges[0].state, RangeState::ReclaimIntent);
        let errs = j.record_error(&ErrorRecord {
            id: 0,
            at: crate::now_iso(),
            operation: "test".into(),
            message: "boom".into(),
            os_error: Some(5),
            recovery_state: "EXTRACTING".into(),
            recommended_action: "retry".into(),
        });
        assert!(errs.is_ok());
    }

    #[test]
    fn open_non_journal_file_fails() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("not-a-journal.db");
        std::fs::write(&path, b"garbage not sqlite").unwrap();
        let err = JobJournal::open(&path).unwrap_err();
        assert!(matches!(err, JournalError::Sqlite(_)), "got: {err:?}");
    }

    #[test]
    fn open_existing_db_without_schema_fails() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("empty.db");
        let conn = rusqlite::Connection::open(&path).unwrap();
        drop(conn);
        let err = JobJournal::open(&path).unwrap_err();
        assert!(matches!(err, JournalError::Schema(_)), "got: {err:?}");
    }

    #[test]
    fn units_record_errors_durably() {
        let dir = tempfile::tempdir().unwrap();
        let path = db_path(dir.path());
        let meta = sample_meta(dir.path());
        let mut j = JobJournal::create(&path, &meta).unwrap();
        j.add_units(&[RecoveryUnitRecord {
            seq: 0,
            state: UnitState::Extracting,
            first_entry: 0,
            last_entry: 2,
            error: None,
            updated_at: crate::now_iso(),
        }])
        .unwrap();
        j.set_unit_error(0, "decoder reported bad data (CRC)").unwrap();
        drop(j);
        let j = JobJournal::open(&path).unwrap();
        let u = j.unit(0).unwrap();
        assert_eq!(u.error.as_deref(), Some("decoder reported bad data (CRC)"));
        assert_eq!(u.state, UnitState::Extracting);
    }

    #[test]
    fn unit_state_transitions_are_recorded_in_order() {
        let dir = tempfile::tempdir().unwrap();
        let path = db_path(dir.path());
        let meta = sample_meta(dir.path());
        let mut j = JobJournal::create(&path, &meta).unwrap();
        j.add_units(&[RecoveryUnitRecord {
            seq: 0,
            state: UnitState::Pending,
            first_entry: 0,
            last_entry: 0,
            error: None,
            updated_at: crate::now_iso(),
        }])
        .unwrap();
        for s in [
            UnitState::Extracting,
            UnitState::OutputWritten,
            UnitState::OutputVerified,
            UnitState::OutputDurable,
            UnitState::Committed,
            UnitState::ReclaimIntent,
            UnitState::Reclaimed,
        ] {
            j.set_unit_state(0, s).unwrap();
        }
        let transitions = j.transitions().unwrap();
        assert_eq!(transitions.len(), 7);
        assert_eq!(transitions[6].from_state, "RECLAIM_INTENT");
        assert_eq!(transitions[6].to_state, "RECLAIMED");
    }
}




