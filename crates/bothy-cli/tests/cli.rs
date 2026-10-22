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
        self.run_with(args, &[], None)
    }

    /// Runs with extra environment variables and, optionally, text on stdin.
    fn run_with(&self, args: &[&str], env: &[(&str, &str)], stdin: Option<&str>) -> Output {
        use std::io::Write;
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_bothy"));
        cmd.args(args)
            .env("BOTHY_HOME", &self.home)
            .env("BOTHY_TOKEN_STORE", "file")
            .env_remove("BOTHY_DB")
            .env_remove("BOTHY_PASSWORD")
            .env_remove("BOTHY_NEW_PASSWORD")
            .envs(env.iter().copied())
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped());
        let mut child = cmd.spawn().expect("bothy runs");
        let mut input = child.stdin.take().unwrap();
        input.write_all(stdin.unwrap_or("").as_bytes()).unwrap();
        drop(input);
        child.wait_with_output().unwrap()
    }

    fn ok_with(&self, args: &[&str], env: &[(&str, &str)], stdin: Option<&str>) -> J {
        let mut a = vec!["--json"];
        a.extend_from_slice(args);
        let out = self.run_with(&a, env, stdin);
        assert!(
            out.status.success(),
            "bothy {args:?} failed ({:?}):\n{}",
            out.status.code(),
            String::from_utf8_lossy(&out.stderr)
        );
        serde_json::from_slice(&out.stdout).unwrap()
    }

    fn fails_with(
        &self,
        args: &[&str],
        env: &[(&str, &str)],
        stdin: Option<&str>,
    ) -> (i32, String) {
        let out = self.run_with(args, env, stdin);
        assert!(!out.status.success(), "bothy {args:?} should fail");
        (
            out.status.code().unwrap(),
            String::from_utf8_lossy(&out.stderr).into_owned(),
        )
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
    // The scan reads raw bytes: it finds the app id in the (always plain) manifest.
    let (code, err) = a.fails_with(&["inspect", "scan", "dev.bothy.cli"], &[], None);
    assert_eq!(code, 1, "{err}");
    assert!(err.contains("plain text found"), "{err}");
    let (code, err) = a.fails_with(&["unlock"], &[("BOTHY_PASSWORD", "x")], None);
    assert_eq!(code, 2);
    assert!(err.contains("not encrypted"), "{err}");
    assert_eq!(a.json(&["status"])["encrypted"], false);
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

fn remote_contains(dir: &Path, needle: &[u8]) -> bool {
    std::fs::read_dir(dir).unwrap().any(|e| {
        let p = e.unwrap().path();
        if p.is_dir() {
            remote_contains(&p, needle)
        } else {
            std::fs::read(&p)
                .unwrap()
                .windows(needle.len())
                .any(|w| w == needle)
        }
    })
}

#[test]
fn encrypted_database_from_init_to_password_change() {
    let dir = tempfile::tempdir().unwrap();
    let remote_dir = dir.path().join("remote");
    let remote = format!("path:{}", remote_dir.display());
    let (a, b, c) = (
        Machine::new(dir.path(), "a"),
        Machine::new(dir.path(), "b"),
        Machine::new(dir.path(), "c"),
    );
    let pw = [("BOTHY_PASSWORD", "hunter2")];

    let init = a.ok_with(
        &["init", "--encrypt", "--remember", "--remote", &remote],
        &pw,
        None,
    );
    assert_eq!(
        (init["encrypted"].clone(), init["key_remembered"].clone()),
        (J::Bool(true), J::Bool(true))
    );
    a.ok(&["put", "notes", "n1", r#"{"t":"SECRET-MARKER-7"}"#]);
    a.ok(&["sync"]); // the remembered key: no password needed
    assert!(!remote_contains(&remote_dir, b"SECRET-MARKER-7"));
    assert!(!remote_contains(&remote_dir, b"notes"));
    assert_eq!(
        a.json(&["inspect", "scan", "SECRET-MARKER-7"])["found"],
        serde_json::json!([])
    );

    // No password and no terminal: a usage error, not a hang.
    let (code, err) = b.fails_with(&["join", "--remote", &remote], &[], None);
    assert_eq!(code, 2, "{err}");
    assert!(err.contains("BOTHY_PASSWORD"), "{err}");
    let (code, err) = b.fails_with(
        &["--password-stdin", "join", "--remote", &remote],
        &[],
        Some("wrong\n"),
    );
    assert_eq!(code, 1, "{err}");
    assert!(err.contains("password"), "{err}");
    let joined = b.ok_with(
        &["--password-stdin", "join", "--remote", &remote],
        &[],
        Some("hunter2\n"),
    );
    assert_eq!(joined["encrypted"], true);
    assert_eq!(
        b.json(&["get", "notes", "n1"]),
        serde_json::json!({"t": "SECRET-MARKER-7"})
    );

    // Not remembered: locked, and syncing needs the password.
    let st = b.json(&["status"]);
    assert_eq!(
        (st["encrypted"].clone(), st["locked"].clone()),
        (J::Bool(true), J::Bool(true))
    );
    assert_eq!(b.fails_with(&["sync"], &[], None).0, 2);
    b.ok_with(&["sync"], &pw, None);
    b.ok_with(
        &["--password-stdin", "unlock", "--remember"],
        &[],
        Some("hunter2\n"),
    );
    assert_eq!(b.json(&["status"])["locked"], false);

    // Password change on a: the old one stops working, b's remembered key keeps working.
    let changed = a.ok_with(
        &["--password-stdin", "passwd"],
        &[],
        Some("hunter2\nnew-pass\n"),
    );
    assert_eq!(changed["changed"], true);
    b.ok(&["put", "notes", "n2", r#"{"t":"from b"}"#]);
    b.ok(&["sync"]);
    a.ok(&["sync"]);
    assert_eq!(
        a.json(&["get", "notes", "n2"]),
        serde_json::json!({"t": "from b"})
    );
    assert_eq!(c.fails_with(&["join", "--remote", &remote], &pw, None).0, 1);
    c.ok_with(
        &["join", "--remote", &remote],
        &[("BOTHY_PASSWORD", "new-pass")],
        None,
    );

    // lock forgets the key; the old password no longer unlocks.
    assert_eq!(b.json(&["lock"])["forgotten"], true);
    assert_eq!(b.json(&["status"])["locked"], true);
    assert_eq!(b.fails_with(&["unlock"], &pw, None).0, 1);
    b.ok_with(&["unlock"], &[("BOTHY_PASSWORD", "new-pass")], None);
    assert_eq!(
        a.fails_with(&["unlock"], &[], None).0,
        2,
        "unlock always checks a password, even with a remembered key"
    );
}
