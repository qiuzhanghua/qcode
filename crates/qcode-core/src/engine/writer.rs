use super::*;

/// Appends events for one session while keeping `seq` and `turn` consistent.
pub struct SessionWriter<'a> {
    pub(crate) store: &'a SessionStore,
    pub(crate) session_id: String,
    pub(crate) seq: u64,
    pub(crate) turn: u32,
    /// Held for the lifetime of the turn; two writers cannot share a session.
    pub(crate) _lock: SessionLock,
}

impl<'a> SessionWriter<'a> {
    pub fn resume(store: &'a SessionStore, session_id: &str) -> Result<Self, StoreError> {
        let lock = store.lock(session_id)?;
        // A previous run may have died between `tool_call` and `tool_result`.
        store.repair_interrupted(session_id)?;
        let events = store.load(session_id)?;
        let seq = events.last().map(|event| event.seq).unwrap_or(0);
        let turn = events.last().map(|event| event.turn).unwrap_or(0);
        Ok(Self {
            store,
            session_id: session_id.to_string(),
            seq,
            turn,
            _lock: lock,
        })
    }

    /// Record the shell-environment sample for a session that is about to run
    /// (ADR-0064). Lives outside `run_turn` because the snapshot comes from the
    /// tool layer: the caller samples, then hands the report here, so the event
    /// lands before the turn it describes.
    pub fn record_shell_audit(
        store: &'a SessionStore,
        session_id: &str,
        audit: crate::event::ShellEnvSampled,
    ) -> Result<(), StoreError> {
        if audit.is_quiet() {
            return Ok(());
        }
        let mut writer = Self::resume(store, session_id)?;
        writer.append(crate::event::EventKind::ShellEnvSampled(audit), &mut |_| {})?;
        Ok(())
    }

    pub fn session_id(&self) -> &str {
        &self.session_id
    }

    pub fn append(
        &mut self,
        kind: EventKind,
        sink: &mut dyn FnMut(&Event),
    ) -> Result<Event, StoreError> {
        let seq = self.seq + 1;
        let event = Event::new(seq, &self.session_id, self.turn, kind);
        self.store.append(&event)?;
        self.seq = seq;
        sink(&event);
        Ok(event)
    }

    pub fn begin_turn(
        &mut self,
        input: &str,
        continuation: Option<Continuation>,
        attachments: &[qcode_attachments::AttachmentRef],
        sink: &mut dyn FnMut(&Event),
    ) -> Result<(), StoreError> {
        self.turn += 1;
        self.append(
            EventKind::TurnStarted(TurnStarted {
                input: input.to_string(),
            }),
            sink,
        )?;
        self.append(
            EventKind::UserMessage(UserMessage {
                starts_segment: false,
                queued_id: None,
                text: input.to_string(),
                attachments: attachments.to_vec(),
                continuation,
            }),
            sink,
        )?;
        Ok(())
    }

    pub fn turn(&self) -> u32 {
        self.turn
    }
}

#[cfg(test)]
mod shell_audit_tests {
    use super::*;
    use crate::event::{EventKind, SessionStarted, ShellEnvSampled};
    use crate::store::SessionStore;

    fn session(store: &SessionStore) -> String {
        let meta = SessionStarted {
            model: "mock-1".to_string(),
            provider: "mock".to_string(),
            cwd: "/tmp".to_string(),
            version: "0.0.0".to_string(),
        };
        store.create_task(&meta).expect("create task").id
    }

    #[test]
    fn the_sample_is_written_before_the_turn_it_belongs_to() {
        // ADR-0064: the log says what the agent's shell saw.
        let dir = tempfile::tempdir().expect("temp dir");
        let store = SessionStore::new(dir.path());
        let id = session(&store);

        SessionWriter::record_shell_audit(
            &store,
            &id,
            ShellEnvSampled {
                program: "/bin/zsh".to_string(),
                count: 126,
                added: vec!["JAVA_HOME".to_string()],
                removed: Vec::new(),
                duration_ms: 725,
                cached: false,
                error: None,
            },
        )
        .expect("record");

        let events = store.load(&id).expect("load");
        let audit = events
            .iter()
            .find_map(|event| match &event.kind {
                EventKind::ShellEnvSampled(audit) => Some(audit),
                _ => None,
            })
            .expect("the audit event is in the log");
        assert_eq!(audit.program, "/bin/zsh");
        assert_eq!(audit.count, 126);
        assert_eq!(audit.added, vec!["JAVA_HOME".to_string()]);
        assert_eq!(audit.duration_ms, 725);
        assert!(!audit.cached);
    }

    #[test]
    fn a_quiet_sample_leaves_no_trace() {
        // Sampling off, or a first attempt that produced nothing: no row.
        let dir = tempfile::tempdir().expect("temp dir");
        let store = SessionStore::new(dir.path());
        let id = session(&store);
        let before = store.load(&id).expect("load").len();

        SessionWriter::record_shell_audit(
            &store,
            &id,
            ShellEnvSampled {
                program: "/bin/zsh".to_string(),
                count: 0,
                added: Vec::new(),
                removed: Vec::new(),
                duration_ms: 0,
                cached: false,
                error: None,
            },
        )
        .expect("record");

        assert_eq!(store.load(&id).expect("load").len(), before);
    }
}
