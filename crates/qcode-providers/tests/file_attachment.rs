//! Files in a turn: the log keeps a reference, the store keeps the bytes, and
//! the model gets a handle it can open with its own file tools.
//!
//! This is ADR-0061 §7: a file is never inlined into the request. What the
//! model reads is a line naming the name, size, digest and the read-only path.

use qcode_attachments::{AttachmentRef, AttachmentStore, FileRef};
use qcode_core::engine::attachments::file_handle_text;
use qcode_core::engine::{AttachmentSource, RunOptions, TurnContext, run_turn};
use qcode_core::event::{ContentBlock, EventKind, SessionStarted};
use qcode_core::history::messages_from_events_with;
use qcode_core::policy::Policy;
use qcode_core::provider::Role;
use qcode_core::store::SessionStore;
use qcode_core::tool::ToolRegistry;
use qcode_providers::MockProvider;
use tokio_util::sync::CancellationToken;

fn meta() -> SessionStarted {
    SessionStarted {
        model: "mock-1".to_string(),
        provider: "mock".to_string(),
        cwd: "/tmp".to_string(),
        version: "0.1.0".to_string(),
    }
}

fn options<'a>(id: &'a str, cwd: &std::path::Path, files: Vec<FileRef>) -> RunOptions<'a> {
    RunOptions {
        session_id: id,
        model: "mock-1",
        input: "what is in this file?".to_string(),
        cwd: cwd.to_path_buf(),
        max_turns: 2,
        cancel: CancellationToken::new(),
        pricing: None,
        max_cost_usd: None,
        on_delta: None,
        context_window: None,
        agents_md: None,
        skills: None,
        network: None,
        attachments: files
            .into_iter()
            .map(|file| AttachmentSource::Stored(AttachmentRef::File(file)))
            .collect(),
        inbox: None,
        continuation: None,
    }
}

#[tokio::test]
async fn a_file_reaches_the_model_as_a_handle_and_survives_replay() {
    let dir = tempfile::tempdir().expect("tempdir");
    let store = SessionStore::new(dir.path().join("data"));
    let (id, _) = store.create(&meta()).expect("session");
    let attachments = AttachmentStore::open(store.root()).expect("store");
    let body = b"hello world\n";
    let file = attachments
        .put_file("notes.txt", &body[..])
        .expect("put_file");
    assert_eq!(file.bytes, body.len() as u64);

    let provider = MockProvider::text("read it");
    let tools = ToolRegistry::new(Vec::new());
    let policy = Policy::default();
    run_turn(
        TurnContext {
            store: &store,
            provider: &provider,
            tools: &tools,
            policy: &policy,
            approver: &qcode_core::approval::AutoApprove,
            snapshots: None,
            scheduler: None,
        },
        options(&id, dir.path(), vec![file.clone()]),
        |_| {},
    )
    .await
    .expect("turn");

    // The log records the reference, not a copy of the bytes.
    let events = store.load(&id).expect("load");
    let logged = events
        .iter()
        .find_map(|event| match &event.kind {
            EventKind::UserMessage(message) => message.attachments.first().cloned(),
            _ => None,
        })
        .expect("an attachment on the user message");
    let AttachmentRef::File(logged) = logged else {
        panic!("expected a file ref");
    };
    assert_eq!(logged, file);

    // The provider reads a handle: name, size, digest and the path on disk.
    let request = provider.requests().first().cloned().expect("a request");
    let user = request
        .messages
        .iter()
        .rev()
        .find(|message| message.role == Role::User)
        .expect("a user message");
    assert_eq!(user.blocks.len(), 2);
    let ContentBlock::Text { text } = &user.blocks[1] else {
        panic!("expected the handle right after the question");
    };
    assert!(
        text.starts_with("[File \"notes.txt\" (12 bytes, sha256:"),
        "{text}"
    );
    assert!(text.contains("read-only copy saved at"), "{text}");
    assert!(text.contains("notes.txt"), "{text}");

    // The stored copy is readable and holds the exact bytes.
    let path = attachments.file_path(&file).expect("path");
    assert_eq!(std::fs::read(&path).expect("read"), body);

    // Replay rebuilds the same handle from the log alone.
    let replayed = messages_from_events_with(
        &events,
        |_| None,
        |file| file_handle_text(&attachments, file),
    );
    let replayed_user = replayed
        .iter()
        .rev()
        .find(|message| message.role == Role::User)
        .expect("a user message");
    assert_eq!(
        format!("{:?}", replayed_user.blocks[1]),
        format!("{:?}", user.blocks[1]),
        "replay must recreate the handle the first send produced"
    );
}

#[tokio::test]
async fn a_file_whose_object_is_gone_falls_back_to_a_limitation_notice() {
    let dir = tempfile::tempdir().expect("tempdir");
    let store = SessionStore::new(dir.path().join("data"));
    let attachments = AttachmentStore::open(store.root()).expect("store");
    let file = attachments
        .put_file("gone.txt", &b"x"[..])
        .expect("put_file");

    let path = attachments.file_path(&file).expect("path");
    std::fs::remove_file(&path).expect("drop the object");

    let text = file_handle_text(&attachments, &file);
    assert!(text.contains("not readable from this process"), "{text}");
    assert!(text.contains("do not claim to have read it"), "{text}");
}
