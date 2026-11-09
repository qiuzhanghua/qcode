//! Behaviour of the content-addressed store (ADR-0061 §2 and §7).

use std::fs;
use std::io::{Cursor, Read};
use std::path::Path;

use qcode_attachments::{
    AttachmentId, AttachmentStore, ImageMime, MAX_NAME_BYTES, NewImage, StoreError,
    sanitize_file_name,
};

fn store() -> (tempfile::TempDir, AttachmentStore) {
    let dir = tempfile::tempdir().expect("temp dir");
    let store = AttachmentStore::open(dir.path()).expect("open store");
    (dir, store)
}

fn files_in(dir: &Path) -> usize {
    let Ok(entries) = fs::read_dir(dir) else {
        return 0;
    };
    entries
        .filter_map(Result::ok)
        .map(|entry| {
            let path = entry.path();
            if path.is_dir() { files_in(&path) } else { 1 }
        })
        .sum()
}

#[test]
fn file_round_trips_and_lands_on_the_sharded_path() {
    let (_dir, store) = store();
    let bytes = b"# notes\n\nhello\n";
    let file = store
        .put_file("notes.md", Cursor::new(bytes))
        .expect("put file");

    assert_eq!(file.name, "notes.md");
    assert_eq!(file.bytes, bytes.len() as u64);

    let path = store.file_path(&file).expect("file path");
    let hex = file.id.to_hex();
    assert!(
        path.ends_with(format!("files/{}/{}/notes.md", &hex[..2], hex)),
        "{path:?}"
    );
    assert_eq!(store.read_file(&file).expect("read back"), bytes);
}

#[test]
fn the_same_bytes_are_stored_once() {
    let (_dir, store) = store();
    let first = store
        .put_file("a.txt", Cursor::new(b"same"))
        .expect("first");
    let second = store
        .put_file("a.txt", Cursor::new(b"same"))
        .expect("second");

    assert_eq!(first.id, second.id);
    assert_eq!(files_in(&store.root().join("files")), 1);
    assert_eq!(files_in(&store.root().join("tmp")), 0);
}

#[test]
fn different_names_under_one_content_share_the_directory() {
    let (_dir, store) = store();
    let a = store.put_file("a.txt", Cursor::new(b"shared")).expect("a");
    let b = store.put_file("b.txt", Cursor::new(b"shared")).expect("b");

    assert_eq!(a.id, b.id);
    assert_eq!(store.read_file(&a).unwrap(), b"shared");
    assert_eq!(store.read_file(&b).unwrap(), b"shared");
}

#[test]
fn oversized_files_are_rejected_and_leave_nothing_behind() {
    let (_dir, store) = store();
    let reader = std::io::repeat(0u8).take(17);
    let error = store
        .put_file_capped("big.bin", reader, 16)
        .expect_err("too large");

    assert!(
        matches!(error, StoreError::TooLarge { limit: 16 }),
        "{error:?}"
    );
    assert_eq!(files_in(&store.root().join("tmp")), 0);
    assert_eq!(files_in(&store.root().join("files")), 0);
}

#[test]
fn stored_objects_are_read_only() {
    let (_dir, store) = store();
    let file = store.put_file("a.txt", Cursor::new(b"ro")).expect("put");
    let path = store.file_path(&file).expect("path");
    let readonly = fs::metadata(&path)
        .expect("metadata")
        .permissions()
        .readonly();
    assert!(readonly, "{} should be read-only", path.display());
}

#[test]
fn a_deleted_object_reports_missing_instead_of_panicking() {
    let (_dir, store) = store();
    let file = store.put_file("a.txt", Cursor::new(b"gone")).expect("put");
    let path = store.file_path(&file).expect("path");
    fs::remove_file(&path).expect("remove");

    let error = store.read_file(&file).expect_err("missing");
    assert!(matches!(error, StoreError::Missing { .. }), "{error:?}");
    assert!(matches!(
        store.file_path(&file),
        Err(StoreError::Missing { .. })
    ));
}

#[test]
fn opening_the_store_clears_stale_temp_files() {
    let dir = tempfile::tempdir().expect("temp dir");
    let store = AttachmentStore::open(dir.path()).expect("open");
    let stale = store.root().join("tmp").join("leftover");
    fs::write(&stale, b"half written").expect("write");

    let reopened = AttachmentStore::open(dir.path()).expect("reopen");
    assert!(!stale.exists(), "stale temp file should be removed");
    assert_eq!(reopened.root(), store.root());
}

#[test]
fn images_round_trip_with_their_dimensions() {
    let (_dir, store) = store();
    let mut png = b"\x89PNG\r\n\x1a\n\0\0\0\rIHDR".to_vec();
    png.extend_from_slice(&1568u32.to_be_bytes());
    png.extend_from_slice(&1024u32.to_be_bytes());
    png.extend_from_slice(b"trailing payload");
    let image = store
        .put_image(NewImage {
            bytes: &png,
            mime: ImageMime::Png,
            name: Some("shot.png"),
        })
        .expect("put image");

    assert_eq!(image.mime, ImageMime::Png);
    assert_eq!((image.width, image.height), (1568, 1024));
    assert_eq!(image.bytes, png.len() as u64);
    assert!(image.origin.is_none(), "not normalized yet, so no original");
    assert_eq!(store.read_image(&image).unwrap(), png);

    let hex = image.id.to_hex();
    let path = store.image_path(&image);
    assert!(
        path.ends_with(format!("objects/{}/{hex}", &hex[..2])),
        "{path:?}"
    );
}

#[test]
fn a_mislabelled_image_is_rejected() {
    let (_dir, store) = store();
    let jpeg = [0xff, 0xd8, 0xff, 0x00, 0x01];
    let error = store
        .put_image(NewImage {
            bytes: &jpeg,
            mime: ImageMime::Png,
            name: None,
        })
        .expect_err("mismatch");

    match error {
        StoreError::MimeMismatch { declared, found } => {
            assert_eq!(declared, ImageMime::Png);
            assert_eq!(found, Some(ImageMime::Jpeg));
        }
        other => panic!("unexpected error: {other:?}"),
    }
    assert_eq!(files_in(&store.root().join("objects")), 0);
}

#[test]
fn file_names_are_reduced_to_one_safe_component() {
    assert_eq!(sanitize_file_name("../../etc/passwd"), "passwd");
    assert_eq!(sanitize_file_name(r"C:\Users\q\notes.md"), "notes.md");
    assert_eq!(sanitize_file_name("/tmp/dir/"), "dir");
    assert_eq!(sanitize_file_name("a:b?c*d|e\"f<g>h"), "a_b_c_d_e_f_g_h");
    assert_eq!(sanitize_file_name("белл\u{7}"), "белл");
    assert_eq!(sanitize_file_name("CON"), "_CON");
    assert_eq!(sanitize_file_name("con.txt"), "_con.txt");
    assert_eq!(sanitize_file_name(""), "file");
    assert_eq!(sanitize_file_name(".."), "file");
    assert_eq!(sanitize_file_name("   "), "file");
}

#[test]
fn long_names_are_truncated_on_a_char_boundary() {
    let name = "文".repeat(200);
    let sanitized = sanitize_file_name(&name);
    assert!(sanitized.len() <= MAX_NAME_BYTES);
    assert!(sanitized.is_char_boundary(sanitized.len()));
    assert!(std::str::from_utf8(sanitized.as_bytes()).is_ok());
}

#[test]
fn ids_round_trip_through_hex() {
    let id = AttachmentId::from_bytes([0x5a; 32]);
    assert_eq!(AttachmentId::from_hex(&id.to_hex()), Some(id));
}
