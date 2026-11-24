use std::sync::{Arc, Barrier};

use agent_tools::artifact::*;
use agent_tools::code_mode::{RpcError, ToolCallRequest, ToolRegistry};
use serde_json::{Value, json};

mod http;

fn note(text: &str, room: &str) -> CreateArtifact {
    CreateArtifact {
        workspace_id: Some("workspace_1".into()),
        room_id: Some(room.into()),
        title: "Garden notes".into(),
        kind: ArtifactKind::Markdown,
        mime_type: "text/markdown".into(),
        language: None,
        content: ArtifactContent::Text(text.into()),
    }
}

fn change(expected_version: u64, text: &str) -> UpdateArtifact {
    UpdateArtifact {
        expected_version,
        title: None,
        content: ArtifactContent::Text(text.into()),
    }
}

#[test]
fn local_versions_deduplicate_and_filter_after_reopen() {
    let temp = tempfile::tempdir().unwrap();
    let store = LocalArtifactBackend::new(temp.path()).unwrap();
    let first = store.create(note("a\nb\nc\n", "room_a")).unwrap();
    let second = store.create(note("a\nb\nc\n", "room_b")).unwrap();
    let blobs = temp.path().join(".artifacts").join("blobs");
    assert_eq!(std::fs::read_dir(&blobs).unwrap().count(), 1);
    assert_eq!(first.version, 1);
    assert_ne!(first.artifact_id, second.artifact_id);
    let id = &first.artifact_id;
    let v2 = store.update(id, change(1, "a\nx\nc\n")).unwrap();
    assert_eq!(v2.metadata.version, 2);
    assert_eq!(v2.metadata.created_at_ms, first.created_at_ms);
    assert!(v2.metadata.updated_at_ms >= first.updated_at_ms);
    let diff = v2.diff.unwrap();
    assert_eq!((diff.added_lines, diff.removed_lines), (1, 1));
    assert_eq!(
        diff.unified_diff.as_deref(),
        Some("--- previous\n+++ current\n@@ -2,1 +2,1 @@\n-b\n+x\n")
    );
    assert!(matches!(
        store.update(id, change(1, "conflict")),
        Err(ArtifactError::VersionConflict)
    ));
    assert_eq!(std::fs::read_dir(blobs).unwrap().count(), 2);
    let mut replacement = change(2, "a\nx\nc\nlast");
    replacement.title = Some("Revised notes".into());
    let v3 = store.update(id, replacement).unwrap();
    assert_eq!(v3.metadata.version, 3);
    assert_eq!(v3.metadata.title, "Revised notes");
    assert_eq!(
        v3.metadata.sha256,
        agent_tools::sha256_hex(b"a\nx\nc\nlast")
    );
    assert_eq!(v3.metadata.byte_size, 10);
    assert!(
        v3.diff
            .unwrap()
            .unified_diff
            .unwrap()
            .contains("@@ -3,0 +4,1 @@\n+last\n\\ No newline")
    );
    let store = LocalArtifactBackend::new(temp.path()).unwrap();
    assert_eq!(store.get(id, None).unwrap().metadata, v3.metadata);
    assert_eq!(store.get(id, Some(1)).unwrap().metadata, first);
    assert_eq!(
        store.get(id, Some(2)).unwrap().content,
        ArtifactContent::Text("a\nx\nc\n".into())
    );
    assert!(matches!(
        store.get(id, Some(4)),
        Err(ArtifactError::NotFound)
    ));
    let file = store
        .create(CreateArtifact {
            kind: ArtifactKind::File,
            mime_type: "application/octet-stream".into(),
            content: ArtifactContent::from_bytes(&[0, 255]),
            ..note("", "room_a")
        })
        .unwrap();
    let update = store
        .update(
            &file.artifact_id,
            UpdateArtifact {
                expected_version: 1,
                title: None,
                content: ArtifactContent::from_bytes(&[255, 0]),
            },
        )
        .unwrap();
    assert!(update.diff.is_none());
    assert_eq!(
        store
            .get(&file.artifact_id, None)
            .unwrap()
            .content
            .decode()
            .unwrap(),
        [255, 0]
    );
    let filter = ArtifactList {
        room_id: Some("room_a".into()),
        kind: Some(ArtifactKind::Markdown),
        ..ArtifactList::default()
    };
    assert_eq!(store.list(filter).unwrap().artifacts, [v3.metadata]);
    let mut seen = Vec::new();
    let mut after = None;
    loop {
        let page = store
            .list(ArtifactList {
                limit: Some(1),
                after,
                ..ArtifactList::default()
            })
            .unwrap();
        assert_eq!(page.artifacts.len(), 1);
        seen.push(page.artifacts[0].artifact_id.clone());
        after = page.next_after;
        if after.is_none() {
            break;
        }
    }
    let mut expected = [id.clone(), second.artifact_id.clone(), file.artifact_id];
    expected.sort();
    assert_eq!(seen, expected);
    store.delete(id).unwrap();
    assert!(matches!(
        store.get(id, Some(1)),
        Err(ArtifactError::NotFound)
    ));
    assert!(matches!(store.delete(id), Err(ArtifactError::NotFound)));
    assert_eq!(
        store
            .get(&second.artifact_id, None)
            .unwrap()
            .content
            .decode()
            .unwrap(),
        b"a\nb\nc\n"
    );
}

#[test]
fn two_local_backends_compete_for_one_version() {
    let temp = tempfile::tempdir().unwrap();
    let left = LocalArtifactBackend::new(temp.path()).unwrap();
    let right = LocalArtifactBackend::new(temp.path()).unwrap();
    let first = left.create(note("start", "room_a")).unwrap();
    let id = &first.artifact_id;
    let gate = Barrier::new(2);
    let (a, b) = std::thread::scope(|scope| {
        let a = scope.spawn(|| {
            gate.wait();
            left.update(id, change(1, "left"))
        });
        let b = scope.spawn(|| {
            gate.wait();
            right.update(id, change(1, "right"))
        });
        (a.join().unwrap(), b.join().unwrap())
    });
    let (winner, conflicted, text) = if a.is_ok() {
        (a, b, "left")
    } else {
        (b, a, "right")
    };
    assert_eq!(winner.unwrap().metadata.version, 2);
    assert!(matches!(conflicted, Err(ArtifactError::VersionConflict)));
    assert_eq!(
        left.get(id, None).unwrap().content.decode().unwrap(),
        text.as_bytes()
    );
}

#[test]
fn size_path_kind_and_mime_are_checked_before_writes() {
    let temp = tempfile::tempdir().unwrap();
    let store = LocalArtifactBackend::new(temp.path().join("work")).unwrap();
    let limit = "x".repeat(MAX_ARTIFACT_BYTES);
    let first = store.create(note(&limit, "room_a")).unwrap();
    let oversized = format!("{limit}x");
    let base64_too_large = CreateArtifact {
        content: ArtifactContent::from_bytes(oversized.as_bytes()),
        ..note("", "room_a")
    };
    assert!(matches!(
        store.create(base64_too_large),
        Err(ArtifactError::SizeLimit)
    ));
    assert!(matches!(
        store.update(&first.artifact_id, change(1, &oversized)),
        Err(ArtifactError::SizeLimit)
    ));
    assert_eq!(
        store
            .get(&first.artifact_id, None)
            .unwrap()
            .metadata
            .version,
        1
    );
    for id in ["../index", "a/b", "a%2fb", "a\\b", "..", "/tmp/x", "a?b"] {
        assert!(matches!(
            store.get(id, None),
            Err(ArtifactError::InvalidInput(_))
        ));
        assert!(matches!(
            store.delete(id),
            Err(ArtifactError::InvalidInput(_))
        ));
        assert!(matches!(
            store.create(note("x", id)),
            Err(ArtifactError::InvalidInput(_))
        ));
    }
    let wrong_mime = CreateArtifact {
        mime_type: "text/html".into(),
        ..note("x", "room_a")
    };
    assert!(matches!(
        store.create(wrong_mime),
        Err(ArtifactError::InvalidInput(_))
    ));
    let truncated = store
        .update(&first.artifact_id, change(1, "small"))
        .unwrap()
        .diff
        .unwrap();
    assert_eq!((truncated.added_lines, truncated.removed_lines), (1, 1));
    assert!(truncated.truncated && truncated.unified_diff.is_none());
}

#[cfg(unix)]
#[test]
fn local_store_rejects_symlink_and_corrupt_content() {
    use std::{fs, os::unix::fs::symlink};
    let root = tempfile::tempdir().unwrap();
    let outside = tempfile::tempdir().unwrap();
    symlink(outside.path(), root.path().join(".artifacts")).unwrap();
    assert!(matches!(
        LocalArtifactBackend::new(root.path()),
        Err(ArtifactError::Storage)
    ));
    fs::remove_file(root.path().join(".artifacts")).unwrap();
    let store = LocalArtifactBackend::new(root.path()).unwrap();
    let first = store.create(note("note", "room_a")).unwrap();
    let blob = root
        .path()
        .join(".artifacts")
        .join("blobs")
        .join(first.sha256);
    fs::write(&blob, "same").unwrap();
    assert!(matches!(
        store.get(&first.artifact_id, None),
        Err(ArtifactError::Storage)
    ));
    fs::remove_file(&blob).unwrap();
    let target = outside.path().join("untouched");
    fs::write(&target, "note").unwrap();
    symlink(&target, &blob).unwrap();
    assert!(matches!(
        store.get(&first.artifact_id, None),
        Err(ArtifactError::Storage)
    ));
    assert!(matches!(
        store.create(note("note", "room_a")),
        Err(ArtifactError::Storage)
    ));
    assert_eq!(fs::read_to_string(target).unwrap(), "note");
    assert_eq!(
        store.list(ArtifactList::default()).unwrap().artifacts.len(),
        1
    );
}

#[tokio::test]
async fn schema_and_mounted_tool_reject_bad_calls_and_preserve_cas_error() {
    let temp = tempfile::tempdir().unwrap();
    let backend = Arc::new(LocalArtifactBackend::new(temp.path()).unwrap());
    let tool = ArtifactTool::new(backend.clone());
    let schema = artifact_tool_schema();
    let branches = schema["oneOf"].as_array().unwrap();
    let actions: Vec<_> = branches
        .iter()
        .map(|x| x["properties"]["action"]["const"].as_str().unwrap())
        .collect();
    let dispatcher = ToolRegistry::new().with_artifact(backend).into_dispatcher();
    assert_eq!(dispatcher.specs()[0].namespace, "artifact");
    assert_eq!(dispatcher.specs()[0].methods, actions);
    assert_eq!(
        branches[0]["properties"]["kind"]["enum"],
        json!([
            "markdown", "code", "html", "svg", "mermaid", "chart", "table", "diff", "image", "file"
        ])
    );
    assert_eq!(
        branches[1]["required"],
        json!(["action", "artifact_id", "expected_version", "content"])
    );
    for bad in [
        json!({"action":"create","title":"note","kind":"unknown","mime_type":"text/plain","content":{"text":"a"}}),
        json!({"action":"create","title":"note","kind":"code","mime_type":"text/plain","content":{"text":"a","base64":"YQ=="}}),
        json!({"action":"create","title":"note","kind":"markdown","mime_type":"text/html","content":{"text":"a"}}),
        json!({"action":"get","artifact_id":"../index"}),
        json!({"action":"update","artifact_id":"abc","content":{"text":"a"}}),
        json!({"action":"get","artifact_id":"abc","byte_offset":1}),
        json!({"action":"delete","artifact_id":"abc","path":"abc"}),
    ] {
        assert!(tool.call(bad.clone()).await.is_err(), "accepted {bad}");
    }
    let created = tool.call(json!({"action":"create","title":"note","kind":"markdown","mime_type":"text/markdown","content":{"text":"a"}})).await.unwrap();
    let update = |expected_version| {
        ToolCallRequest::new(
            "fixture",
            "artifact",
            "update",
            json!({"artifact_id":created["artifact_id"],"expected_version":expected_version,"content":{"text":"b"}}),
        )
    };
    assert!(dispatcher.dispatch_one(update(1)).await.ok);
    let refused: RpcError = dispatcher.dispatch_one(update(1)).await.error.unwrap();
    assert_eq!(
        refused.data,
        Some(json!({"kind":"VersionConflict","status":409}))
    );
}

#[tokio::test]
async fn tool_content_continuation_is_bounded_and_pinned_to_a_version() {
    let temp = tempfile::tempdir().unwrap();
    let store = Arc::new(LocalArtifactBackend::new(temp.path()).unwrap());
    let tool = ArtifactTool::new(store.clone());
    let text = format!("{}{}", "\0".repeat(4095), "🪴".repeat(2100));
    let first = store.create(note(&text, "room_a")).unwrap();
    let id = first.artifact_id;
    let get = |offset: usize, version: Option<u64>| {
        let mut arg = json!({"action":"get", "artifact_id":id, "byte_offset":offset});
        if let Some(version) = version {
            arg["version"] = json!(version);
        }
        arg
    };
    let mut result = tool.call(get(0, None)).await.unwrap();
    assert_eq!(result["next_byte_offset"], 4095);
    store.update(&id, change(1, "new head")).unwrap();
    let mut rebuilt = String::new();
    loop {
        assert!(result.to_string().len() <= 30 * 1024);
        assert_eq!(result["metadata"]["version"], 1);
        rebuilt.push_str(result["content"]["text"].as_str().unwrap());
        let Some(offset) = result["next_byte_offset"].as_u64() else {
            break;
        };
        result = tool.call(get(offset as usize, Some(1))).await.unwrap();
    }
    assert_eq!(rebuilt, text);
    assert!(tool.call(get(4096, Some(1))).await.is_err());
    let meta = tool
        .call(json!({"action":"get","artifact_id":id,"metadata_only":true}))
        .await
        .unwrap();
    assert_eq!(meta["metadata"]["version"], 2);
    assert_eq!(meta["content"], Value::Null);
}
