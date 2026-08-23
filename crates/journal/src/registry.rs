//! Mirrored job registry in application data.
//!
//! A small database that remembers every job, so the app can discover
//! interrupted jobs even when the archive's directory is opened fresh.

use std::path::{Path, PathBuf};

use rusqlite::{params, Connection, OptionalExtension, TransactionBehavior};

use crate::error::Result;

const REGISTRY_SCHEMA: &str = r#"
CREATE TABLE IF NOT EXISTS jobs (
    id           TEXT PRIMARY KEY,
    archive_dir  TEXT NOT NULL,
    job_db_path  TEXT NOT NULL,
    archive      TEXT NOT NULL,
    destination  TEXT NOT NULL,
    created_at   TEXT NOT NULL,
    updated_at   TEXT NOT NULL,
    status       TEXT NOT NULL
);
"#;

/// An entry in the job registry.
#[derive(Debug, Clone)]
pub struct RegistryEntry {
    pub job_id: String,
    pub archive_dir: PathBuf,
    pub job_db_path: PathBuf,
    pub archive: PathBuf,
    pub destination: PathBuf,
    pub created_at: String,
    pub updated_at: String,
    pub status: String,
}

/// The application-data job registry.
pub struct Registry {
    conn: Connection,
}

impl Registry {
    /// Open (creating if needed) the registry at `<app_data>/registry.db`.
    pub fn open(app_data_dir: &Path) -> Result<Registry> {
        std::fs::create_dir_all(app_data_dir)?;
        let conn = Connection::open(app_data_dir.join("registry.db"))?;
        conn.pragma_update(None, "journal_mode", "WAL")?;
        conn.pragma_update(None, "synchronous", "FULL")?;
        conn.execute_batch(REGISTRY_SCHEMA)?;
        Ok(Registry { conn })
    }

    /// The default application data directory for SpaceExtract.
    pub fn default_app_data_dir() -> PathBuf {
        let local = std::env::var_os("LOCALAPPDATA")
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from("."));
        local.join("SpaceExtract")
    }

    /// Register or update a job (durable).
    pub fn upsert(&mut self, e: &RegistryEntry) -> Result<()> {
        let tx = self.conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        tx.execute(
            "INSERT INTO jobs (id, archive_dir, job_db_path, archive, destination, created_at, updated_at, status) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8) \
             ON CONFLICT(id) DO UPDATE SET \
               archive_dir = excluded.archive_dir, job_db_path = excluded.job_db_path, \
               archive = excluded.archive, destination = excluded.destination, \
               created_at = excluded.created_at, updated_at = excluded.updated_at, status = excluded.status",
            params![
                e.job_id,
                e.archive_dir.to_string_lossy(),
                e.job_db_path.to_string_lossy(),
                e.archive.to_string_lossy(),
                e.destination.to_string_lossy(),
                e.created_at,
                e.updated_at,
                e.status
            ],
        )?;
        tx.commit()?;
        Ok(())
    }

    /// List all registered jobs.
    pub fn all(&self) -> Result<Vec<RegistryEntry>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, archive_dir, job_db_path, archive, destination, created_at, updated_at, status FROM jobs ORDER BY updated_at DESC",
        )?;
        let rows = stmt.query_map([], |r| {
            Ok(RegistryEntry {
                job_id: r.get(0)?,
                archive_dir: PathBuf::from(r.get::<_, String>(1)?),
                job_db_path: PathBuf::from(r.get::<_, String>(2)?),
                archive: PathBuf::from(r.get::<_, String>(3)?),
                destination: PathBuf::from(r.get::<_, String>(4)?),
                created_at: r.get(5)?,
                updated_at: r.get(6)?,
                status: r.get(7)?,
            })
        })?;
        let mut out = Vec::new();
        for r in rows {
            out.push(r?);
        }
        Ok(out)
    }

    /// Look up one job.
    pub fn get(&self, job_id: &str) -> Result<Option<RegistryEntry>> {
        let row = self
            .conn
            .query_row(
                "SELECT id, archive_dir, job_db_path, archive, destination, created_at, updated_at, status \
                 FROM jobs WHERE id = ?1",
                [job_id],
                |r| {
                    Ok(RegistryEntry {
                        job_id: r.get(0)?,
                        archive_dir: PathBuf::from(r.get::<_, String>(1)?),
                        job_db_path: PathBuf::from(r.get::<_, String>(2)?),
                        archive: PathBuf::from(r.get::<_, String>(3)?),
                        destination: PathBuf::from(r.get::<_, String>(4)?),
                        created_at: r.get(5)?,
                        updated_at: r.get(6)?,
                        status: r.get(7)?,
                    })
                },
            )
            .optional()?;
        Ok(row)
    }

    /// Remove a job from the registry (used when a job is abandoned).
    pub fn remove(&mut self, job_id: &str) -> Result<()> {
        self.conn.execute("DELETE FROM jobs WHERE id = ?1", [job_id])?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn registry_upsert_and_read() {
        let dir = tempfile::tempdir().unwrap();
        let mut reg = Registry::open(dir.path()).unwrap();
        let e = RegistryEntry {
            job_id: "job-1".into(),
            archive_dir: dir.path().join("a"),
            job_db_path: dir.path().join("a").join(".spacextract").join("job-1").join("job.db"),
            archive: dir.path().join("a").join("x.rar"),
            destination: dir.path().join("out"),
            created_at: crate::now_iso(),
            updated_at: crate::now_iso(),
            status: "ACTIVE".into(),
        };
        reg.upsert(&e).unwrap();
        let got = reg.get("job-1").unwrap().unwrap();
        assert_eq!(got.job_id, "job-1");
        assert_eq!(got.archive, e.archive);
        assert_eq!(reg.all().unwrap().len(), 1);
        reg.remove("job-1").unwrap();
        assert!(reg.get("job-1").unwrap().is_none());
    }

    #[test]
    fn registry_reopens() {
        let dir = tempfile::tempdir().unwrap();
        let mut reg = Registry::open(dir.path()).unwrap();
        let e = RegistryEntry {
            job_id: "j".into(),
            archive_dir: dir.path().join("a"),
            job_db_path: dir.path().join("a").join(".spacextract").join("j").join("job.db"),
            archive: dir.path().join("x.rar"),
            destination: dir.path().join("out"),
            created_at: crate::now_iso(),
            updated_at: crate::now_iso(),
            status: "ACTIVE".into(),
        };
        reg.upsert(&e).unwrap();
        drop(reg);
        let reg2 = Registry::open(dir.path()).unwrap();
        assert_eq!(reg2.all().unwrap().len(), 1);
    }
}
