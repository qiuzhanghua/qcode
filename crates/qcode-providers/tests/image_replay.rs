//! Images in a turn: the log keeps a reference, the store keeps the bytes, and
//! a resumed turn hands the provider the very same bytes.
//!
//! This is the contract ADR-0061 promises: "replay equals the first send".

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as BASE64;
use qcode_attachments::{AttachmentRef, AttachmentStore};
use qcode_core::engine::{Attachment, RunOptions, TurnContext, run_turn};
use qcode_core::event::{ContentBlock, EventKind, SessionStarted};
use qcode_core::history::messages_from_events_with;
use qcode_core::policy::Policy;
use qcode_core::provider::{Capabilities, Role};
use qcode_core::store::SessionStore;
use qcode_core::tool::ToolRegistry;
use qcode_providers::MockProvider;
use tokio_util::sync::CancellationToken;

/// A 2×1 PNG: enough for the store to read a real width/height back.
fn png() -> Vec<u8> {
    const BODY: &str = "iVBORw0KGgoAAAANSUhEUgAAAAIAAAABCAYAAAD0In+KAAAAEUlEQVR4nGP8z8Dwn4GBgQEADgAB/1sM3sAAAAAASUVORK5CYII=";
    BASE64.decode(BODY.trim()).expect("fixture")
}

fn meta() -> SessionStarted {
    SessionStarted {
        model: "mock-1".to_string(),
        provider: "mock".to_string(),
        cwd: "/tmp".to_string(),
        version: "0.1.0".to_string(),
    }
}

fn options<'a>(id: &'a str, cwd: &std::path::Path, attachments: Vec<Attachment>) -> RunOptions<'a> {
    RunOptions {
        session_id: id,
        model: "mock-1",
        input: "what is in this picture?".to_string(),
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
        attachments,
        inbox: None,
        continuation: None,
    }
}

#[tokio::test]
async fn an_image_is_stored_once_and_replayed_verbatim() {
    let dir = tempfile::tempdir().expect("tempdir");
    let store = SessionStore::new(dir.path().join("data"));
    let (id, _) = store.create(&meta()).expect("session");
    let bytes = png();

    let caps = Capabilities {
        images: true,
        ..Capabilities::default()
    };
    let provider = MockProvider::text("a dot").with_capabilities(caps);
    let tools = ToolRegistry::new(Vec::new());
    let policy = Policy::default();

    let outcome = run_turn(
        TurnContext {
            store: &store,
            provider: &provider,
            tools: &tools,
            policy: &policy,
            approver: &qcode_core::approval::AutoApprove,
            snapshots: None,
        },
        options(
            &id,
            dir.path(),
            vec![Attachment {
                mime_type: "image/png".to_string(),
                data_base64: BASE64.encode(&bytes),
            }],
        ),
        |_| {},
    )
    .await
    .expect("turn");
    assert_eq!(outcome.text, "a dot");

    // The log records a reference, not the bytes.
    let events = store.load(&id).expect("load");
    let image = events
        .iter()
        .find_map(|event| match &event.kind {
            EventKind::UserMessage(message) => message.attachments.first().cloned(),
            _ => None,
        })
        .expect("an attachment on the user message");
    let AttachmentRef::Image(image) = image else {
        panic!("expected an image ref");
    };
    assert_eq!(image.mime.as_str(), "image/png");
    assert_eq!(image.bytes, bytes.len() as u64);
    assert_eq!((image.width, image.height), (2, 1));

    // The bytes live in the store, and only once.
    let attachments = AttachmentStore::open(store.root()).expect("store");
    assert_eq!(attachments.read_image(&image).expect("read back"), bytes);
    assert_eq!(objects(store.root()), 1, "one object for one image");

    // The provider saw the bytes inline, exactly as before the store existed.
    let request = provider.requests().first().cloned().expect("a request");
    let user = request
        .messages
        .iter()
        .rev()
        .find(|message| message.role == Role::User)
        .expect("a user message");
    assert!(matches!(
        &user.blocks[0],
        ContentBlock::Text { text } if text == "what is in this picture?"
    ));
    let ContentBlock::Image {
        mime_type,
        data_base64,
    } = &user.blocks[1]
    else {
        panic!("expected the image right after the text");
    };
    assert_eq!(mime_type, "image/png");
    assert_eq!(BASE64.decode(data_base64).expect("base64"), bytes);

    // Replaying the log yields the same blocks, byte for byte.
    let replayed = messages_from_events_with(&events, |image| attachments.read_image(image).ok());
    let replayed_user = replayed
        .iter()
        .rev()
        .find(|message| message.role == Role::User)
        .expect("a user message");
    assert_eq!(replayed_user.blocks.len(), 2);
    assert_eq!(
        format!("{:?}", replayed_user.blocks[1]),
        format!("{:?}", user.blocks[1]),
        "replay must recreate the exact block the first send produced"
    );

    // A second turn with the same image dedupes: still one object.
    let provider = MockProvider::text("again").with_capabilities(Capabilities {
        images: true,
        ..Capabilities::default()
    });
    run_turn(
        TurnContext {
            store: &store,
            provider: &provider,
            tools: &tools,
            policy: &policy,
            approver: &qcode_core::approval::AutoApprove,
            snapshots: None,
        },
        options(
            &id,
            dir.path(),
            vec![Attachment {
                mime_type: "image/png".to_string(),
                data_base64: BASE64.encode(&bytes),
            }],
        ),
        |_| {},
    )
    .await
    .expect("second turn");
    assert_eq!(objects(store.root()), 1, "the same bytes are stored once");
}

#[tokio::test]
async fn a_mislabelled_attachment_is_refused_before_anything_is_written() {
    let dir = tempfile::tempdir().expect("tempdir");
    let store = SessionStore::new(dir.path().join("data"));
    let (id, _) = store.create(&meta()).expect("session");

    let provider = MockProvider::text("unused").with_capabilities(Capabilities {
        images: true,
        ..Capabilities::default()
    });
    let tools = ToolRegistry::new(Vec::new());
    let policy = Policy::default();

    let error = run_turn(
        TurnContext {
            store: &store,
            provider: &provider,
            tools: &tools,
            policy: &policy,
            approver: &qcode_core::approval::AutoApprove,
            snapshots: None,
        },
        options(
            &id,
            dir.path(),
            vec![Attachment {
                // GIF magic in a PNG costume.
                mime_type: "image/png".to_string(),
                data_base64: BASE64.encode(b"GIF89a\x01\x00\x01\x00\x00\x00\x00;"),
            }],
        ),
        |_| {},
    )
    .await
    .expect_err("the store must reject a mislabelled image");

    let message = error.to_string();
    assert!(
        message.contains("Png") && message.contains("Gif"),
        "the error should name both the declared and the sniffed type: {message}"
    );
    assert_eq!(objects(store.root()), 0, "nothing was written");
    assert!(
        provider.requests().is_empty(),
        "the provider is never called for an unusable attachment"
    );
}

/// Number of stored image objects under `<data>/attachments/v1/objects`.
fn objects(root: &std::path::Path) -> usize {
    let objects = root.join("attachments").join("v1").join("objects");
    let mut count = 0;
    let mut stack = vec![objects];
    while let Some(dir) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                stack.push(path);
            } else {
                count += 1;
            }
        }
    }
    count
}
