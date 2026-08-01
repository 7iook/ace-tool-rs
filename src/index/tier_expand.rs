//! Hint expansion -- turns `retrieval_tiers.hints` coordinate entries into
//! real code slices, or explicit errors when the file has moved / drifted /
//! is unreadable.
//!
//! # Four-state error taxonomy (Requirement 4.8, hard rule)
//!
//! Path missing, permission denied, and version mismatch are **three
//! independent** failure classes; conflating "no permission" with
//! "content drifted" would send the consumer looking for a nonexistent
//! version drift. Judgment order is fixed:
//!
//!   1. Read the file (via `fs::read`).
//!   2. If read errored with `PermissionDenied` -> `PermissionDenied`.
//!   3. If read errored with `NotFound` -> `PathNotFound`.
//!   4. If read errored otherwise -> `Io`.
//!   5. Read succeeded: recompute hash; if != expected -> `VersionMismatch`.
//!   6. All matched -> return the sliced content.
//!
//! Reference impl: `ace-backend-rs/scripts/tier_consumer.py:26-131`.

use crate::index::wire_types::{HintEntry, TierInfo};
use crate::index::IndexManager;
use std::io;
use std::path::{Path, PathBuf};
use thiserror::Error;

/// Hint expansion failure kinds. Each variant is a distinct wire-level error
/// code -- do NOT merge or upcast (see module docs).
#[derive(Debug, Error)]
pub enum HintExpandError {
    /// The hint's file does not exist under `project_root`. Typical cause:
    /// file renamed / deleted after indexing; index snapshot is stale.
    #[error("hint path not found: {path}")]
    PathNotFound { path: PathBuf },

    /// The hint's file exists but the current process cannot read it.
    /// This is NEVER reported as `VersionMismatch` -- consumers must be able
    /// to distinguish "I can't read this file" from "this file has drifted".
    #[error("permission denied reading hint: {path}")]
    PermissionDenied {
        path: PathBuf,
        #[source]
        source: io::Error,
    },

    /// The file was read successfully, but its recomputed `blob_hash` does
    /// not match the expected one from the hint. Content is NOT returned:
    /// a drifted slice is worse than an error, because the caller would
    /// silently edit the wrong lines.
    #[error("blob_hash mismatch for {path}: expected {expected}, actual {actual}")]
    VersionMismatch {
        path: String,
        expected: String,
        actual: String,
    },

    /// Catch-all for other IO failures (encoding, EIO, etc.). Distinct so
    /// callers can telemetry-count it separately from the three primary
    /// categories.
    #[error("io error reading hint")]
    Io {
        #[source]
        source: io::Error,
    },

    /// The hint path is unsafe (escape attempt / absolute path / drive letter
    /// / null byte). §4.4 trust-nothing boundary: the client does NOT trust
    /// server-returned paths, even when the server is our own -- the hint
    /// coordinate is data, not a filesystem instruction. Emitted before any
    /// filesystem access.
    #[error("unsafe hint path rejected: {path} ({reason})")]
    UnsafePath { path: String, reason: String },
}

/// Strip the `#chunkNofM` suffix (if any) and return the clean file path.
/// See `tier_consumer.py:59`.
fn clean_hint_path(raw: &str) -> &str {
    match raw.find('#') {
        Some(idx) => raw[..idx].trim(),
        None => raw.trim(),
    }
}

/// Reject unsafe hint paths BEFORE any filesystem access.
///
/// The hint's `path` is server-provided data, not a filesystem instruction.
/// §4.4 trust-nothing boundary: even for our own server, we do not let a
/// malformed hint make us read outside the project root. `Path::join` is
/// *not* a safe primitive here -- it happily appends `..` components and
/// on Windows an absolute `C:\...` argument replaces the base entirely.
///
/// Rejections:
/// - null byte (`\0`): filesystem APIs may misinterpret
/// - Windows drive letter (`C:` etc.) or UNC prefix (`\\`)
/// - leading `/` or `\` (absolute-rooted)
/// - any `..` path component (parent-directory escape)
/// - any `\` on non-Windows paths (canonicalized to `/`)
///
/// Returns the cleaned relative path components joined with `/`.
fn validate_hint_path(clean: &str) -> Result<PathBuf, HintExpandError> {
    let reject = |reason: &str| {
        Err(HintExpandError::UnsafePath {
            path: clean.to_string(),
            reason: reason.to_string(),
        })
    };
    if clean.is_empty() {
        return reject("empty path");
    }
    if clean.contains('\0') {
        return reject("null byte in path");
    }
    // Windows drive letter ("C:...") or UNC prefix ("\\\\...") -- both let
    // `Path::join` produce an absolute path outside project_root.
    if clean.len() >= 2 && clean.as_bytes()[1] == b':' {
        return reject("drive-letter path forbidden");
    }
    if clean.starts_with('/') || clean.starts_with('\\') {
        return reject("absolute-rooted path forbidden");
    }
    // Split on both separators to catch mixed-style attacks.
    let mut out = PathBuf::new();
    for part in clean.split(['/', '\\']) {
        if part.is_empty() || part == "." {
            continue;
        }
        if part == ".." {
            return reject("parent-directory component ('..') forbidden");
        }
        // Extra Windows-specific: reject any lone drive-letter-shaped part
        // that could re-anchor mid-path (defense in depth).
        if part.len() == 2 && part.as_bytes()[1] == b':' {
            return reject("drive-letter component forbidden");
        }
        out.push(part);
    }
    if out.as_os_str().is_empty() {
        return reject("path resolves to empty");
    }
    Ok(out)
}

/// Expand a single hint into the exact source lines it points at.
///
/// Returns the sliced content (`start_line..=end_line`, 1-indexed inclusive)
/// on success. See module docs for the error-classification order.
pub fn expand_hint(project_root: &Path, hint: &HintEntry) -> Result<String, HintExpandError> {
    let clean = clean_hint_path(&hint.path);
    // §4.4 boundary: validate BEFORE touching the filesystem. `Path::join`
    // trusts its argument and will happily produce a path outside
    // project_root if the hint contains '..', drive letters, or absolute
    // roots. See `validate_hint_path` for the rejection set.
    let safe_rel = validate_hint_path(clean)?;
    let full = project_root.join(&safe_rel);

    // Try to read once and classify by ErrorKind. Doing existence / perms /
    // read separately would introduce a TOCTOU gap between the check and the
    // read; a single `fs::read` is atomic.
    let bytes = match std::fs::read(&full) {
        Ok(b) => b,
        Err(e) => {
            return Err(match e.kind() {
                io::ErrorKind::PermissionDenied => HintExpandError::PermissionDenied {
                    path: full,
                    source: e,
                },
                io::ErrorKind::NotFound => HintExpandError::PathNotFound { path: full },
                _ => HintExpandError::Io { source: e },
            });
        }
    };

    // Match the indexing pipeline byte-for-byte:
    //   read_file_bytes -> from_utf8_lossy -> sanitize_content -> calculate_blob_name
    // (`IndexManager::sanitize_content` strips the same C0 control byte set
    // the indexer strips; skipping it here would compute a hash over a
    // superset of the bytes the server hashed -> spurious VersionMismatch
    // on any file with tabs stripped or CRLF quirks). See
    // `manager.rs:634-649` (index path) and
    // `manager.rs:1527-1529` (rehash-for-cache path); both pass content
    // through `sanitize_content` before `calculate_blob_name`.
    let raw = String::from_utf8_lossy(&bytes).into_owned();
    let content = IndexManager::sanitize_content(&raw);
    let actual = IndexManager::calculate_blob_name(clean, &content);
    if actual != hint.blob_hash {
        return Err(HintExpandError::VersionMismatch {
            path: clean.to_string(),
            expected: hint.blob_hash.clone(),
            actual,
        });
    }

    // 1-indexed inclusive slice. Match tier_consumer.py:123-127 clamping so
    // an off-by-one at the file tail does not fail the whole expansion.
    let lines: Vec<&str> = content.lines().collect();
    let s = hint.start_line.max(1) as usize;
    let e_raw = hint.end_line.max(1) as usize;
    let e = e_raw.min(lines.len());
    if s > e {
        // Range is entirely past EOF -- return empty rather than error;
        // this matches the Python reference behavior.
        return Ok(String::new());
    }
    // s and e are 1-indexed inclusive; slice with s-1..e (0-indexed
    // half-open) yields exactly `e - s + 1` lines.
    Ok(lines[s - 1..e].join("\n"))
}

/// C3 composition helper: append expanded hint bodies to the server's
/// `formatted_retrieval` string. Errors are folded into inline markers so
/// one bad hint never fails the whole request.
///
/// # Output shape
///
/// The composed string looks like (angle-bracket placeholders are illustrative
/// only; nothing is templated):
///
/// - `<base>` verbatim
/// - blank line, `---`, blank line
/// - `## Expanded hints`
/// - for each hint: `### <path> (lines <s>-<e>)`, then a fenced code block
///   containing the expanded content, OR
/// - on failure: an HTML comment marker `<!-- expand failed: <Kind>: <msg> -->`
///
/// # Why append instead of splice
///
/// Splicing back into the middle of `formatted_retrieval` (replacing the
/// `(hint)` line-headers) would require parsing markdown -- fragile, and
/// the current server format uses `## path (Lines s-e) (hint)` as its
/// contract with upstream regex consumers. Appending is transparent to
/// that contract.
pub fn append_expanded_hints(base: &str, tiers: &TierInfo, project_root: &Path) -> String {
    if tiers.hints.is_empty() {
        return base.to_string();
    }
    let mut out = String::with_capacity(base.len() + tiers.hints.len() * 256);
    out.push_str(base);
    out.push_str("\n\n---\n\n## Expanded hints\n");
    for hint in &tiers.hints {
        let clean = clean_hint_path(&hint.path);
        out.push_str(&format!(
            "\n### {} (lines {}-{})\n\n",
            clean, hint.start_line, hint.end_line
        ));
        match expand_hint(project_root, hint) {
            Ok(content) => {
                out.push_str("```\n");
                out.push_str(&content);
                if !content.ends_with('\n') {
                    out.push('\n');
                }
                out.push_str("```\n");
            }
            Err(err) => {
                // Emit both a machine-parseable marker (with the exact enum
                // variant name) AND a human-readable message so the caller
                // can debug drift without cross-referencing this file.
                let kind = match err {
                    HintExpandError::PathNotFound { .. } => "PathNotFound",
                    HintExpandError::PermissionDenied { .. } => "PermissionDenied",
                    HintExpandError::VersionMismatch { .. } => "VersionMismatch",
                    HintExpandError::Io { .. } => "Io",
                    HintExpandError::UnsafePath { .. } => "UnsafePath",
                };
                tracing::warn!(
                    error = %err,
                    path = clean,
                    "hint expansion failed; leaving marker"
                );
                out.push_str(&format!("<!-- expand failed: {kind}: {err} -->\n"));
            }
        }
    }
    out
}

// ─────────────────────────────────────────────────────────────────────
// Unit tests -- fast path (no filesystem, no encoding surprises).
// Integration coverage lives in `tests/tiering_test.rs`.
// ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clean_hint_path_strips_chunk_suffix() {
        assert_eq!(clean_hint_path("src/util.rs#chunk1of2"), "src/util.rs");
        assert_eq!(clean_hint_path("src/util.rs"), "src/util.rs");
        assert_eq!(clean_hint_path("  src/util.rs#chunk1of2  "), "src/util.rs");
    }

    #[test]
    fn append_expanded_hints_returns_base_verbatim_when_no_hints() {
        // GIVEN empty tiers.hints. WHEN composed THEN output MUST equal base
        // byte-for-byte (no trailing separator, no header, nothing appended).
        // This guards the "old server or tiering-off" fallback path.
        let base = "just the formatted retrieval, please";
        let tiers = TierInfo::default();
        let root = std::path::PathBuf::from("/nonexistent");
        assert_eq!(append_expanded_hints(base, &tiers, &root), base);
    }
}
