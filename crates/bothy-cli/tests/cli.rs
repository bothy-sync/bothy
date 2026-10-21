//! End-to-end CLI runs: two "machines" (separate BOTHY_HOME directories) share a folder remote.

use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use serde_json::Value as J;

struct Machine {
    home: PathBuf,
}

impl Machine {
    fn new(root: &Path, name: &str) -> Self {
        Self {
            home: root.join(name),
        }
    }

    fn run(&self, args: &[&str]) -> Output {
        Command::new(env!("CARGO_BIN_EXE_bothy"))
            .args(args)
            .env("BOTHY_HOME", &self.home)
            .env("BOTHY_TOKEN_STORE", "file")
            .env_remove("BOTHY_DB")
            .output()
            .expect("bothy runs")
    }

    fn ok(&self, args: &[&str]) -> String {
        let out = self.run(args);
        assert!(
            out.status.success(),
            "bothy {args:?} failed ({:?}):\n{}",
            out.status.code(),
            String::from_utf8_lossy(&out.stderr)
        );
        String::from_utf8(out.stdout).unwrap()
    }

    fn json(&self, args: &[&str]) -> J {
        let mut a = vec!["--json"];
        a.extend_from_slice(args);
        serde_json::from_str(self.ok(&a).trim()).unwrap()
    }
}

fn setup() -> (tempfile::TempDir, Machine, Machine, String) {
    let dir = tempfile::tempdir().unwrap();
    let remote = format!("path:{}", dir.path().join("remote").display());
    let (a, b) = (Machine::new(dir.path(), "a"), Machine::new(dir.path(), "b"));
    let db_id = a.json(&["init", "--remote", &remote])["db_id"]
        .as_str()
        .unwrap()
        .to_owned();
    b.json(&["join", "--remote", &remote]);
    (dir, a, b, db_id)
}

#[test]
fn two_machines_converge_and_export_identically() {
    let (_dir, a, b, _) = setup();
    a.ok(&["put", "notes", "n1", r#"{"title":"from a","pinned":false}"#]);
    b.ok(&["put", "todos", "t1", r#"{"done":false}"#]);
    for m in [&a, &b, &a] {
        m.ok(&["sync"]);
    }
    let (ea, eb) = (a.ok(&["export", "--all"]), b.ok(&["export", "--all"]));
    assert_eq!(ea, eb, "export --all must be byte-identical");
    assert_eq!(ea.lines().count(), 2);
}

#[test]
fn offline_edits_merge_and_the_loser_shows_in_meta() {
    let (_dir, a, b, _) = setup();
    a.ok(&["put", "notes", "n1", r#"{"title":"draft"}"#]);
    a.ok(&["sync"]);
    b.ok(&["sync"]);

    // Different fields: both survive.
    a.ok(&["put", "notes", "n1", r#"{"title":"A"}"#, "--patch"]);
    b.ok(&["put", "notes", "n1", r#"{"tags":["x"]}"#, "--patch"]);
    // Same field: the later write wins.
    a.ok(&["put", "notes", "n2", r#"{"v":"first"}"#]);
    std::thread::sleep(std::time::Duration::from_millis(5));
    b.ok(&["put", "notes", "n2", r#"{"v":"second"}"#]);
    for m in [&a, &b, &a] {
        m.ok(&["sync"]);
    }
    assert_eq!(
        a.json(&["get", "notes", "n1"]),
        serde_json::json!({"title": "A", "tags": ["x"]})
    );
    assert_eq!(
        a.json(&["get", "notes", "n2"]),
        serde_json::json!({"v": "second"})
    );
    let meta = a.json(&["get", "notes", "n2", "--with-meta"]);
    assert_eq!(meta["conflicts"][0]["loser_value"], "first");
    assert_eq!(meta["fields"]["v"]["visible"], true);

    let all = a.json(&["conflicts"]);
    assert!(
        all.as_array()
            .unwrap()
            .iter()
            .any(|c| c["id"] == "n2" && c["loser_value"] == "first"),
        "{all}"
    );
    assert_eq!(a.json(&["conflicts", "--since", "1h"]), all);
    assert_eq!(
        a.json(&["conflicts", "--since", "2999-01-01"]),
        serde_json::json!([])
    );
    let cleared = a.json(&["conflicts", "--clear"]);
    assert_eq!(cleared["cleared"], all.as_array().unwrap().len());
    assert_eq!(a.json(&["conflicts"]), serde_json::json!([]));
}

#[test]
fn delete_wins_over_an_older_edit() {
    let (_dir, a, b, _) = setup();
    a.ok(&["put", "notes", "n1", r#"{"title":"x"}"#]);
    a.ok(&["sync"]);
    b.ok(&["sync"]);
    b.ok(&[
        "put",
        "notes",
        "n1",
        r#"{"body":"offline edit"}"#,
        "--patch",
    ]);
    std::thread::sleep(std::time::Duration::from_millis(5));
    a.ok(&["del", "notes", "n1"]);
    for m in [&a, &b, &a] {
        m.ok(&["sync"]);
    }
    for m in [&a, &b] {
        let out = m.run(&["get", "notes", "n1"]);
        assert_eq!(out.status.code(), Some(1), "deleted document stays deleted");
    }
}

#[test]
fn third_machine_joins_from_a_snapshot() {
    let (dir, a, _b, db_id) = setup();
    for i in 0..5 {
        a.ok(&["put", "notes", &format!("n{i}"), &format!(r#"{{"i":{i}}}"#)]);
        a.ok(&["sync"]);
    }
    a.ok(&["snapshot", "create"]);
    let c = Machine::new(dir.path(), "c");
    let remote = format!("path:{}", dir.path().join("remote").display());
    let joined = c.json(&["join", "--remote", &remote, "--db-id", &db_id]);
    assert_eq!(joined["report"]["snapshots_applied"], 1);
    assert_eq!(joined["report"]["segments_down"], 0);
    assert_eq!(a.ok(&["export", "--all"]), c.ok(&["export", "--all"]));
}

#[test]
fn status_inspect_and_usage_errors() {
    let (_dir, a, _b, db_id) = setup();
    a.ok(&["put", "notes", "n1", "{}"]);
    let st = a.json(&["status"]);
    assert_eq!(st["db_id"], db_id.as_str());
    assert_eq!(st["unsynced_ops"], 0, "an empty document creates no ops");
    a.ok(&["put", "notes", "n1", r#"{"a":1}"#]);
    assert_eq!(a.json(&["status"])["unsynced_ops"], 1);
    a.ok(&["sync"]);
    let files = a.json(&["inspect", "remote"]);
    assert!(
        files
            .as_array()
            .unwrap()
            .iter()
            .any(|f| f["kind"] == "segment")
    );

    assert_eq!(
        a.run(&["put", "notes", "n1", "[1]"]).status.code(),
        Some(2),
        "not an object"
    );
    assert_eq!(
        a.run(&["--db", "missing", "status"]).status.code(),
        Some(2),
        "unknown db"
    );
    assert_eq!(
        a.run(&["put", "Bad Name", "x", "{}"]).status.code(),
        Some(2),
        "bad collection"
    );
    assert_eq!(
        a.run(&["devices", "prune", "00"]).status.code(),
        Some(2),
        "prune needs --yes"
    );
    assert_eq!(
        a.run(&["nonsense"]).status.code(),
        Some(2),
        "clap usage error"
    );
}

#[test]
fn drive_databases_need_a_sign_in() {
    let dir = tempfile::tempdir().unwrap();
    let m = Machine::new(dir.path(), "m");
    let out = m.run(&["init", "--remote", "appdata"]);
    assert_eq!(out.status.code(), Some(3));
    assert!(String::from_utf8_lossy(&out.stderr).contains("auth login"));
    let st = m.json(&["auth", "status"]);
    assert_eq!(st["signed_in"], false);
}
