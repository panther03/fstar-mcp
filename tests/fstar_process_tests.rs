#![cfg(unix)]

use fstar_mcp::fstar::{FStarConfig, FStarProcess};
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::time::Duration;

struct Fixture {
    directory: PathBuf,
    source: PathBuf,
    executable: PathBuf,
}

impl Fixture {
    fn new() -> Self {
        let directory = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("target")
            .join("process-tests")
            .join(format!("{}-{}", std::process::id(), uuid::Uuid::new_v4()));
        fs::create_dir_all(&directory).unwrap();
        let source = directory.join("Test.fst");
        fs::write(&source, "module Test\nlet x = 1\n").unwrap();
        let executable = directory.join("fake-fstar");
        fs::write(
            &executable,
            r#"#!/usr/bin/env python3
import json
import sys

print(json.dumps({"kind": "protocol-info", "version": 3, "features": ["full-buffer", "vfs-add", "lookup"]}), flush=True)
active = None
for line in sys.stdin:
    request = json.loads(line)
    qid = request["query-id"]
    query = request["query"]
    if query == "full-buffer":
        code = request["args"]["code"]
        if qid == "1":
            print(json.dumps({"query-id": "10", "kind": "response", "status": "success", "response": [{"message": "wrong query", "number": 1, "level": "error", "ranges": []}]}), flush=True)
        print(json.dumps({"query-id": qid, "kind": "message", "level": "progress", "contents": {"stage": "full-buffer-started"}}), flush=True)
        if "WAIT" in code:
            active = qid
            continue
        rng = {"fname": "Test.fst", "beg": [1, 0], "end": [2, 9]}
        print(json.dumps({"query-id": qid + ".1", "kind": "message", "level": "progress", "contents": {"stage": "full-buffer-fragment-started", "ranges": rng}}), flush=True)
        print(json.dumps({"query-id": qid + ".2", "kind": "message", "level": "progress", "contents": {"stage": "full-buffer-fragment-ok", "ranges": rng}}), flush=True)
        print(json.dumps({"query-id": qid + ".3", "kind": "message", "level": "progress", "contents": {"stage": "full-buffer-finished"}}), flush=True)
    elif query == "cancel":
        if active is not None:
            print(json.dumps({"query-id": active + ".1", "kind": "message", "level": "progress", "contents": {"stage": "full-buffer-finished"}}), flush=True)
            active = None
    elif query in ("vfs-add", "lookup"):
        print(json.dumps({"query-id": qid, "kind": "response", "status": "success", "response": None}), flush=True)
"#,
        )
        .unwrap();
        let mut permissions = fs::metadata(&executable).unwrap().permissions();
        permissions.set_mode(0o755);
        fs::set_permissions(&executable, permissions).unwrap();
        Self {
            directory,
            source,
            executable,
        }
    }

    fn config(&self) -> FStarConfig {
        FStarConfig {
            fstar_exe: Some(self.executable.to_string_lossy().into_owned()),
            cwd: Some(self.directory.to_string_lossy().into_owned()),
            ..FStarConfig::default()
        }
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.directory);
    }
}

#[tokio::test]
async fn full_buffer_uses_exact_query_id_roots_and_buffers_other_responses() {
    let fixture = Fixture::new();
    let mut process = FStarProcess::spawn(fixture.config(), &fixture.source, false)
        .await
        .unwrap();

    let result = process
        .full_buffer_query(
            "module Test\nlet x = 1\n",
            "full",
            None,
            Duration::from_secs(2),
        )
        .await
        .unwrap();

    assert!(result.finished);
    assert!(!result.timed_out);
    assert!(result.diagnostics.is_empty());
    assert_eq!(result.fragments.len(), 1);
    process
        .vfs_add(Some("Test.fst"), "module Test")
        .await
        .unwrap();
}

#[tokio::test]
async fn a_new_request_can_cancel_an_in_flight_full_buffer_query() {
    let fixture = Fixture::new();
    let mut process = FStarProcess::spawn(fixture.config(), &fixture.source, false)
        .await
        .unwrap();
    let control = process.control();
    let check = tokio::spawn(async move {
        process
            .full_buffer_query(
                "module Test\n// WAIT\n",
                "full",
                None,
                Duration::from_secs(2),
            )
            .await
    });

    tokio::time::sleep(Duration::from_millis(100)).await;
    assert!(control.cancel((2, 0)).await.unwrap());
    let result = check.await.unwrap().unwrap();
    assert!(result.finished);
}

#[tokio::test]
async fn full_buffer_timeout_returns_partial_progress() {
    let fixture = Fixture::new();
    let mut process = FStarProcess::spawn(fixture.config(), &fixture.source, false)
        .await
        .unwrap();
    let result = process
        .full_buffer_query(
            "module Test\n// WAIT\n",
            "full",
            None,
            Duration::from_millis(100),
        )
        .await
        .unwrap();
    assert!(result.timed_out);
    assert!(!result.finished);
}

#[tokio::test]
#[ignore = "requires fstar.exe on PATH"]
async fn real_fstar_typechecks_a_fixture() {
    let directory = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("target")
        .join("real-fstar-test");
    fs::create_dir_all(&directory).unwrap();
    let source = directory.join("RealFixture.fst");
    fs::write(&source, "module RealFixture\nlet x : int = 1\n").unwrap();
    let config =
        FStarConfig::discover(&source, Some(Path::new(env!("CARGO_MANIFEST_DIR")))).unwrap();
    let mut process = FStarProcess::spawn(config, &source, false).await.unwrap();
    let result = process
        .full_buffer_query(
            &fs::read_to_string(&source).unwrap(),
            "full",
            None,
            Duration::from_secs(30),
        )
        .await
        .unwrap();
    assert!(result.finished);
    assert!(result
        .diagnostics
        .iter()
        .all(|diagnostic| diagnostic.level != "error"));
}
