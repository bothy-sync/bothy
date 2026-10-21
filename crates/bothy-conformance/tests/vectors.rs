//! The committed vectors pass, and regenerating them changes nothing except zstd bytes.

use std::path::PathBuf;

fn vectors_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../spec/vectors")
}

#[test]
fn committed_vectors_pass() {
    let n = bothy_conformance::check::check_dir(&vectors_dir()).unwrap();
    assert!(n > 100, "only {n} cases");
}

#[test]
fn fresh_vectors_pass() {
    let dir = std::env::temp_dir().join(format!("bothy-vectors-{}", std::process::id()));
    bothy_conformance::write_all(&dir).unwrap();
    let result = bothy_conformance::check::check_dir(&dir);
    std::fs::remove_dir_all(&dir).ok();
    result.unwrap();
}

#[test]
fn committed_vectors_match_the_generator() {
    // Compressed bytes depend on the zstd library, so compare everything else.
    fn strip(v: &mut serde_json::Value) {
        if v.get("compressed") == Some(&serde_json::Value::Bool(true)) {
            v.as_object_mut().unwrap().remove("bytes");
        }
    }
    for (rel, mut fresh) in bothy_conformance::generate::all() {
        let text = std::fs::read_to_string(vectors_dir().join(&rel)).unwrap_or_else(|e| {
            panic!("{rel}: {e}; run `cargo run -p bothy-conformance -- generate spec/vectors`")
        });
        let mut committed: serde_json::Value = serde_json::from_str(&text).unwrap();
        strip(&mut fresh);
        strip(&mut committed);
        assert_eq!(committed, fresh, "{rel} is stale; regenerate the vectors");
    }
}
