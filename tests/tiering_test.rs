//! Client-side tiering tests (C1 + C2 + C3)
//!
//! Contract source of truth:
//!   `ace-backend-rs/crates/ace-api/src/wire.rs` (SearchRequest.supports_tiering,
//!   SearchResponse.retrieval_tiers, TierInfo, HintEntry)
//!
//! Reference impl (Python): `ace-backend-rs/scripts/tier_consumer.py` lines 26-131.
//!
//! # Layout
//!
//! - Tests 1-3: wire serialization / deserialization (C1)
//! - Tests 4-7: hint expander four-state taxonomy (C2)
//! - Test 8: end-to-end search_context wiring (C3)
//!
//! Every test asserts a **real** behavior against real bytes / real files /
//! real HTTP mock; no assertions on mocks themselves.

use ace_tool::index::tier_expand::{expand_hint, HintExpandError};
use ace_tool::index::wire_types::{HintEntry, SearchRequest, SearchResponse, TierInfo, BlobsPayload};
use ace_tool::index::IndexManager;
use serde_json::json;
use std::fs;
use tempfile::TempDir;

// ─────────────────────────────────────────────────────────────────────
// C1 · Wire serialization / deserialization tests
// ─────────────────────────────────────────────────────────────────────

#[test]
fn supports_tiering_true_appears_in_serialized_request_body() {
    // GIVEN a SearchRequest with supports_tiering = true (the value client will
    // actually send once C3 is wired). WHEN serialized THEN JSON body MUST
    // contain the literal `"supports_tiering":true` — this pins the field name
    // against typos and the boolean value against silent drift to false.
    let req = SearchRequest {
        information_request: "hello".to_string(),
        blobs: BlobsPayload {
            checkpoint_id: None,
            added_blobs: vec![],
            deleted_blobs: vec![],
        },
        dialog: vec![],
        max_output_length: 0,
        disable_codebase_retrieval: false,
        enable_commit_retrieval: false,
        supports_tiering: true,
    };
    let body = serde_json::to_string(&req).expect("SearchRequest must serialize");
    assert!(
        body.contains(r#""supports_tiering":true"#),
        "serialized request MUST carry supports_tiering:true; got: {body}"
    );
}

#[test]
fn search_response_deserializes_without_retrieval_tiers_field() {
    // GIVEN an old-server response (no retrieval_tiers key at all).
    // WHEN deserialized THEN retrieval_tiers must be None (forward compatibility
    // per wire.rs `skip_serializing_if = Option::is_none`).
    // Note: use serde_json::json! to build the literal; raw-string `#` clashes
    // with Rust's r#""# delimiter grammar when the JSON payload itself
    // contains a `#` character.
    let body = json!({"formatted_retrieval": "# results"}).to_string();
    let resp: SearchResponse = serde_json::from_str(&body).expect("must deserialize old-server body");
    assert_eq!(resp.formatted_retrieval.as_deref(), Some("# results"));
    assert!(
        resp.retrieval_tiers.is_none(),
        "missing retrieval_tiers key MUST yield None"
    );
}

#[test]
fn search_response_deserializes_with_retrieval_tiers_populated() {
    // GIVEN a new-server response carrying full + hints. WHEN deserialized
    // THEN all five HintEntry fields (path/start_line/end_line/lines/blob_hash)
    // must be parsed with correct types.
    let body = json!({
        "formatted_retrieval": "# results",
        "retrieval_tiers": {
            "full": ["src/lib.rs", "src/main.rs"],
            "hints": [
                {
                    "path": "src/util.rs#chunk1of2",
                    "start_line": 10,
                    "end_line": 42,
                    "lines": 33,
                    "blob_hash": "deadbeef"
                }
            ]
        }
    })
    .to_string();
    let resp: SearchResponse = serde_json::from_str(&body).expect("new-server body must deserialize");
    let tiers = resp.retrieval_tiers.expect("retrieval_tiers must be Some");
    assert_eq!(tiers.full, vec!["src/lib.rs", "src/main.rs"]);
    assert_eq!(tiers.hints.len(), 1);
    let h = &tiers.hints[0];
    assert_eq!(h.path, "src/util.rs#chunk1of2");
    assert_eq!(h.start_line, 10);
    assert_eq!(h.end_line, 42);
    assert_eq!(h.lines, 33);
    assert_eq!(h.blob_hash, "deadbeef");
}

// ─────────────────────────────────────────────────────────────────────
// C2 · Hint expander tests (four-state taxonomy)
// ─────────────────────────────────────────────────────────────────────

fn hint_for(path: &str, start: i32, end: i32, hash: &str) -> HintEntry {
    HintEntry {
        path: path.to_string(),
        start_line: start,
        end_line: end,
        lines: end - start + 1,
        blob_hash: hash.to_string(),
    }
}

#[test]
fn hint_expand_happy_path_returns_sliced_lines() {
    // GIVEN a real file where blob_hash matches (recomputed via
    // IndexManager::calculate_blob_name -- the SSOT algorithm, same as server).
    // WHEN expanded with lines 2..=3 (1-indexed inclusive) THEN return exactly
    // lines 2 and 3 joined by "\n".
    let dir = TempDir::new().unwrap();
    let rel = "src/util.rs";
    let full = dir.path().join(rel);
    fs::create_dir_all(full.parent().unwrap()).unwrap();
    let content = "line1\nline2\nline3\nline4\n";
    fs::write(&full, content).unwrap();

    let expected_hash = IndexManager::calculate_blob_name(rel, content);
    let hint = hint_for(rel, 2, 3, &expected_hash);
    let extracted = expand_hint(dir.path(), &hint).expect("hash matches -- expected Ok");
    assert_eq!(
        extracted, "line2\nline3",
        "must return lines 2..=3 (1-indexed inclusive)"
    );
}

#[test]
fn hint_expand_happy_path_strips_chunk_suffix() {
    // GIVEN a hint.path carrying `#chunkNofM` suffix (the wire block-header
    // convention; tier_consumer.py:59). WHEN expanding THEN suffix must be
    // stripped before fs::read AND before hash recomputation.
    let dir = TempDir::new().unwrap();
    let rel = "src/util.rs";
    fs::create_dir_all(dir.path().join("src")).unwrap();
    let content = "alpha\nbeta\ngamma\n";
    fs::write(dir.path().join(rel), content).unwrap();

    let expected_hash = IndexManager::calculate_blob_name(rel, content);
    let hint = hint_for("src/util.rs#chunk1of2", 1, 2, &expected_hash);
    let extracted = expand_hint(dir.path(), &hint).expect("suffix must be stripped");
    assert_eq!(extracted, "alpha\nbeta");
}

#[test]
fn hint_expand_returns_path_not_found_when_missing() {
    // GIVEN hint.path pointing to a file that does not exist under project_root.
    // WHEN expanded THEN error MUST be PathNotFound (never conflated with
    // VersionMismatch even though hash comparison would fail too --
    // Requirement 4.8 hard rule).
    let dir = TempDir::new().unwrap();
    let hint = hint_for("does/not/exist.rs", 1, 1, "0000");
    match expand_hint(dir.path(), &hint) {
        Err(HintExpandError::PathNotFound { path }) => {
            assert!(
                path.to_string_lossy().contains("exist.rs"),
                "PathNotFound must carry the offending path; got: {path:?}"
            );
        }
        other => panic!("expected PathNotFound, got {other:?}"),
    }
}

#[test]
fn hint_expand_returns_version_mismatch_when_content_changed() {
    // GIVEN a file that exists AND is readable AND is NOT the version the hint
    // was minted against. WHEN expanded THEN error MUST be VersionMismatch
    // carrying both expected and actual hashes. And content MUST NOT be
    // returned (returning drifted content is worse than returning nothing --
    // tier_consumer.py:111-114 spec-level hard rule).
    let dir = TempDir::new().unwrap();
    let rel = "src/util.rs";
    fs::create_dir_all(dir.path().join("src")).unwrap();
    fs::write(dir.path().join(rel), "current content\n").unwrap();
    let hint = hint_for(rel, 1, 1, "0000000000000000000000000000000000000000000000000000000000000000");

    match expand_hint(dir.path(), &hint) {
        Err(HintExpandError::VersionMismatch { path, expected, actual }) => {
            assert_eq!(path, rel);
            assert_eq!(expected, "0000000000000000000000000000000000000000000000000000000000000000");
            assert_ne!(actual, expected, "actual hash must differ from expected");
            assert_eq!(actual.len(), 64, "actual must be sha256 hex");
        }
        other => panic!("expected VersionMismatch, got {other:?}"),
    }
}

#[cfg(unix)]
#[test]
fn hint_expand_returns_permission_denied_when_readonly() {
    // GIVEN a file with 0o000 perms on Unix. WHEN expanded THEN error MUST be
    // PermissionDenied (NEVER conflated with VersionMismatch -- Requirement
    // 4.8). Judgment-order matters: PermissionDenied MUST take precedence over
    // "read succeeded but hash mismatched".
    use std::os::unix::fs::PermissionsExt;
    let dir = TempDir::new().unwrap();
    let rel = "src/secret.rs";
    fs::create_dir_all(dir.path().join("src")).unwrap();
    let full = dir.path().join(rel);
    fs::write(&full, "hidden\n").unwrap();
    let mut perms = fs::metadata(&full).unwrap().permissions();
    perms.set_mode(0o000);
    fs::set_permissions(&full, perms).unwrap();

    let hint = hint_for(rel, 1, 1, "0000");
    let res = expand_hint(dir.path(), &hint);
    // Restore perms so TempDir cleanup succeeds.
    let mut restore = fs::metadata(&full).unwrap().permissions();
    restore.set_mode(0o644);
    let _ = fs::set_permissions(&full, restore);

    match res {
        Err(HintExpandError::PermissionDenied { path, .. }) => {
            assert!(path.to_string_lossy().contains("secret.rs"));
        }
        other => panic!("expected PermissionDenied, got {other:?}"),
    }
}

// ─────────────────────────────────────────────────────────────────────
// C3 · End-to-end test
// ─────────────────────────────────────────────────────────────────────
//
// Rather than driving the full IndexManager::search_context() pipeline (which
// requires a mock POST /batch-upload too), we test the pure composition
// helper that C3 exposes: given a parsed SearchResponse and a project_root,
// produce the final string the caller sees. That is where the C3 contract
// lives; the HTTP layer above is already covered by other tests.

use ace_tool::index::tier_expand::append_expanded_hints;

#[test]
fn end_to_end_search_context_with_hints_appends_expanded_content() {
    // GIVEN a SearchResponse carrying formatted_retrieval + one hint whose
    // blob_hash matches a real file on disk. WHEN we run the C3 composition
    // helper THEN the final string MUST contain both:
    //   1. The original formatted_retrieval verbatim.
    //   2. The expanded hint content (real lines from disk).
    // AND MUST carry an "Expanded hints" section header.
    let dir = TempDir::new().unwrap();
    let rel = "src/util.rs";
    fs::create_dir_all(dir.path().join("src")).unwrap();
    let content = "fn a() {}\nfn b() {}\nfn c() {}\n";
    fs::write(dir.path().join(rel), content).unwrap();

    let hash = IndexManager::calculate_blob_name(rel, content);
    let tiers = TierInfo {
        full: vec![],
        hints: vec![HintEntry {
            path: rel.to_string(),
            start_line: 1,
            end_line: 2,
            lines: 2,
            blob_hash: hash,
        }],
    };
    let base = "# main formatted retrieval body";
    let combined = append_expanded_hints(base, &tiers, dir.path());
    assert!(
        combined.contains(base),
        "combined output MUST include original formatted_retrieval"
    );
    assert!(
        combined.contains("Expanded hints"),
        "combined output MUST include an 'Expanded hints' section header"
    );
    assert!(
        combined.contains("fn a() {}"),
        "combined output MUST include expanded line 1"
    );
    assert!(
        combined.contains("fn b() {}"),
        "combined output MUST include expanded line 2"
    );
    assert!(
        !combined.contains("fn c() {}"),
        "combined output MUST NOT include line 3 (out of range)"
    );
}

#[test]
fn end_to_end_failed_hint_becomes_marker_not_error() {
    // GIVEN a hint whose blob_hash does not match. WHEN composed THEN the
    // final string MUST still contain the base, MUST NOT panic, and MUST
    // carry an "expand failed" marker so the caller can trace it. This
    // pins the "one bad hint does not fail the whole request" contract.
    let dir = TempDir::new().unwrap();
    let rel = "src/util.rs";
    fs::create_dir_all(dir.path().join("src")).unwrap();
    fs::write(dir.path().join(rel), "real content\n").unwrap();

    let tiers = TierInfo {
        full: vec![],
        hints: vec![HintEntry {
            path: rel.to_string(),
            start_line: 1,
            end_line: 1,
            lines: 1,
            blob_hash: "0".repeat(64),
        }],
    };
    let base = "# base";
    let combined = append_expanded_hints(base, &tiers, dir.path());
    assert!(combined.contains(base));
    assert!(
        combined.contains("expand failed"),
        "failed expansions MUST leave an 'expand failed' marker; got: {combined}"
    );
    assert!(
        combined.contains("VersionMismatch"),
        "marker MUST carry the error kind; got: {combined}"
    );
}

// ─────────────────────────────────────────────────────────────────────
// C1-guard · production wire-up (IndexManager::compose_with_tiers)
// ─────────────────────────────────────────────────────────────────────

use ace_tool::config::{Config, ConfigOptions};

fn make_test_manager(project_root: &std::path::Path) -> IndexManager {
    // Minimal Config: base_url + token are dummy; compose_with_tiers
    // never touches HTTP -- it only walks tiers and reads local files.
    // Config::new returns Result<Arc<Config>>, so unwrap directly.
    let cfg = Config::new(
        "http://127.0.0.1:1".to_string(),
        "dummy".to_string(),
        ConfigOptions::default(),
    )
    .expect("Config::new failed in test fixture");
    IndexManager::new(cfg, project_root.to_path_buf())
        .expect("IndexManager::new failed in test fixture")
}

#[test]
fn production_wire_up_compose_with_tiers_actually_calls_append_expanded_hints() {
    // GIVEN a real IndexManager (the same struct search_context() uses),
    // and a hint whose file exists with matching blob_hash.
    // WHEN compose_with_tiers is invoked (the production wire-up point
    // that search_context calls at manager.rs after HTTP response parse).
    // THEN the returned string MUST contain the expanded content and the
    // "Expanded hints" section header.
    //
    // Mutation guard (C1 · reviewer's Critical #1): if a maintainer changes
    // compose_with_tiers to `_ => base` unconditionally (i.e. drops the
    // hint-expansion branch), this test turns red. Before this test, the
    // production wire-up was covered ONLY by tests that called
    // append_expanded_hints directly -- deleting the wire-up did not
    // produce a single red.
    let dir = TempDir::new().unwrap();
    let rel = "src/lib.rs";
    fs::create_dir_all(dir.path().join("src")).unwrap();
    let content = "fn one() {}\nfn two() {}\n";
    fs::write(dir.path().join(rel), content).unwrap();
    let sanitized = IndexManager::sanitize_content(content);
    let hash = IndexManager::calculate_blob_name(rel, &sanitized);

    let mgr = make_test_manager(dir.path());
    let tiers = TierInfo {
        full: vec![],
        hints: vec![HintEntry {
            path: rel.to_string(),
            start_line: 1,
            end_line: 2,
            lines: 2,
            blob_hash: hash,
        }],
    };
    let out = mgr.compose_with_tiers("# base".to_string(), Some(&tiers));
    assert!(out.contains("# base"), "base MUST be preserved: {out}");
    assert!(
        out.contains("Expanded hints"),
        "production wire-up MUST invoke append_expanded_hints (no hint section = mutation regression); got: {out}"
    );
    assert!(
        out.contains("fn one() {}"),
        "expanded line 1 MUST appear: {out}"
    );
}

#[test]
fn production_wire_up_bypasses_expansion_when_hints_empty() {
    // GIVEN retrieval_tiers with an empty hints vec (server sent tiers header
    // but no actual hint entries -- e.g. all files fit in full tier).
    // WHEN compose_with_tiers is invoked THEN output MUST byte-for-byte
    // equal the base string (no "---" separator, no header, no fenced blocks).
    // Guards the Property 5 byte-for-byte fallback path at the wire-up level.
    let dir = TempDir::new().unwrap();
    let mgr = make_test_manager(dir.path());
    let tiers = TierInfo {
        full: vec!["a.rs".to_string()],
        hints: vec![],
    };
    let base = "# some retrieval body without hint expansion";
    let out = mgr.compose_with_tiers(base.to_string(), Some(&tiers));
    assert_eq!(out, base, "empty hints MUST NOT trigger expansion append");
}

#[test]
fn production_wire_up_bypasses_expansion_when_no_tiers() {
    // GIVEN no retrieval_tiers key at all (old server / tiering-off).
    // WHEN compose_with_tiers is invoked THEN output MUST byte-for-byte
    // equal the base string. Old-server contract byte-for-byte fallback.
    let dir = TempDir::new().unwrap();
    let mgr = make_test_manager(dir.path());
    let base = "# old-server-shaped body";
    let out = mgr.compose_with_tiers(base.to_string(), None);
    assert_eq!(out, base, "None tiers MUST NOT trigger expansion append");
}

// ─────────────────────────────────────────────────────────────────────
// C3-guard · path traversal / unsafe hint rejection
// ─────────────────────────────────────────────────────────────────────

#[test]
fn hint_expand_rejects_parent_dir_traversal() {
    // GIVEN a hint whose path contains `../` components pointing outside
    // project_root. WHEN expand_hint is invoked THEN it MUST return
    // UnsafePath BEFORE any filesystem access.
    // Guards C3 (reviewer's Critical #3): `Path::join` on `../../etc/passwd`
    // silently escapes project_root; validate_hint_path stops it upstream.
    let dir = TempDir::new().unwrap();
    for attack in [
        "../etc/passwd",
        "src/../../etc/hosts",
        "a/b/../../c",
        "..\\..\\Windows\\System32\\config",
    ] {
        let h = HintEntry {
            path: attack.to_string(),
            start_line: 1,
            end_line: 1,
            lines: 1,
            blob_hash: "0".repeat(64),
        };
        let err = expand_hint(dir.path(), &h).expect_err(&format!(
            "attack path {attack:?} MUST be rejected as UnsafePath"
        ));
        assert!(
            matches!(err, HintExpandError::UnsafePath { .. }),
            "attack {attack:?} MUST classify as UnsafePath, got: {err:?}"
        );
    }
}

#[test]
fn hint_expand_rejects_absolute_and_drive_letter_paths() {
    // GIVEN hints that use absolute-rooted or drive-letter paths. `Path::join`
    // treats absolute args as replacements: `project_root.join("/etc/passwd")`
    // yields `/etc/passwd`, and on Windows `.join("C:\\...")` re-anchors.
    // WHEN expand_hint is invoked THEN it MUST return UnsafePath BEFORE any FS.
    let dir = TempDir::new().unwrap();
    for attack in [
        "/etc/passwd",
        "\\\\server\\share\\file",
        "C:\\Windows\\System32\\drivers\\etc\\hosts",
        "c:/Windows/notepad.exe",
    ] {
        let h = HintEntry {
            path: attack.to_string(),
            start_line: 1,
            end_line: 1,
            lines: 1,
            blob_hash: "0".repeat(64),
        };
        let err = expand_hint(dir.path(), &h).expect_err(&format!(
            "rooted path {attack:?} MUST be rejected as UnsafePath"
        ));
        assert!(
            matches!(err, HintExpandError::UnsafePath { .. }),
            "rooted {attack:?} MUST classify as UnsafePath, got: {err:?}"
        );
    }
}

#[test]
fn hint_expand_rejects_null_byte_in_path() {
    // Null bytes in filesystem paths are undefined behavior on POSIX and
    // an outright error on Rust's Path -- but reject upstream so the
    // error stays classified as UnsafePath (not Io).
    let dir = TempDir::new().unwrap();
    let h = HintEntry {
        path: "src/util.rs\0.txt".to_string(),
        start_line: 1,
        end_line: 1,
        lines: 1,
        blob_hash: "0".repeat(64),
    };
    let err = expand_hint(dir.path(), &h).expect_err("null byte MUST be rejected");
    assert!(
        matches!(err, HintExpandError::UnsafePath { .. }),
        "null-byte path MUST classify as UnsafePath, got: {err:?}"
    );
}

// ─────────────────────────────────────────────────────────────────────
// C2-guard · sanitize alignment (blob_hash reproducibility)
// ─────────────────────────────────────────────────────────────────────

#[test]
fn hint_expand_matches_hash_after_sanitize_when_file_has_control_chars() {
    // GIVEN a file containing C0 control characters (e.g. embedded BEL / NUL
    // replaced with U+FFFD via lossy decode) -- these are stripped by
    // IndexManager::sanitize_content at index time.
    // WHEN the server sends a hint whose blob_hash was computed AFTER
    // sanitize, THEN expand_hint MUST recompute the hash on the sanitized
    // content (not the raw content) so it matches. Guards C2 (reviewer's
    // Critical #2): the pre-fix expand_hint hashed raw lossy content and
    // produced spurious VersionMismatch on any file with tabs / BOM / etc.
    let dir = TempDir::new().unwrap();
    let rel = "src/ctrl.rs";
    fs::create_dir_all(dir.path().join("src")).unwrap();
    // Bytes 0x01 (SOH) and 0x07 (BEL) are stripped by sanitize_content.
    let raw_bytes: Vec<u8> = b"fn one() {}\n\x01\x07fn two() {}\n".to_vec();
    fs::write(dir.path().join(rel), &raw_bytes).unwrap();
    // Simulate server-side hashing: lossy decode -> sanitize -> hash.
    let raw = String::from_utf8_lossy(&raw_bytes).into_owned();
    let sanitized = IndexManager::sanitize_content(&raw);
    let hash = IndexManager::calculate_blob_name(rel, &sanitized);

    let h = HintEntry {
        path: rel.to_string(),
        start_line: 1,
        end_line: 2,
        lines: 2,
        blob_hash: hash,
    };
    // Must NOT be VersionMismatch -- if we forget the sanitize step in
    // expand_hint, this returns VersionMismatch and the test turns red.
    let ok = expand_hint(dir.path(), &h).expect(
        "hash must match after sanitize alignment; if VersionMismatch, sanitize step regressed",
    );
    assert!(ok.contains("fn one() {}"), "expected line 1: {ok}");
    assert!(ok.contains("fn two() {}"), "expected line 2: {ok}");
}
