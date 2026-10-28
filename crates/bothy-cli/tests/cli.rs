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

#[test]
fn bench_propagate_over_a_local_channel() {
    let dir = tempfile::tempdir().unwrap();
    let remote = format!("path:{}", dir.path().join("remote").display());
    let m = Machine::new(dir.path(), "m");
    m.json(&["--db", "a", "init", "--remote", &remote]);
    m.json(&["--db", "b", "join", "--remote", &remote]);
    let r = m.json(&[
        "--db",
        "a",
        "bench",
        "propagate",
        "--peer",
        "b",
        "--count",
        "3",
        "--interval-ms",
        "50",
        "--p2p",
        "--no-stun",
        "--host",
        "127.0.0.1",
    ]);
    assert_eq!(r["arrived"], 3, "{r}");
    assert!(r["p50_ms"].as_u64().unwrap() < 2000, "{r}");
    assert!(r["connect_ms"].as_u64().is_some(), "{r}");
    // --peer must share the remote database.
    let other = format!("path:{}", dir.path().join("other").display());
    m.json(&["--db", "c", "init", "--remote", &other]);
    let (code, _) = m.fails_with(
        &[
            "--db",
            "a",
            "bench",
            "propagate",
            "--peer",
            "c",
            "--p2p",
            "--no-stun",
        ],
        &[],
        None,
    );
    assert_eq!(code, 2);
}

/// `sync --watch` stops on Ctrl-C even while it wakes up every second for P2P status.
#[cfg(unix)]
#[test]
fn sync_watch_stops_on_sigint() {
    let (_dir, a, _b, _) = setup();
    let mut child = Command::new(env!("CARGO_BIN_EXE_bothy"))
        .args(["sync", "--watch", "--p2p", "--no-stun"])
        .env("BOTHY_HOME", &a.home)
        .env("BOTHY_TOKEN_STORE", "file")
        .env_remove("BOTHY_DB")
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .unwrap();
    for _ in 0..3 {
        std::thread::sleep(std::time::Duration::from_millis(1500));
        let _ = Command::new("kill")
            .args(["-INT", &child.id().to_string()])
            .status();
        for _ in 0..50 {
            if child.try_wait().unwrap().is_some() {
                return;
            }
            std::thread::sleep(std::time::Duration::from_millis(100));
        }
    }
    let _ = child.kill();
    panic!("sync --watch did not stop on SIGINT");
}

/// `bench propagate --reverse` times writes that `bench echo` makes on another machine.
#[cfg(unix)]
#[test]
fn bench_reverse_times_writes_from_the_other_machine() {
    let (_dir, a, b, _) = setup();
    let mut echo = Command::new(env!("CARGO_BIN_EXE_bothy"))
        .args(["bench", "echo", "--p2p", "--no-stun", "--host", "127.0.0.1"])
        .env("BOTHY_HOME", &b.home)
        .env("BOTHY_TOKEN_STORE", "file")
        .env_remove("BOTHY_DB")
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .unwrap();
    let r = a.json(&[
        "bench",
        "propagate",
        "--reverse",
        "--count",
        "3",
        "--interval-ms",
        "100",
        "--timeout",
        "30",
        "--p2p",
        "--no-stun",
        "--host",
        "127.0.0.1",
    ]);
    let _ = echo.kill();
    let _ = echo.wait();
    assert_eq!(r["arrived"], 3, "{r}");
    assert!(r["p50_ms"].as_u64().unwrap() < 5000, "{r}");
}

#[test]
fn setup_google_checks_and_stores_the_client() {
    let dir = tempfile::tempdir().unwrap();
    let m = Machine::new(dir.path(), "m");
    let id = "123-abc.apps.googleusercontent.com";
    let desktop = dir.path().join("desktop.json");
    std::fs::write(
        &desktop,
        format!(r#"{{"installed":{{"client_id":"{id}","client_secret":"s","project_id":"my-sync-1","redirect_uris":["http://localhost"]}}}}"#),
    )
    .unwrap();
    let web = dir.path().join("web.json");
    std::fs::write(
        &web,
        format!(r#"{{"web":{{"client_id":"{id}","client_secret":"s","project_id":"my-sync-1"}}}}"#),
    )
    .unwrap();
    let args = |project: &str, file: &std::path::Path| {
        vec![
            "setup".to_owned(),
            "google".into(),
            "--project".into(),
            project.into(),
            "--client".into(),
            file.display().to_string(),
            "--no-browser".into(),
            "--no-login".into(),
        ]
    };
    let ok: Vec<String> = args("my-sync-1", &desktop);
    let r = m.json(&ok.iter().map(String::as_str).collect::<Vec<_>>());
    assert_eq!(r["client_id"], id, "{r}");
    assert!(m.ok(&["auth", "status"]).contains("(project my-sync-1)"));

    let wrong_kind = args("my-sync-1", &web);
    let (code, err) = m.fails_with(
        &wrong_kind.iter().map(String::as_str).collect::<Vec<_>>(),
        &[],
        None,
    );
    assert_eq!(code, 2);
    assert!(
        err.contains("Web application") && err.contains("Desktop app"),
        "{err}"
    );

    let other_project = args("other-proj", &desktop);
    let (code, err) = m.fails_with(
        &other_project.iter().map(String::as_str).collect::<Vec<_>>(),
        &[],
        None,
    );
    assert_eq!(code, 2);
    assert!(err.contains("my-sync-1"), "{err}");

    let bad_id = args("Bad_Id", &desktop);
    let (code, _) = m.fails_with(
        &bad_id.iter().map(String::as_str).collect::<Vec<_>>(),
        &[],
        None,
    );
    assert_eq!(code, 2);
}

#[test]
fn auth_status_shows_the_project_of_each_client() {
    let dir = tempfile::tempdir().unwrap();
    let m = Machine::new(dir.path(), "m");
    let desktop = dir.path().join("desktop.json");
    std::fs::write(
        &desktop,
        r#"{"installed":{"client_id":"1-d.apps.googleusercontent.com","client_secret":"desk-secret","project_id":"my-sync-1","redirect_uris":["http://localhost"]}}"#,
    )
    .unwrap();
    let tv = dir.path().join("tv.json");
    std::fs::write(
        &tv,
        r#"{"installed":{"client_id":"1-t.apps.googleusercontent.com","client_secret":"tv-secret","project_id":"other-proj"}}"#,
    )
    .unwrap();
    let (d, t) = (desktop.display().to_string(), tv.display().to_string());
    m.ok(&["auth", "setup", "--client", &d, "--device-client", &t]);

    let st = m.json(&["auth", "status"]);
    assert_eq!(st["signed_in"], false);
    assert_eq!(st["clients"][0]["kind"], "desktop", "{st}");
    assert_eq!(st["clients"][0]["project"], "my-sync-1", "{st}");
    assert_eq!(
        st["clients"][1]["client_id"], "1-t.apps.googleusercontent.com",
        "{st}"
    );
    assert_eq!(st["clients"][1]["project"], "other-proj", "{st}");
    // A BOTHY_HOME gets its own keyring entry.
    assert!(
        st["keyring_account"]
            .as_str()
            .unwrap()
            .starts_with("google:"),
        "{st}"
    );

    let text = m.ok(&["auth", "status"]);
    assert!(
        text.contains("desktop 1-d.apps.googleusercontent.com (project my-sync-1)"),
        "{text}"
    );
    assert!(text.contains("(project other-proj)"), "{text}");
    for out in [text, st.to_string()] {
        assert!(!out.contains("secret"), "{out}");
    }
}

#[test]
fn switching_clients_asks_for_a_new_sign_in() {
    let dir = tempfile::tempdir().unwrap();
    let m = Machine::new(dir.path(), "m");
    let desktop = dir.path().join("desktop.json");
    std::fs::write(
        &desktop,
        r#"{"installed":{"client_id":"1-d.apps.googleusercontent.com","client_secret":"s","project_id":"my-sync-1","redirect_uris":["http://localhost"]}}"#,
    )
    .unwrap();
    m.ok(&["auth", "setup", "--client", &desktop.display().to_string()]);
    // A sign-in made with another client, as after switching from the official client.
    std::fs::write(
        m.home.join("config/token.json"),
        r#"{"refresh_token":"r","scopes":["https://www.googleapis.com/auth/drive.file"],"client":"desktop","client_id":"2-x.apps.googleusercontent.com"}"#,
    )
    .unwrap();
    let st = m.json(&["auth", "status"]);
    assert_eq!(st["needs_login"], true, "{st}");
    assert!(
        m.ok(&["auth", "status"])
            .contains("run `bothy auth login` again")
    );

    // --official forgets the stored clients.
    let r = m.json(&["auth", "setup", "--official"]);
    assert_eq!(r["clients"], "official", "{r}");
    assert!(!m.home.join("config/client-desktop.json").exists());
    let st = m.json(&["auth", "status"]);
    assert_eq!(st["clients_configured"], serde_json::json!([]), "{st}");
    let official = st["clients"].as_array().unwrap();
    assert!(official.iter().all(|c| c["official"] == true), "{st}");
}
