//! JSONL 会话转录：元信息 + 消息 + usage + 笔记。

use std::fs::{File, OpenOptions};
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use harn_core::error::{HarnError, Result};
use harn_core::message::{Message, Usage};
use serde::{Deserialize, Serialize};

const VERSION: u32 = 1;

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SessionMeta {
    pub version: u32,
    pub id: String,
    pub cwd: String,
    pub model: String,
    pub created_at: String,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum Entry {
    Meta {
        version: u32,
        id: String,
        cwd: String,
        model: String,
        created_at: String,
    },
    Message {
        timestamp: String,
        message: Message,
    },
    Usage {
        timestamp: String,
        usage: Usage,
    },
    Note {
        timestamp: String,
        kind: String,
        text: String,
    },
}

#[derive(Debug, Default)]
pub struct LoadedSession {
    pub meta: Option<SessionMeta>,
    pub messages: Vec<Message>,
    pub usage: Usage,
    pub notes: Vec<(String, String)>,
}

pub struct SessionWriter {
    id: String,
    path: PathBuf,
    file: Mutex<File>,
}

impl SessionWriter {
    pub fn id(&self) -> &str {
        &self.id
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn append_message(&self, message: &Message) -> Result<()> {
        self.append(&Entry::Message {
            timestamp: now(),
            message: message.clone(),
        })
    }

    pub fn append_usage(&self, usage: &Usage) -> Result<()> {
        self.append(&Entry::Usage {
            timestamp: now(),
            usage: usage.clone(),
        })
    }

    pub fn append_note(&self, kind: &str, text: &str) -> Result<()> {
        self.append(&Entry::Note {
            timestamp: now(),
            kind: kind.to_string(),
            text: text.to_string(),
        })
    }

    fn append(&self, entry: &Entry) -> Result<()> {
        let mut file = self
            .file
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let line = serde_json::to_string(entry)?;
        writeln!(file, "{line}").map_err(|e| {
            HarnError::Session(harn_i18n::tr!("session.error.write_failed", error = e))
        })?;
        file.flush().map_err(|e| {
            HarnError::Session(harn_i18n::tr!("session.error.flush_failed", error = e))
        })?;
        Ok(())
    }
}

pub struct SessionStore {
    dir: PathBuf,
}

impl SessionStore {
    pub fn new(dir: impl Into<PathBuf>) -> Result<Self> {
        let dir = dir.into();
        std::fs::create_dir_all(&dir).map_err(|e| {
            HarnError::Session(harn_i18n::tr!("session.error.create_dir_failed", error = e))
        })?;
        Ok(Self { dir })
    }

    pub fn dir(&self) -> &Path {
        &self.dir
    }

    pub fn create(&self, cwd: &Path, model: &str) -> Result<SessionWriter> {
        let id = uuid::Uuid::new_v4().simple().to_string();
        let path = self.dir.join(format!("{id}.jsonl"));
        let mut file = OpenOptions::new()
            .create_new(true)
            .append(true)
            .open(&path)
            .map_err(|e| {
                HarnError::Session(harn_i18n::tr!(
                    "session.error.create_file_failed",
                    error = e
                ))
            })?;
        let meta = Entry::Meta {
            version: VERSION,
            id: id.clone(),
            cwd: cwd.to_string_lossy().into_owned(),
            model: model.to_string(),
            created_at: now(),
        };
        writeln!(file, "{}", serde_json::to_string(&meta)?).map_err(|e| {
            HarnError::Session(harn_i18n::tr!("session.error.write_meta_failed", error = e))
        })?;
        file.flush().map_err(|e| {
            HarnError::Session(harn_i18n::tr!("session.error.flush_failed", error = e))
        })?;
        Ok(SessionWriter {
            id,
            path,
            file: Mutex::new(file),
        })
    }

    pub fn open(&self, id: &str) -> Result<SessionWriter> {
        let path = self.resolve(id)?;
        let id = path
            .file_stem()
            .and_then(|stem| stem.to_str())
            .unwrap_or(id)
            .to_string();
        let file = OpenOptions::new().append(true).open(&path).map_err(|e| {
            HarnError::Session(harn_i18n::tr!("session.error.open_failed", error = e))
        })?;
        Ok(SessionWriter {
            id,
            path,
            file: Mutex::new(file),
        })
    }

    pub fn load(&self, id: &str) -> Result<LoadedSession> {
        let path = self.resolve(id)?;
        let file = File::open(&path).map_err(|e| {
            HarnError::Session(harn_i18n::tr!("session.error.open_failed", error = e))
        })?;
        let reader = BufReader::new(file);
        let mut loaded = LoadedSession::default();
        for (index, line) in reader.lines().enumerate() {
            let line = line.map_err(|e| {
                HarnError::Session(harn_i18n::tr!("session.error.read_failed", error = e))
            })?;
            if line.trim().is_empty() {
                continue;
            }
            let entry: Entry = serde_json::from_str(&line).map_err(|e| {
                HarnError::Session(harn_i18n::tr!(
                    "session.error.parse_line_failed",
                    line = index + 1,
                    error = e
                ))
            })?;
            match entry {
                Entry::Meta {
                    version,
                    id,
                    cwd,
                    model,
                    created_at,
                } => {
                    loaded.meta = Some(SessionMeta {
                        version,
                        id,
                        cwd,
                        model,
                        created_at,
                    });
                }
                Entry::Message { message, .. } => loaded.messages.push(message),
                Entry::Usage { usage, .. } => loaded.usage.add(&usage),
                Entry::Note { kind, text, .. } => loaded.notes.push((kind, text)),
            }
        }
        Ok(loaded)
    }

    pub fn list(&self) -> Result<Vec<(SessionMeta, u64)>> {
        let mut out = Vec::new();
        let entries = std::fs::read_dir(&self.dir).map_err(|e| {
            HarnError::Session(harn_i18n::tr!("session.error.read_dir_failed", error = e))
        })?;
        for entry in entries {
            let entry = entry.map_err(|e| {
                HarnError::Session(harn_i18n::tr!("session.error.read_entry_failed", error = e))
            })?;
            let path = entry.path();
            if path.extension().and_then(|ext| ext.to_str()) != Some("jsonl") {
                continue;
            }
            let size = entry.metadata().map(|m| m.len()).unwrap_or(0);
            let file = match File::open(&path) {
                Ok(file) => file,
                Err(_) => continue,
            };
            let mut first = String::new();
            if BufReader::new(file).read_line(&mut first).is_err() {
                continue;
            }
            if let Ok(Entry::Meta {
                version,
                id,
                cwd,
                model,
                created_at,
            }) = serde_json::from_str::<Entry>(first.trim())
            {
                out.push((
                    SessionMeta {
                        version,
                        id,
                        cwd,
                        model,
                        created_at,
                    },
                    size,
                ));
            }
        }
        out.sort_by(|a, b| b.0.created_at.cmp(&a.0.created_at));
        Ok(out)
    }

    pub fn latest(&self) -> Result<Option<SessionMeta>> {
        Ok(self.list()?.into_iter().next().map(|(meta, _)| meta))
    }

    pub fn resolve(&self, id: &str) -> Result<PathBuf> {
        let exact = self.dir.join(format!("{id}.jsonl"));
        if exact.exists() {
            return Ok(exact);
        }
        let mut matches = Vec::new();
        let entries = std::fs::read_dir(&self.dir).map_err(|e| {
            HarnError::Session(harn_i18n::tr!("session.error.read_dir_failed", error = e))
        })?;
        for entry in entries.flatten() {
            let path = entry.path();
            let Some(stem) = path.file_stem().and_then(|stem| stem.to_str()) else {
                continue;
            };
            if stem.starts_with(id) {
                matches.push(path);
            }
        }
        matches.sort();
        match matches.len() {
            0 => Err(HarnError::Session(harn_i18n::tr!(
                "session.error.not_found",
                id = id
            ))),
            1 => Ok(matches.remove(0)),
            _ => Err(HarnError::Session(harn_i18n::tr!(
                "session.error.ambiguous_id",
                id = id,
                count = matches.len()
            ))),
        }
    }
}

fn now() -> String {
    chrono::Utc::now().to_rfc3339()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trips_messages_and_usage() {
        let dir = tempfile_dir();
        let store = SessionStore::new(&dir).unwrap();
        let writer = store.create(Path::new("/work"), "test-model").unwrap();
        writer.append_message(&Message::user_text("hello")).unwrap();
        writer
            .append_message(&Message::assistant_text("world"))
            .unwrap();
        writer
            .append_usage(&Usage {
                input_tokens: 10,
                output_tokens: 20,
                ..Default::default()
            })
            .unwrap();
        writer.append_note("plan", "do it").unwrap();

        let loaded = store.load(writer.id()).unwrap();
        assert_eq!(loaded.messages.len(), 2);
        assert_eq!(loaded.usage.total_tokens(), 30);
        assert_eq!(loaded.meta.unwrap().model, "test-model");
        assert_eq!(store.list().unwrap().len(), 1);

        std::fs::remove_dir_all(dir).ok();
    }

    fn tempfile_dir() -> PathBuf {
        std::env::temp_dir().join(format!(
            "harn-session-test-{}-{}",
            std::process::id(),
            uuid::Uuid::new_v4().simple()
        ))
    }
}
