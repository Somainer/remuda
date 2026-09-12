//! Content-addressed blob store for raw observation bytes.

use crate::util::{digest_hex, digest_of};
use crate::{Error, FsyncPolicy, RawPayload};
use remuda_protocol::{Id, RawRef, U64};
use rusqlite::{Connection, OptionalExtension, params};
use std::fs::{self, File};
use std::io::Write;
use std::path::{Path, PathBuf};
use uuid::Uuid;

pub(crate) struct BlobStore {
    dir: PathBuf,
}

impl BlobStore {
    pub(crate) fn new(data_dir: &Path) -> Result<Self, Error> {
        let dir = data_dir.join("blobs");
        fs::create_dir_all(&dir)?;
        Ok(Self { dir })
    }

    /// Write bytes to disk first, then index them in the caller's transaction.
    pub(crate) fn put(
        &self,
        conn: &Connection,
        raw: &RawPayload,
        fsync: FsyncPolicy,
    ) -> Result<RawRef, Error> {
        let digest = digest_of(&raw.bytes);
        let digest_s: String = digest.clone().into();
        if let Some((object_id, length)) = conn
            .query_row(
                "SELECT object_id, length FROM blobs WHERE digest = ?1",
                rusqlite::params![digest_s],
                |row| Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?)),
            )
            .optional()?
        {
            return Ok(RawRef {
                object_id: Id::try_from(object_id)?,
                offset: U64(0),
                length: U64(length as u64),
                digest,
                media_type: raw.media_type.clone(),
                redaction: raw.redaction,
            });
        }

        let object_id = Id::new("obj")?;
        let hex = digest_hex(&digest);
        let dest = self.dir.join(&hex);
        if !dest.exists() {
            let tmp = self.dir.join(format!(".{hex}.{}.tmp", Uuid::now_v7()));
            {
                let mut file = File::create(&tmp)?;
                file.write_all(&raw.bytes)?;
                match fsync {
                    FsyncPolicy::Never => {}
                    FsyncPolicy::Data => file.sync_data()?,
                    FsyncPolicy::All => file.sync_all()?,
                }
            }
            fs::rename(&tmp, &dest)?;
        }

        conn.execute(
            "INSERT INTO blobs (object_id, digest, length, media_type) VALUES (?1, ?2, ?3, ?4)",
            params![
                object_id.as_str(),
                digest_s,
                raw.bytes.len() as i64,
                raw.media_type
            ],
        )?;

        Ok(RawRef {
            object_id,
            offset: U64(0),
            length: U64(raw.bytes.len() as u64),
            digest,
            media_type: raw.media_type.clone(),
            redaction: raw.redaction,
        })
    }
}
