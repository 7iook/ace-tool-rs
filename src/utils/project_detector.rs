//! Project root detection utilities

use std::fs;
use std::path::{Path, PathBuf};

use sha2::{Digest, Sha256};

/// Get the .ace-tool directory path for a project
/// Creates the directory if it doesn't exist
pub fn get_ace_dir(project_root: &Path) -> PathBuf {
    let ace_dir = project_root.join(".ace-tool");

    if !ace_dir.exists() {
        if let Err(e) = fs::create_dir_all(&ace_dir) {
            tracing::warn!("Failed to create .ace-tool directory: {}", e);
        } else {
            // Try to add .ace-tool to .gitignore
            add_to_gitignore(project_root);
        }
    }

    ace_dir
}

/// Add .ace-tool to .gitignore
fn add_to_gitignore(project_root: &Path) {
    let gitignore_path = project_root.join(".gitignore");

    let content = if gitignore_path.exists() {
        match fs::read_to_string(&gitignore_path) {
            Ok(c) => c,
            Err(_) => return,
        }
    } else {
        String::new()
    };

    // Check if already included
    if gitignore_has_ace_tool(&content) {
        return;
    }

    // Add .ace-tool to .gitignore
    let new_content = if content.ends_with('\n') || content.is_empty() {
        format!("{}.ace-tool/\n", content)
    } else {
        format!("{}\n.ace-tool/\n", content)
    };

    if let Err(e) = fs::write(&gitignore_path, new_content) {
        tracing::warn!("Failed to update .gitignore: {}", e);
    }
}

fn gitignore_has_ace_tool(content: &str) -> bool {
    content.lines().any(|line| {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            return false;
        }
        let entry = line.split('#').next().unwrap_or(line).trim();
        entry == ".ace-tool" || entry == ".ace-tool/"
    })
}

/// Get index file path, scoped to the specific backend (`base_url` + `token`)
/// the caller is talking to.
///
/// # Why per-backend
///
/// The index records which blobs the *server* already has. If the same
/// project is indexed against two different ACE backends (or the same
/// backend with a different token/tenant), a shared `index.bin` would claim
/// blobs are present on a server that never received them, producing a
/// permanent `400 unknown blobs` on retrieval with no self-healing path
/// (see `.agent-workspace/.archive/2026-09-08/ace-unknown-blobs/ace-unknown-blobs-rca.md`,
/// hypothesis D). Keying the filename on `backend_fingerprint(base_url,
/// token)` gives each backend its own cache: switching back to a
/// previously-used backend is a cache hit again instead of a full rebuild.
pub fn get_index_file_path(project_root: &Path, base_url: &str, token: &str) -> PathBuf {
    let ace_dir = get_ace_dir(project_root);

    let legacy_path = ace_dir.join("index.bin");
    if legacy_path.exists() {
        tracing::info!(
            "Legacy shared index file {:?} is no longer used (index files are now \
             per-backend: index-<fp>.bin); it is left untouched and can be deleted manually",
            legacy_path
        );
    }

    let fp = backend_fingerprint(base_url, token);
    ace_dir.join(format!("index-{}.bin", fp))
}

/// Derive a short fingerprint identifying a specific backend + token pair.
///
/// `sha256("v1:" + base_url + "\0" + token)`, first 8 bytes as lowercase hex
/// (16 chars). The `\0` separator prevents trivial collisions between e.g.
/// `base_url="https://a"` + `token="bctoken"` and `base_url="https://abc"` +
/// `token="token"`.
fn backend_fingerprint(base_url: &str, token: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(b"v1:");
    hasher.update(base_url.as_bytes());
    hasher.update(b"\0");
    hasher.update(token.as_bytes());
    hex::encode(&hasher.finalize()[..8])
}
