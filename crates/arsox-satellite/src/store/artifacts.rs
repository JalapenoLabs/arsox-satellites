// Copyright © 2026 Jalapeno Labs

//! What each thread's artifacts/ directory held when a turn last ended.
//!
//! The artifact scan is the only writer, and it replaces a thread's rows whole:
//! a file it no longer finds is forgotten, so the same name written again later
//! is announced again as the new file it is. The listing reads the rows as a
//! hash cache and writes nothing, so reading what a thread holds never changes
//! what the next scan announces.

use super::{Store, StoreError};
use sqlx::Row as _;

/// One file as a scan recorded it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RecordedArtifact {
    /// Relative to the thread's artifacts/ directory, `/`-separated.
    pub path: String,
    pub size_bytes: u64,

    /// The inode and change time the hash was taken at. A file that still
    /// matches both, and its size, still has the contents that were hashed.
    pub inode: u64,
    pub changed_nanos: i64,

    pub sha256: String,
    pub content_type: Option<String>,
}

impl Store {
    /// Every file the thread's last scan recorded.
    ///
    /// # Errors
    ///
    /// Returns a database error if the query fails.
    pub async fn recorded_artifacts(
        &self,
        thread_id: &str,
    ) -> Result<Vec<RecordedArtifact>, StoreError> {
        let rows = sqlx::query(
            "SELECT path, size_bytes, inode, changed_at, sha256, content_type
               FROM artifacts
              WHERE thread_id = ?",
        )
        .bind(thread_id)
        .fetch_all(self.pool())
        .await?;

        Ok(rows
            .iter()
            .map(|row| RecordedArtifact {
                path: row.get("path"),
                // SQLite integers are signed. A size or an inode never is, and
                // every one stored here came from a `u64` on the way in.
                size_bytes: row.get::<i64, _>("size_bytes").unsigned_abs(),
                inode: row.get::<i64, _>("inode").cast_unsigned(),
                changed_nanos: row.get("changed_at"),
                sha256: row.get("sha256"),
                content_type: row.get("content_type"),
            })
            .collect())
    }

    /// Replaces the thread's recorded artifacts with what a scan just found.
    ///
    /// One transaction, so a satellite that stops half way leaves the previous
    /// scan's record rather than half of each, and the next scan announces
    /// exactly what changed since the last one that finished.
    ///
    /// # Errors
    ///
    /// Returns a database error if a write fails.
    pub async fn replace_artifacts(
        &self,
        thread_id: &str,
        found: &[RecordedArtifact],
    ) -> Result<(), StoreError> {
        let mut transaction = self.pool().begin().await?;

        sqlx::query("DELETE FROM artifacts WHERE thread_id = ?")
            .bind(thread_id)
            .execute(&mut *transaction)
            .await?;

        for artifact in found {
            sqlx::query(
                "INSERT INTO artifacts
                   (thread_id, path, size_bytes, inode, changed_at, sha256, content_type)
                 VALUES (?, ?, ?, ?, ?, ?, ?)",
            )
            .bind(thread_id)
            .bind(&artifact.path)
            .bind(i64::try_from(artifact.size_bytes).unwrap_or(i64::MAX))
            // Stored bit for bit, since an inode is an identity rather than a
            // quantity and only ever compared for equality.
            .bind(artifact.inode.cast_signed())
            .bind(artifact.changed_nanos)
            .bind(&artifact.sha256)
            .bind(artifact.content_type.as_deref())
            .execute(&mut *transaction)
            .await?;
        }

        transaction.commit().await?;

        Ok(())
    }
}
