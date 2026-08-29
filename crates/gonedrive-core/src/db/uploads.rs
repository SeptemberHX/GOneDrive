//! Resumable upload-session persistence.

use rusqlite::{params, OptionalExtension};
use time::OffsetDateTime;

use crate::error::Result;

use super::{Store, UploadSessionRecord};

impl Store {
    // ---- upload sessions --------------------------------------------------

    pub fn save_upload_session(&self, session: &UploadSessionRecord) -> Result<()> {
        self.with_conn(|conn| {
            conn.execute(
                "INSERT INTO upload_sessions
                   (key, account_id, upload_url, expires_at, committed_offset, total_size,
                    source_fingerprint)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)
                 ON CONFLICT(account_id, key) DO UPDATE SET
                   upload_url = excluded.upload_url,
                   expires_at = excluded.expires_at,
                   committed_offset = excluded.committed_offset,
                   total_size = excluded.total_size,
                   source_fingerprint = excluded.source_fingerprint",
                params![
                    session.key,
                    session.account_id,
                    session.upload_url,
                    session.expires_at.map(|t| t.unix_timestamp()),
                    session.committed_offset as i64,
                    session.total_size as i64,
                    session.source_fingerprint,
                ],
            )?;
            Ok(())
        })
    }

    pub fn upload_session(
        &self,
        account_id: &str,
        key: &str,
    ) -> Result<Option<UploadSessionRecord>> {
        self.with_conn(|conn| {
            Ok(conn
                .query_row(
                    "SELECT key, account_id, upload_url, expires_at, committed_offset, total_size,
                            source_fingerprint
                       FROM upload_sessions WHERE account_id = ?1 AND key = ?2",
                    params![account_id, key],
                    |row| {
                        Ok(UploadSessionRecord {
                            key: row.get(0)?,
                            account_id: row.get(1)?,
                            upload_url: row.get(2)?,
                            expires_at: row
                                .get::<_, Option<i64>>(3)?
                                .and_then(|t| OffsetDateTime::from_unix_timestamp(t).ok()),
                            committed_offset: row.get::<_, i64>(4)? as u64,
                            total_size: row.get::<_, i64>(5)? as u64,
                            source_fingerprint: row.get(6)?,
                        })
                    },
                )
                .optional()?)
        })
    }

    pub fn set_upload_offset(&self, account_id: &str, key: &str, offset: u64) -> Result<()> {
        self.with_conn(|conn| {
            conn.execute(
                "UPDATE upload_sessions
                    SET committed_offset = ?3
                  WHERE account_id = ?1 AND key = ?2",
                params![account_id, key, offset as i64],
            )?;
            Ok(())
        })
    }

    pub fn delete_upload_session(&self, account_id: &str, key: &str) -> Result<()> {
        self.with_conn(|conn| {
            conn.execute(
                "DELETE FROM upload_sessions WHERE account_id = ?1 AND key = ?2",
                params![account_id, key],
            )?;
            Ok(())
        })
    }
}
