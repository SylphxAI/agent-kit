use std::{
    fs::{self, OpenOptions},
    io::{Read, Write},
    path::{Path, PathBuf},
    time::Duration,
};

use rusqlite::{Connection, OpenFlags, OptionalExtension, TransactionBehavior, params};
use uuid::Uuid;

use super::*;

/// Content-addressed files and a transactional SQLite index in ".artifacts".
/// The workspace root and its contents must be writable only by trusted principals.
/// Use a local filesystem with SQLite locking and atomic file publication semantics.
#[derive(Clone, Debug)]
pub struct LocalArtifactBackend {
    directory: PathBuf,
}

impl LocalArtifactBackend {
    pub fn new(workspace_root: impl AsRef<Path>) -> Result<Self, ArtifactError> {
        fs::create_dir_all(workspace_root.as_ref()).map_err(|_| ArtifactError::Storage)?;
        let directory = workspace_root
            .as_ref()
            .canonicalize()
            .map_err(|_| ArtifactError::Storage)?
            .join(".artifacts");
        ensure_directory(&directory)?;
        ensure_directory(&directory.join("blobs"))?;
        let backend = Self { directory };
        backend
            .connect()?
            .execute_batch(
                "BEGIN IMMEDIATE;
             CREATE TABLE IF NOT EXISTS artifact_heads (
                 id TEXT PRIMARY KEY NOT NULL, version INTEGER NOT NULL,
                 room TEXT, kind TEXT NOT NULL
             );
             CREATE TABLE IF NOT EXISTS artifact_versions (
                 id TEXT NOT NULL REFERENCES artifact_heads(id) ON DELETE CASCADE,
                 version INTEGER NOT NULL, metadata TEXT NOT NULL,
                 PRIMARY KEY (id, version)
             );
             CREATE INDEX IF NOT EXISTS artifact_by_room ON artifact_heads(room, id);
             CREATE INDEX IF NOT EXISTS artifact_by_kind ON artifact_heads(kind, id);
             COMMIT;",
            )
            .map_err(|_| ArtifactError::Storage)?;
        Ok(backend)
    }

    fn connect(&self) -> Result<Connection, ArtifactError> {
        check_directory(&self.directory)?;
        let db = Connection::open_with_flags(
            self.directory.join("index.sqlite3"),
            OpenFlags::SQLITE_OPEN_READ_WRITE
                | OpenFlags::SQLITE_OPEN_CREATE
                | OpenFlags::SQLITE_OPEN_NOFOLLOW,
        )
        .map_err(|_| ArtifactError::Storage)?;
        db.busy_timeout(Duration::from_secs(5))
            .map_err(|_| ArtifactError::Storage)?;
        db.execute_batch("PRAGMA foreign_keys = ON; PRAGMA synchronous = FULL;")
            .map_err(|_| ArtifactError::Storage)?;
        Ok(db)
    }

    fn read_blob(&self, meta: &ArtifactMetadata) -> Result<Vec<u8>, ArtifactError> {
        meta.check().map_err(|_| ArtifactError::Storage)?;
        let directory = self.directory.join("blobs");
        check_directory(&directory)?;
        let path = directory.join(&meta.sha256);
        if !fs::symlink_metadata(&path)
            .map_err(|_| ArtifactError::Storage)?
            .is_file()
        {
            return Err(ArtifactError::Storage);
        }
        let mut options = OpenOptions::new();
        options.read(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK);
        }
        let file = options.open(path).map_err(|_| ArtifactError::Storage)?;
        let mut bytes = Vec::with_capacity(meta.byte_size);
        file.take(meta.byte_size as u64 + 1)
            .read_to_end(&mut bytes)
            .map_err(|_| ArtifactError::Storage)?;
        meta.check_content(&bytes)
            .map_err(|_| ArtifactError::Storage)?;
        Ok(bytes)
    }

    fn write_blob(&self, meta: &ArtifactMetadata, bytes: &[u8]) -> Result<(), ArtifactError> {
        let directory = self.directory.join("blobs");
        check_directory(&directory)?;
        let path = directory.join(&meta.sha256);
        match fs::symlink_metadata(&path) {
            Ok(_) => {
                self.read_blob(meta)?;
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                let mut temp = tempfile::NamedTempFile::new_in(&directory)
                    .map_err(|_| ArtifactError::Storage)?;
                temp.write_all(bytes).map_err(|_| ArtifactError::Storage)?;
                temp.as_file()
                    .sync_all()
                    .map_err(|_| ArtifactError::Storage)?;
                temp.persist_noclobber(&path)
                    .map_err(|_| ArtifactError::Storage)?;
            }
            Err(_) => return Err(ArtifactError::Storage),
        }
        // Ensure the blob directory entry is durable before publishing its SQLite reference.
        #[cfg(unix)]
        fs::File::open(directory)
            .and_then(|dir| dir.sync_all())
            .map_err(|_| ArtifactError::Storage)?;
        Ok(())
    }
}

fn ensure_directory(path: &Path) -> Result<(), ArtifactError> {
    let mut builder = fs::DirBuilder::new();
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        builder.mode(0o700);
    }
    match builder.create(path) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => check_directory(path),
        Err(_) => Err(ArtifactError::Storage),
    }
}

fn check_directory(path: &Path) -> Result<(), ArtifactError> {
    if fs::symlink_metadata(path)
        .map_err(|_| ArtifactError::Storage)?
        .is_dir()
    {
        Ok(())
    } else {
        Err(ArtifactError::Storage)
    }
}

fn decode_metadata(id: &str, version: u64, data: &str) -> Result<ArtifactMetadata, ArtifactError> {
    let meta: ArtifactMetadata = serde_json::from_str(data).map_err(|_| ArtifactError::Storage)?;
    meta.check().map_err(|_| ArtifactError::Storage)?;
    if meta.artifact_id != id || meta.version != version || meta.preview_url.is_some() {
        return Err(ArtifactError::Storage);
    }
    Ok(meta)
}

fn read_metadata(
    db: &Connection,
    id: &str,
    version: Option<u64>,
) -> Result<ArtifactMetadata, ArtifactError> {
    let (version, data): (u64, String) = db.query_row(
        "SELECT v.version, v.metadata FROM artifact_heads h JOIN artifact_versions v ON v.id = h.id
         WHERE h.id = ?1 AND v.version = coalesce(?2, h.version)",
        params![id, version], |row| Ok((row.get(0)?, row.get(1)?)),
    ).optional().map_err(|_| ArtifactError::Storage)?.ok_or(ArtifactError::NotFound)?;
    decode_metadata(id, version, &data)
}

fn insert_metadata(db: &Connection, meta: &ArtifactMetadata) -> Result<(), ArtifactError> {
    let data = serde_json::to_string(meta).map_err(|_| ArtifactError::Storage)?;
    db.execute(
        "INSERT INTO artifact_versions VALUES (?1, ?2, ?3)",
        params![meta.artifact_id, meta.version, data],
    )
    .map_err(|_| ArtifactError::Storage)?;
    Ok(())
}

impl ArtifactBackend for LocalArtifactBackend {
    fn create(&self, request: CreateArtifact) -> Result<ArtifactMetadata, ArtifactError> {
        let bytes = request.validated_bytes()?;
        let now = now_ms();
        let meta = ArtifactMetadata {
            artifact_id: Uuid::now_v7().to_string(),
            workspace_id: request.workspace_id,
            room_id: request.room_id,
            title: request.title,
            kind: request.kind,
            mime_type: request.mime_type,
            language: request.language,
            version: 1,
            sha256: crate::sha256_hex(&bytes),
            byte_size: bytes.len(),
            created_at_ms: now,
            updated_at_ms: now,
            preview_url: None,
        };
        let mut db = self.connect()?;
        let tx = db
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(|_| ArtifactError::Storage)?;
        tx.execute(
            "INSERT INTO artifact_heads VALUES (?1, 1, ?2, ?3)",
            params![meta.artifact_id, meta.room_id, meta.kind.as_str()],
        )
        .map_err(|_| ArtifactError::Storage)?;
        self.write_blob(&meta, &bytes)?;
        insert_metadata(&tx, &meta)?;
        tx.commit().map_err(|_| ArtifactError::Storage)?;
        Ok(meta)
    }

    fn update(&self, id: &str, request: UpdateArtifact) -> Result<ArtifactUpdate, ArtifactError> {
        check_id(id)?;
        let bytes = request.validated_bytes()?;
        let mut db = self.connect()?;
        let tx = db
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(|_| ArtifactError::Storage)?;
        let mut meta = read_metadata(&tx, id, None)?;
        if meta.version != request.expected_version {
            return Err(ArtifactError::VersionConflict);
        }
        check_version(meta.version + 1)?;
        check_bytes(meta.kind, &meta.mime_type, &bytes)?;
        let previous = self.read_blob(&meta)?;
        let diff = if meta.kind.is_text() {
            Some(summarize_diff(
                std::str::from_utf8(&previous).map_err(|_| ArtifactError::Storage)?,
                std::str::from_utf8(&bytes).map_err(|_| ArtifactError::Storage)?,
            ))
        } else {
            None
        };
        meta.version += 1;
        if let Some(title) = request.title {
            meta.title = title;
        }
        meta.updated_at_ms = now_ms().max(meta.updated_at_ms);
        meta.sha256 = crate::sha256_hex(&bytes);
        meta.byte_size = bytes.len();
        self.write_blob(&meta, &bytes)?;
        insert_metadata(&tx, &meta)?;
        tx.execute(
            "UPDATE artifact_heads SET version = ?2 WHERE id = ?1",
            params![id, meta.version],
        )
        .map_err(|_| ArtifactError::Storage)?;
        tx.commit().map_err(|_| ArtifactError::Storage)?;
        Ok(ArtifactUpdate {
            metadata: meta,
            diff,
        })
    }

    fn get(&self, id: &str, version: Option<u64>) -> Result<ArtifactVersion, ArtifactError> {
        check_id(id)?;
        if let Some(v) = version {
            check_version(v)?;
        }
        let meta = read_metadata(&self.connect()?, id, version)?;
        let content = content_for(meta.kind, self.read_blob(&meta)?)?;
        Ok(ArtifactVersion {
            metadata: meta,
            content,
        })
    }

    fn list(&self, filter: ArtifactList) -> Result<ArtifactPage, ArtifactError> {
        let limit = filter.page_limit()?;
        let db = self.connect()?;
        let mut sql = String::from(
            "SELECT h.id, h.version, v.metadata FROM artifact_heads h
             JOIN artifact_versions v ON v.id = h.id AND v.version = h.version WHERE h.id > ?1",
        );
        // Only static SQL fragments; user values are bound parameters.
        let after = filter.after.as_deref().unwrap_or("");
        let room = filter.room_id.as_deref();
        let kind = filter.kind.map(ArtifactKind::as_str);
        sql.push_str(if room.is_some() {
            " AND h.room = ?2"
        } else {
            " AND ?2 IS NULL"
        });
        sql.push_str(if kind.is_some() {
            " AND h.kind = ?3"
        } else {
            " AND ?3 IS NULL"
        });
        sql.push_str(" ORDER BY h.id LIMIT ?4");
        let mut statement = db.prepare(&sql).map_err(|_| ArtifactError::Storage)?;
        let mut rows = statement
            .query(params![after, room, kind, limit + 1])
            .map_err(|_| ArtifactError::Storage)?;
        let mut page = ArtifactPage {
            artifacts: Vec::with_capacity(limit),
            next_after: None,
        };
        while let Some(row) = rows.next().map_err(|_| ArtifactError::Storage)? {
            if page.artifacts.len() == limit {
                page.next_after = page.artifacts.last().map(|meta| meta.artifact_id.clone());
                break;
            }
            let id: String = row.get(0).map_err(|_| ArtifactError::Storage)?;
            let version = row.get(1).map_err(|_| ArtifactError::Storage)?;
            let data: String = row.get(2).map_err(|_| ArtifactError::Storage)?;
            let meta = decode_metadata(&id, version, &data)?;
            if room.is_some_and(|room| meta.room_id.as_deref() != Some(room))
                || filter.kind.is_some_and(|kind| meta.kind != kind)
            {
                return Err(ArtifactError::Storage);
            }
            page.artifacts.push(meta);
        }
        Ok(page)
    }

    fn delete(&self, id: &str) -> Result<(), ArtifactError> {
        check_id(id)?;
        // The FK cascade atomically removes every version. Shared blobs are retained,
        // including unreferenced blobs; this is logical deletion, not secure erasure.
        if self
            .connect()?
            .execute("DELETE FROM artifact_heads WHERE id = ?1", [id])
            .map_err(|_| ArtifactError::Storage)?
            == 0
        {
            return Err(ArtifactError::NotFound);
        }
        Ok(())
    }
}
