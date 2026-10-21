//! Generator and checker for the Bothy format conformance vectors (`spec/vectors`).
//!
//! Other implementations can use the vector files directly; this crate is how the reference
//! implementation produces and verifies them.

pub mod check;
pub mod crypto_vectors;
pub mod generate;
pub mod typed;

use std::path::Path;

/// Writes every vector file under `dir`, replacing existing ones.
pub fn write_all(dir: &Path) -> std::io::Result<usize> {
    let files = generate::all();
    for (rel, json) in &files {
        let path = dir.join(rel);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let mut text = serde_json::to_string_pretty(json).expect("JSON values always serialize");
        text.push('\n');
        std::fs::write(path, text)?;
    }
    Ok(files.len())
}
