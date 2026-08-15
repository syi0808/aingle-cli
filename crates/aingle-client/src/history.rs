use std::{
    fs::{self, File, OpenOptions},
    io::{Read, Write},
    path::{Path, PathBuf},
    sync::Mutex,
};

use aingle_protocol::Visibility;
use chrono::Utc;
use rusqlite::{Connection, params};
use serde::Serialize;
use thiserror::Error;
use uuid::Uuid;

const FILE_MAGIC: &[u8; 8] = b"AINGHST1";

#[derive(Debug, Error)]
pub enum HistoryError {
    #[error("history I/O error: {0}")]
    Io(#[from] std::io::Error),
    #[error("history database error: {0}")]
    Database(#[from] rusqlite::Error),
    #[error("invalid transcript: {0}")]
    InvalidTranscript(String),
}

pub struct HistoryStore {
    root: PathBuf,
    database: Mutex<Connection>,
}

#[derive(Debug, Serialize)]
pub struct ConversationSummary {
    pub conversation_id: Uuid,
    pub peer_agent_id: String,
    pub started_at: String,
    pub ended_at: Option<String>,
    pub message_count: u64,
    pub final_seq: Option<u64>,
    pub sync_status: String,
}

#[derive(Debug, Serialize)]
pub struct StoredMessage {
    pub seq: u64,
    pub sender: u8,
    pub timestamp_ms: u64,
    pub content: String,
}

impl HistoryStore {
    pub fn open(path: impl AsRef<Path>) -> Result<Self, HistoryError> {
        let root = path.as_ref().to_path_buf();
        fs::create_dir_all(root.join("conversations"))?;
        let database = Connection::open(root.join("index.db"))?;
        database.execute_batch(
            "PRAGMA journal_mode=WAL;
             CREATE TABLE IF NOT EXISTS conversations (
               conversation_id TEXT PRIMARY KEY,
               peer_agent_id TEXT NOT NULL,
               started_at TEXT NOT NULL,
               ended_at TEXT,
               message_count INTEGER NOT NULL DEFAULT 0,
               final_seq INTEGER,
               local_path TEXT NOT NULL,
               sync_status TEXT NOT NULL DEFAULT 'active',
               visibility TEXT NOT NULL
             );",
        )?;
        Ok(Self {
            root,
            database: Mutex::new(database),
        })
    }

    pub fn begin(
        &self,
        id: Uuid,
        peer_agent_id: &str,
        visibility: Visibility,
    ) -> Result<(), HistoryError> {
        let path = self.transcript_path(id);
        let mut file = OpenOptions::new()
            .create_new(true)
            .write(true)
            .open(&path)?;
        file.write_all(FILE_MAGIC)?;
        file.write_all(id.as_bytes())?;
        let peer = peer_agent_id.as_bytes();
        file.write_all(&(peer.len() as u16).to_be_bytes())?;
        file.write_all(peer)?;
        file.sync_data()?;
        self.connection()?.execute(
            "INSERT OR REPLACE INTO conversations (conversation_id, peer_agent_id, started_at, local_path, visibility) VALUES (?1, ?2, ?3, ?4, ?5)",
            params![id.to_string(), peer_agent_id, Utc::now().to_rfc3339(), path.to_string_lossy(), visibility_name(visibility)],
        )?;
        Ok(())
    }

    pub fn append(
        &self,
        id: Uuid,
        seq: u64,
        sender: u8,
        timestamp_ms: u64,
        payload: &[u8],
    ) -> Result<(), HistoryError> {
        let mut file = OpenOptions::new()
            .append(true)
            .open(self.transcript_path(id))?;
        file.write_all(&seq.to_be_bytes())?;
        file.write_all(&[sender])?;
        file.write_all(&timestamp_ms.to_be_bytes())?;
        file.write_all(&(payload.len() as u32).to_be_bytes())?;
        file.write_all(payload)?;
        self.connection()?.execute(
            "UPDATE conversations SET message_count = message_count + 1 WHERE conversation_id = ?1",
            [id.to_string()],
        )?;
        Ok(())
    }

    pub fn finish(&self, id: Uuid, final_seq: u64) -> Result<(), HistoryError> {
        let messages = self.read(id)?;
        let complete = messages.len() as u64 == final_seq
            && messages
                .iter()
                .enumerate()
                .all(|(index, message)| message.seq == index as u64 + 1);
        self.connection()?.execute(
            "UPDATE conversations SET ended_at = ?2, final_seq = ?3, sync_status = ?4 WHERE conversation_id = ?1",
            params![id.to_string(), Utc::now().to_rfc3339(), final_seq, if complete { "complete" } else { "partial" }],
        )?;
        Ok(())
    }

    pub fn list(&self, limit: usize) -> Result<Vec<ConversationSummary>, HistoryError> {
        let connection = self.connection()?;
        let mut statement = connection.prepare(
            "SELECT conversation_id, peer_agent_id, started_at, ended_at, message_count, final_seq, sync_status FROM conversations ORDER BY started_at DESC LIMIT ?1",
        )?;
        let rows = statement.query_map([limit as i64], |row| {
            let id: String = row.get(0)?;
            Ok(ConversationSummary {
                conversation_id: Uuid::parse_str(&id).map_err(|error| {
                    rusqlite::Error::FromSqlConversionFailure(
                        0,
                        rusqlite::types::Type::Text,
                        Box::new(error),
                    )
                })?,
                peer_agent_id: row.get(1)?,
                started_at: row.get(2)?,
                ended_at: row.get(3)?,
                message_count: row.get(4)?,
                final_seq: row.get(5)?,
                sync_status: row.get(6)?,
            })
        })?;
        rows.collect::<Result<_, _>>().map_err(HistoryError::from)
    }

    pub fn read(&self, id: Uuid) -> Result<Vec<StoredMessage>, HistoryError> {
        let mut bytes = Vec::new();
        File::open(self.transcript_path(id))?.read_to_end(&mut bytes)?;
        if bytes.len() < 26 || &bytes[..8] != FILE_MAGIC || &bytes[8..24] != id.as_bytes() {
            return Err(HistoryError::InvalidTranscript("invalid header".into()));
        }
        let peer_length = u16::from_be_bytes([bytes[24], bytes[25]]) as usize;
        let mut offset = 26 + peer_length;
        if offset > bytes.len() {
            return Err(HistoryError::InvalidTranscript(
                "truncated peer identity".into(),
            ));
        }
        let mut messages = Vec::new();
        while offset < bytes.len() {
            if bytes.len() - offset < 21 {
                return Err(HistoryError::InvalidTranscript(
                    "truncated message header".into(),
                ));
            }
            let seq =
                u64::from_be_bytes(bytes[offset..offset + 8].try_into().expect("fixed slice"));
            let sender = bytes[offset + 8];
            let timestamp_ms = u64::from_be_bytes(
                bytes[offset + 9..offset + 17]
                    .try_into()
                    .expect("fixed slice"),
            );
            let length = u32::from_be_bytes(
                bytes[offset + 17..offset + 21]
                    .try_into()
                    .expect("fixed slice"),
            ) as usize;
            offset += 21;
            if bytes.len() - offset < length {
                return Err(HistoryError::InvalidTranscript(
                    "truncated message body".into(),
                ));
            }
            messages.push(StoredMessage {
                seq,
                sender,
                timestamp_ms,
                content: String::from_utf8_lossy(&bytes[offset..offset + length]).into_owned(),
            });
            offset += length;
        }
        Ok(messages)
    }

    pub fn contains(&self, id: Uuid) -> bool {
        self.transcript_path(id).exists()
    }
    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn import(
        &self,
        id: Uuid,
        peer_agent_id: &str,
        visibility: Visibility,
        messages: &[StoredMessage],
        final_seq: u64,
    ) -> Result<(), HistoryError> {
        if self.contains(id) {
            return Ok(());
        }
        self.begin(id, peer_agent_id, visibility)?;
        for message in messages {
            self.append(
                id,
                message.seq,
                message.sender,
                message.timestamp_ms,
                message.content.as_bytes(),
            )?;
        }
        self.finish(id, final_seq)
    }

    fn transcript_path(&self, id: Uuid) -> PathBuf {
        self.root.join("conversations").join(format!("{id}.aingle"))
    }
    fn connection(&self) -> Result<std::sync::MutexGuard<'_, Connection>, HistoryError> {
        self.database
            .lock()
            .map_err(|_| HistoryError::InvalidTranscript("history database lock poisoned".into()))
    }
}

fn visibility_name(value: Visibility) -> &'static str {
    match value {
        Visibility::Public => "public",
        Visibility::Unlisted => "unlisted",
        Visibility::Private => "private",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn history_is_append_only_and_detects_completion() {
        let temporary = tempfile::tempdir().unwrap();
        let store = HistoryStore::open(temporary.path()).unwrap();
        let id = Uuid::now_v7();
        store.begin(id, "agent_peer", Visibility::Public).unwrap();
        store.append(id, 1, 0, 10, b"hello").unwrap();
        store.append(id, 2, 1, 20, b"world").unwrap();
        store.finish(id, 2).unwrap();
        let messages = store.read(id).unwrap();
        assert_eq!(messages.len(), 2);
        assert_eq!(messages[1].content, "world");
        assert_eq!(store.list(10).unwrap()[0].sync_status, "complete");
    }

    #[test]
    fn missing_sequence_is_partial() {
        let temporary = tempfile::tempdir().unwrap();
        let store = HistoryStore::open(temporary.path()).unwrap();
        let id = Uuid::now_v7();
        store.begin(id, "agent_peer", Visibility::Private).unwrap();
        store.append(id, 2, 1, 20, b"late").unwrap();
        store.finish(id, 2).unwrap();
        assert_eq!(store.list(10).unwrap()[0].sync_status, "partial");
    }
}
