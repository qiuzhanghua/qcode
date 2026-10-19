//! Append-only JSONL session store.
//!
//! One file per session under `<root>/sessions/<id>.jsonl`. Writes are appends;
//! a torn final line is discarded on load, which is the crash-recovery contract.

use std::fs::{self, OpenOptions};
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};

use crate::event::{Event, EventKind, SessionStarted};

#[derive(Debug, thiserror::Error)]
pub enum StoreError {
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
    #[error("json error: {0}")]
    Json(#[from] serde_json::Error),
    #[error("invalid session id: {0}")]
    InvalidId(String),
    #[error("session not found: {0}")]
    NotFound(String),
    #[error("non-monotonic seq: expected {expected}, got {got}")]
    Seq { expected: u64, got: u64 },
    #[error("invalid fork boundary {boundary} (session has 1..={last})")]
    InvalidFork { boundary: u64, last: u64 },
}

#[derive(Debug, Clone)]
pub struct SessionStore {
    root: PathBuf,
}

#[derive(Debug, Clone, PartialEq)]
pub struct SessionSummary {
    pub id: String,
    pub started: SessionStarted,
    pub turns: u32,
    pub last_seq: u64,
}

impl SessionStore {
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn sessions_dir(&self) -> PathBuf {
        self.root.join("sessions")
    }

    pub fn session_path(&self, id: &str) -> PathBuf {
        self.sessions_dir().join(format!("{id}.jsonl"))
    }

    /// Create a new session file and append its `session_started` event.
    /// Returns the generated id together with the event that opened the log.
    pub fn create(&self, meta: &SessionStarted) -> Result<(String, Event), StoreError> {
        let id = ulid::Ulid::new().to_string();
        fs::create_dir_all(self.sessions_dir())?;
        let event = Event::new(1, &id, 0, EventKind::SessionStarted(meta.clone()));
        self.append(&event)?;
        Ok((id, event))
    }

    pub fn append(&self, event: &Event) -> Result<(), StoreError> {
        self.validate_id(&event.session)?;
        let path = self.session_path(&event.session);
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        let mut file = OpenOptions::new().create(true).append(true).open(path)?;
        serde_json::to_writer(&mut file, event)?;
        file.write_all(b"\n")?;
        file.flush()?;
        Ok(())
    }

    pub fn load(&self, id: &str) -> Result<Vec<Event>, StoreError> {
        self.validate_id(id)?;
        let path = self.session_path(id);
        let file = match fs::File::open(&path) {
            Ok(file) => file,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
                return Err(StoreError::NotFound(id.to_string()));
            }
            Err(err) => return Err(StoreError::Io(err)),
        };

        let lines: Vec<String> = BufReader::new(file).lines().collect::<Result<_, _>>()?;
        let total = lines.len();
        let mut events: Vec<Event> = Vec::new();

        for (index, line) in lines.iter().enumerate() {
            if line.trim().is_empty() {
                continue;
            }
            match serde_json::from_str::<Event>(line) {
                Ok(event) => {
                    if let Some(last) = events.last() {
                        let expected = last.seq + 1;
                        if event.seq != expected {
                            return Err(StoreError::Seq {
                                expected,
                                got: event.seq,
                            });
                        }
                    }
                    events.push(event);
                }
                // A crash can leave a partial final line. Drop only that one.
                Err(_) if index + 1 == total => break,
                Err(err) => return Err(StoreError::Json(err)),
            }
        }

        Ok(events)
    }

    pub fn list(&self) -> Result<Vec<SessionSummary>, StoreError> {
        let dir = self.sessions_dir();
        if !dir.exists() {
            return Ok(Vec::new());
        }

        let mut summaries = Vec::new();
        for entry in fs::read_dir(dir)? {
            let entry = entry?;
            let path = entry.path();
            if path.extension().and_then(|ext| ext.to_str()) != Some("jsonl") {
                continue;
            }
            let Some(id) = path.file_stem().and_then(|stem| stem.to_str()) else {
                continue;
            };
            let Ok(events) = self.load(id) else {
                continue;
            };
            let Some(first) = events.first() else {
                continue;
            };
            let EventKind::SessionStarted(started) = &first.kind else {
                continue;
            };
            summaries.push(SessionSummary {
                id: id.to_string(),
                started: started.clone(),
                turns: events.last().map(|event| event.turn).unwrap_or(0),
                last_seq: events.last().map(|event| event.seq).unwrap_or(0),
            });
        }

        // ULIDs are time-sortable; newest first.
        summaries.sort_by(|a, b| b.id.cmp(&a.id));
        Ok(summaries)
    }

    /// Conversation fork: copy events up to and including `at_seq` into a new
    /// session, renumbering `seq` and `session`. Files on disk are untouched.
    ///
    /// The new session can be resumed like any other; `None` forks the whole
    /// conversation.
    pub fn fork(&self, source: &str, at_seq: Option<u64>) -> Result<String, StoreError> {
        let events = self.load(source)?;
        let last = events.last().map(|event| event.seq).unwrap_or(0);
        let boundary = at_seq.unwrap_or(last);
        if boundary == 0 || boundary > last || !events.iter().any(|event| event.seq == boundary) {
            return Err(StoreError::InvalidFork { boundary, last });
        }

        let id = ulid::Ulid::new().to_string();
        fs::create_dir_all(self.sessions_dir())?;
        for (index, event) in events
            .iter()
            .filter(|event| event.seq <= boundary)
            .enumerate()
        {
            let mut copy = event.clone();
            copy.session = id.clone();
            copy.seq = index as u64 + 1;
            if let Err(err) = self.append(&copy) {
                // Do not leave a half-written fork behind.
                let _ = fs::remove_file(self.session_path(&id));
                return Err(err);
            }
        }
        Ok(id)
    }

    fn validate_id(&self, id: &str) -> Result<(), StoreError> {
        let ok = !id.is_empty() && id.chars().all(|c| c.is_ascii_alphanumeric());
        if ok {
            Ok(())
        } else {
            Err(StoreError::InvalidId(id.to_string()))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::event::{TurnStarted, UserMessage};

    fn meta() -> SessionStarted {
        SessionStarted {
            model: "mock-1".to_string(),
            provider: "mock".to_string(),
            cwd: "/tmp".to_string(),
            version: "0.1.0".to_string(),
        }
    }

    fn seed(store: &SessionStore) -> String {
        let (id, _) = store.create(&meta()).expect("create");
        store
            .append(&Event::new(
                2,
                &id,
                1,
                EventKind::TurnStarted(TurnStarted {
                    input: "hi".to_string(),
                }),
            ))
            .expect("turn started");
        store
            .append(&Event::new(
                3,
                &id,
                1,
                EventKind::UserMessage(UserMessage {
                    text: "hi".to_string(),
                    attachments: Vec::new(),
                }),
            ))
            .expect("user message");
        store
            .append(&Event::new(
                4,
                &id,
                1,
                EventKind::UserMessage(UserMessage {
                    text: "again".to_string(),
                    attachments: Vec::new(),
                }),
            ))
            .expect("user message");
        id
    }

    #[test]
    fn fork_copies_a_prefix_under_a_new_id() {
        let dir = tempfile::tempdir().expect("temp dir");
        let store = SessionStore::new(dir.path());
        let source = seed(&store);

        let fork = store.fork(&source, Some(3)).expect("fork");

        assert_ne!(fork, source);
        assert_eq!(store.load(&source).expect("source").len(), 4);
        let copied = store.load(&fork).expect("fork session");
        assert_eq!(copied.len(), 3);
        assert!(copied.iter().all(|event| event.session == fork));
        assert_eq!(
            copied.iter().map(|event| event.seq).collect::<Vec<_>>(),
            vec![1, 2, 3]
        );
        assert!(matches!(&copied[1].kind, EventKind::TurnStarted(_)));
        assert_eq!(copied[1].turn, 1);
    }

    #[test]
    fn fork_without_a_boundary_copies_everything() {
        let dir = tempfile::tempdir().expect("temp dir");
        let store = SessionStore::new(dir.path());
        let source = seed(&store);

        let fork = store.fork(&source, None).expect("fork");
        assert_eq!(store.load(&fork).expect("fork session").len(), 4);
    }

    #[test]
    fn fork_rejects_a_boundary_outside_the_session() {
        let dir = tempfile::tempdir().expect("temp dir");
        let store = SessionStore::new(dir.path());
        let source = seed(&store);

        assert!(matches!(
            store.fork(&source, Some(0)),
            Err(StoreError::InvalidFork { .. })
        ));
        assert!(matches!(
            store.fork(&source, Some(99)),
            Err(StoreError::InvalidFork { .. })
        ));
    }
}
