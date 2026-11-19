//! Behavioral integration tests for `LocalWorkspace` against a real temporary
//! directory and real `/bin/sh`.

use std::path::PathBuf;

use agent_tools::{LocalWorkspace, WorkspaceBackend, base64_decode, base64_encode, sha256_hex};

fn workspace() -> (tempfile::TempDir, LocalWorkspace) {
    let dir = tempfile::tempdir().expect("tempdir");
    let ws = LocalWorkspace::new(dir.path().to_path_buf());
    (dir, ws)
}

#[test]
fn write_then_read_roundtrip_through_real_filesystem() {
    let (_dir, ws) = workspace();
    let content = b"hello workspace\nline two\n";
    let (resolved, total, sha) = ws
        .write_file_bytes(Some("nested/dir/greeting.txt"), content, false, false)
        .expect("write nested file");
    assert_eq!(total, content.len());
    assert_eq!(sha, sha256_hex(content));
    let on_disk: PathBuf = resolved.clone().into();
    assert_eq!(std::fs::read(&on_disk).expect("read from disk"), content);

    let (resolved2, bytes) = ws
        .read_file_bytes(Some("nested/dir/greeting.txt"))
        .expect("read back");
    assert_eq!(resolved, resolved2);
    assert_eq!(bytes, content);
}

#[test]
fn overwrite_guard_append_and_overwrite() {
    let (_dir, ws) = workspace();
    ws.write_file_bytes(Some("f.txt"), b"one", false, false)
        .expect("first write");
    let err = ws
        .write_file_bytes(Some("f.txt"), b"two", false, false)
        .expect_err("second write without overwrite must fail");
    assert!(err.contains("already exists"), "unexpected error: {err}");

    let (_, total, _) = ws
        .write_file_bytes(Some("f.txt"), b"two", true, false)
        .expect("append");
    assert_eq!(total, 6);
    let (_, bytes) = ws
        .read_file_bytes(Some("f.txt"))
        .expect("read after append");
    assert_eq!(bytes, b"onetwo");

    let (_, total, _) = ws
        .write_file_bytes(Some("f.txt"), b"three", false, true)
        .expect("overwrite");
    assert_eq!(total, 5);
    let (_, bytes) = ws
        .read_file_bytes(Some("f.txt"))
        .expect("read after overwrite");
    assert_eq!(bytes, b"three");
}

#[test]
fn read_and_list_negative_edges() {
    let (_dir, ws) = workspace();
    std::fs::write(_dir.path().join("plain.txt"), b"x").expect("seed file");
    assert!(ws.read_file_bytes(Some("missing.txt")).is_err());
    assert!(ws.read_file_bytes(Some("")).is_err());
    assert!(ws.read_file_bytes(None).is_err());
    let err = ws.read_file_bytes(Some("")).expect_err("empty path");
    assert!(err.contains("must not be empty"), "unexpected error: {err}");

    std::fs::create_dir(_dir.path().join("adir")).expect("seed dir");
    assert!(ws.read_file_bytes(Some("adir")).is_err());
    assert!(ws.list_dir(Some("no-such-dir"), false, 10).is_err());

    std::fs::write(_dir.path().join(".hidden"), b"h").expect("seed hidden");
    let listing = ws.list_dir(None, false, 10).expect("list root");
    let names: Vec<&str> = listing
        .get("entries")
        .and_then(|e| e.as_array())
        .expect("list returns entries array")
        .iter()
        .filter_map(|e| e.get("name").and_then(|n| n.as_str()))
        .collect();
    assert!(names.contains(&"plain.txt"));
    assert!(!names.contains(&".hidden"));

    let listing_with_hidden = ws.list_dir(None, true, 10).expect("list root with hidden");
    let names_h: Vec<&str> = listing_with_hidden
        .get("entries")
        .and_then(|e| e.as_array())
        .expect("list returns entries array")
        .iter()
        .filter_map(|e| e.get("name").and_then(|n| n.as_str()))
        .collect();
    assert!(names_h.contains(&".hidden"));
}

#[test]
fn exec_runs_real_shell_and_reports_exit_codes() {
    let (_dir, ws) = workspace();
    let out = ws
        .exec("printf 'hello world'", None, None)
        .expect("exec printf");
    assert_eq!(out.exit_code, 0);
    assert_eq!(out.stdout, "hello world");

    let out = ws
        .exec("printf err >&2; exit 7", None, None)
        .expect("exec failing cmd");
    assert_eq!(out.exit_code, 7);
    assert_eq!(out.stderr, "err");

    assert!(ws.exec("   ", None, None).is_err());
    assert!(ws.exec("true", Some("no-such-cwd"), None).is_err());

    std::fs::create_dir(_dir.path().join("sub")).expect("seed subdir");
    std::fs::write(_dir.path().join("sub/mark.txt"), b"here").expect("seed marker");
    let out = ws
        .exec("cat mark.txt", Some("sub"), None)
        .expect("exec with cwd");
    assert_eq!(out.stdout, "here");
}

#[test]
fn path_resolution_and_base64_edges() {
    let (_dir, ws) = workspace();
    assert!(ws.read_file_bytes(Some("a\0b")).is_err());
    std::fs::write(_dir.path().join("up.txt"), b"up").expect("seed up");
    let (resolved, _) = ws
        .read_file_bytes(Some("sub/../up.txt"))
        .expect("dotdot normalized");
    assert!(resolved.ends_with("up.txt"));

    assert_eq!(base64_encode(b""), "");
    assert_eq!(base64_encode(b"foobar"), "Zm9vYmFy");
    for len in 0..=64usize {
        let data: Vec<u8> = (0..len as u8).collect();
        let enc = base64_encode(&data);
        assert_eq!(base64_decode(&enc).expect("decode own encoding"), data);
    }
    assert_eq!(base64_decode("Zm9v\nYmFy").expect("whitespace"), b"foobar");
    assert_eq!(base64_decode("-w==").expect("url-safe"), [0xfb]);
    assert!(base64_decode("Zm9").is_err());
    assert!(base64_decode("####").is_err());
}
