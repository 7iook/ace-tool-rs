//! Regression tests for the three self-healing fixes described in
//! `.agent-workspace/.archive/2026-09-08/ace-unknown-blobs/ace-unknown-blobs-rca.md`:
//!
//! - F1: the local index must only record blobs the server actually confirmed
//!   receiving, so a partially-accepted upload can self-heal on the next run.
//! - F2: a `400 unknown blobs` retrieval failure must repair the local index
//!   and retry exactly once, instead of failing forever.
//! - F3: the index file must be scoped per-backend, so switching an ACE
//!   backend for the same project doesn't misreport stale blobs as present.
//!
//! These are written as business scenarios (what a real user session looks
//! like across multiple `index_project`/`search_context` calls against a
//! mocked ACE backend), not as unit tests of individual functions.

use std::fs;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use serde_json::json;
use tempfile::TempDir;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, Request, ResponseTemplate};

use ace_tool::config::{Config, ConfigOptions};
use ace_tool::index::IndexManager;

fn no_adaptive_options() -> ConfigOptions {
    ConfigOptions {
        no_adaptive: true,
        ..Default::default()
    }
}

/// Generic `/batch-upload` responder that confirms every blob it was sent
/// (echoing back the same hash `IndexManager` itself would compute), with an
/// empty `skipped_blobs` list. Used by tests that don't care about upload
/// reconciliation and just need indexing to "just work".
fn confirm_all_blobs_responder(req: &Request) -> ResponseTemplate {
    let body: serde_json::Value = serde_json::from_slice(&req.body).unwrap();
    let blobs = body["blobs"].as_array().cloned().unwrap_or_default();
    let confirmed: Vec<String> = blobs
        .iter()
        .map(|b| {
            let p = b["path"].as_str().unwrap_or_default();
            let c = b["content"].as_str().unwrap_or_default();
            IndexManager::calculate_blob_name(p, c)
        })
        .collect();
    ResponseTemplate::new(200).set_body_json(json!({
        "blob_names": confirmed,
        "skipped_blobs": [],
    }))
}

// ============================================================================
// F1 -- upload receipt reconciliation
// ============================================================================

/// Scenario: the server's `/batch-upload` response only confirms a subset of
/// the blobs the client sent (e.g. a batch was partially accepted). The
/// client must not record the unconfirmed blob as indexed -- otherwise the
/// mtime cache would hide it from every future upload attempt and retrieval
/// would permanently 400 on it.
#[tokio::test]
async fn server_confirming_only_some_uploaded_blobs_keeps_the_rest_out_of_the_saved_index() {
    let mock_server = MockServer::start().await;

    // The server confirms "keep.txt"'s blob but silently drops "drop.txt"'s
    // (e.g. it landed in a batch the server only partially accepted).
    Mock::given(method("POST"))
        .and(path("/batch-upload"))
        .respond_with(|req: &Request| {
            let body: serde_json::Value = serde_json::from_slice(&req.body).unwrap();
            let blobs = body["blobs"].as_array().cloned().unwrap_or_default();
            let confirmed: Vec<String> = blobs
                .iter()
                .filter(|b| b["path"].as_str() == Some("keep.txt"))
                .map(|b| {
                    let p = b["path"].as_str().unwrap();
                    let c = b["content"].as_str().unwrap();
                    IndexManager::calculate_blob_name(p, c)
                })
                .collect();
            ResponseTemplate::new(200).set_body_json(json!({
                "blob_names": confirmed,
                "skipped_blobs": [],
            }))
        })
        .mount(&mock_server)
        .await;

    let temp_dir = TempDir::new().unwrap();
    fs::write(temp_dir.path().join("keep.txt"), "keep me").unwrap();
    fs::write(temp_dir.path().join("drop.txt"), "drop me").unwrap();

    let config = Config::new(
        mock_server.uri(),
        "test-token".to_string(),
        no_adaptive_options(),
    )
    .unwrap();
    let manager = IndexManager::new(config, temp_dir.path().to_path_buf()).unwrap();

    let result = manager.index_project().await;
    assert_ne!(
        result.status, "error",
        "indexing must not hard-fail: {}",
        result.message
    );

    // The unconfirmed blob must not have made it into the persisted index.
    let index = manager.load_index();
    assert!(
        index.entries.contains_key("keep.txt"),
        "confirmed file must be recorded"
    );
    assert!(
        !index.entries.contains_key("drop.txt"),
        "unconfirmed file must NOT be recorded as indexed -- it will \
         permanently 400 on retrieval otherwise"
    );

    // Running indexing again must re-attempt uploading drop.txt's blob,
    // because it's no longer present in the saved index (self-heal path,
    // no separate "dirty" flag involved).
    let result2 = manager.index_project().await;
    assert_ne!(result2.status, "error");
    let stats2 = result2.stats.expect("second run must produce stats");
    assert_eq!(
        stats2.existing_blobs, 1,
        "keep.txt must be a cache hit on the second run"
    );

    let requests = mock_server.received_requests().await.unwrap();
    let drop_upload_attempts = requests
        .iter()
        .filter(|r| {
            let body: serde_json::Value = serde_json::from_slice(&r.body).unwrap();
            body["blobs"]
                .as_array()
                .map(|blobs| blobs.iter().any(|b| b["path"] == "drop.txt"))
                .unwrap_or(false)
        })
        .count();
    assert_eq!(
        drop_upload_attempts, 2,
        "drop.txt's blob must be re-uploaded on the following index run \
         (attempt 1 = initial index_project, attempt 2 = the re-run above)"
    );
}

// ============================================================================
// F2 -- search self-heal on `400 unknown blobs`
// ============================================================================

/// Scenario: retrieval fails once with `400 unknown blobs: <hash>` (the
/// server no longer has a blob the local index believes is present, e.g. due
/// to GC / retention expiry / a backend switch). The client must repair its
/// local index and retry the search exactly once, returning the second
/// attempt's result to the caller instead of surfacing the first failure.
#[tokio::test]
async fn search_self_heals_once_after_server_reports_unknown_blob_then_succeeds() {
    let mock_server = MockServer::start().await;

    Mock::given(method("POST"))
        .and(path("/batch-upload"))
        .respond_with(confirm_all_blobs_responder)
        .mount(&mock_server)
        .await;

    let temp_dir = TempDir::new().unwrap();
    fs::write(temp_dir.path().join("a.txt"), "hello world").unwrap();
    let unknown_hash = IndexManager::calculate_blob_name("a.txt", "hello world");

    let retrieval_call_count = Arc::new(AtomicUsize::new(0));
    let retrieval_call_count_for_mock = retrieval_call_count.clone();

    Mock::given(method("POST"))
        .and(path("/agents/codebase-retrieval"))
        .respond_with(move |_req: &Request| {
            let call_index = retrieval_call_count_for_mock.fetch_add(1, Ordering::SeqCst);
            if call_index == 0 {
                ResponseTemplate::new(400)
                    .set_body_string(format!("Bad Request - unknown blobs: {}", unknown_hash))
            } else {
                ResponseTemplate::new(200)
                    .set_body_json(json!({ "formatted_retrieval": "healed search result" }))
            }
        })
        .mount(&mock_server)
        .await;

    let config = Config::new(
        mock_server.uri(),
        "test-token".to_string(),
        no_adaptive_options(),
    )
    .unwrap();
    let manager = IndexManager::new(config, temp_dir.path().to_path_buf()).unwrap();

    let result = manager.search_context("find something").await;
    assert!(
        result.is_ok(),
        "search must succeed after automatic self-heal, got: {:?}",
        result.err()
    );
    assert_eq!(result.unwrap(), "healed search result");

    let requests = mock_server.received_requests().await.unwrap();
    let retrieval_calls = requests
        .iter()
        .filter(|r| r.url.path() == "/agents/codebase-retrieval")
        .count();
    assert_eq!(
        retrieval_calls, 2,
        "must retry exactly once (initial attempt + 1 retry)"
    );

    // The repaired index must have re-uploaded a.txt's blob as part of the
    // retry's index_project() call.
    let upload_requests = requests
        .iter()
        .filter(|r| r.url.path() == "/batch-upload")
        .count();
    assert_eq!(
        upload_requests, 2,
        "a.txt must be re-uploaded during the repair (initial index + repair index)"
    );
}

/// Scenario: the server keeps rejecting the same blob as unknown even after
/// the client rebuilt and re-uploaded it (e.g. it is fundamentally
/// unreachable). The client must give up after exactly one retry rather than
/// looping, and must say so in the error instead of silently returning an
/// empty/successful result.
#[tokio::test]
async fn search_gives_up_after_exactly_one_retry_when_backend_keeps_rejecting_blobs() {
    let mock_server = MockServer::start().await;

    Mock::given(method("POST"))
        .and(path("/batch-upload"))
        .respond_with(confirm_all_blobs_responder)
        .mount(&mock_server)
        .await;

    let temp_dir = TempDir::new().unwrap();
    fs::write(temp_dir.path().join("a.txt"), "hello world").unwrap();
    let unknown_hash = IndexManager::calculate_blob_name("a.txt", "hello world");

    Mock::given(method("POST"))
        .and(path("/agents/codebase-retrieval"))
        .respond_with(move |_req: &Request| {
            ResponseTemplate::new(400)
                .set_body_string(format!("Bad Request - unknown blobs: {}", unknown_hash))
        })
        .mount(&mock_server)
        .await;

    let config = Config::new(
        mock_server.uri(),
        "test-token".to_string(),
        no_adaptive_options(),
    )
    .unwrap();
    let manager = IndexManager::new(config, temp_dir.path().to_path_buf()).unwrap();

    let result = manager.search_context("find something").await;
    assert!(result.is_err(), "must not silently succeed or return empty");
    let err_msg = result.unwrap_err().to_string();
    assert!(
        err_msg.to_lowercase().contains("retry") || err_msg.to_lowercase().contains("retrying"),
        "error message must indicate a repair/retry was already attempted, got: {}",
        err_msg
    );

    let requests = mock_server.received_requests().await.unwrap();
    let retrieval_calls = requests
        .iter()
        .filter(|r| r.url.path() == "/agents/codebase-retrieval")
        .count();
    assert_eq!(
        retrieval_calls, 2,
        "must attempt retrieval exactly twice total, never more (no retry loop)"
    );
}

/// Scenario: retrieval fails with a body that does NOT mention "unknown
/// blobs" (e.g. a generic 400 from malformed input). The client must not
/// attempt a blind repair-and-retry in that case -- it should surface the
/// original error immediately.
#[tokio::test]
async fn search_does_not_retry_on_400_that_is_not_about_unknown_blobs() {
    let mock_server = MockServer::start().await;

    Mock::given(method("POST"))
        .and(path("/batch-upload"))
        .respond_with(confirm_all_blobs_responder)
        .mount(&mock_server)
        .await;

    let temp_dir = TempDir::new().unwrap();
    fs::write(temp_dir.path().join("a.txt"), "hello world").unwrap();

    Mock::given(method("POST"))
        .and(path("/agents/codebase-retrieval"))
        .respond_with(ResponseTemplate::new(400).set_body_string("Bad Request - malformed query"))
        .mount(&mock_server)
        .await;

    let config = Config::new(
        mock_server.uri(),
        "test-token".to_string(),
        no_adaptive_options(),
    )
    .unwrap();
    let manager = IndexManager::new(config, temp_dir.path().to_path_buf()).unwrap();

    let result = manager.search_context("find something").await;
    assert!(result.is_err());

    let requests = mock_server.received_requests().await.unwrap();
    let retrieval_calls = requests
        .iter()
        .filter(|r| r.url.path() == "/agents/codebase-retrieval")
        .count();
    assert_eq!(
        retrieval_calls, 1,
        "a 400 unrelated to unknown blobs must not trigger a retry"
    );
}

// ============================================================================
// F3 -- per-backend index isolation
// ============================================================================

/// Scenario: the same project is indexed against two different ACE backends
/// (as happens when a user's MCP config points at different servers, or
/// switches providers). Each backend must get its own index file, and
/// switching back to a previously-used backend must be a cache hit again
/// instead of re-claiming the other backend's blobs as present.
#[tokio::test]
async fn switching_backends_for_the_same_project_keeps_separate_indexes_and_cache_hits_on_return() {
    let mock_server_a = MockServer::start().await;
    let mock_server_b = MockServer::start().await;

    Mock::given(method("POST"))
        .and(path("/batch-upload"))
        .respond_with(confirm_all_blobs_responder)
        .mount(&mock_server_a)
        .await;
    Mock::given(method("POST"))
        .and(path("/batch-upload"))
        .respond_with(confirm_all_blobs_responder)
        .mount(&mock_server_b)
        .await;

    let temp_dir = TempDir::new().unwrap();
    fs::write(temp_dir.path().join("a.txt"), "shared project file").unwrap();

    let config_a = Config::new(
        mock_server_a.uri(),
        "token-a".to_string(),
        no_adaptive_options(),
    )
    .unwrap();
    let config_b = Config::new(
        mock_server_b.uri(),
        "token-b".to_string(),
        no_adaptive_options(),
    )
    .unwrap();

    let manager_a = IndexManager::new(config_a, temp_dir.path().to_path_buf()).unwrap();
    let manager_b = IndexManager::new(config_b, temp_dir.path().to_path_buf()).unwrap();

    assert_ne!(
        manager_a.index_file_path(),
        manager_b.index_file_path(),
        "different backends must use different index files"
    );

    // Index against backend A.
    let result_a1 = manager_a.index_project().await;
    assert_ne!(result_a1.status, "error");
    assert_eq!(result_a1.stats.unwrap().new_blobs, 1);

    // Index the SAME project against backend B: must be a full (re)upload,
    // not a cache hit against A's index.
    let result_b1 = manager_b.index_project().await;
    assert_ne!(result_b1.status, "error");
    assert_eq!(result_b1.stats.unwrap().new_blobs, 1);

    // Switch back to backend A: must be a cache hit, no re-upload needed.
    let result_a2 = manager_a.index_project().await;
    assert_ne!(result_a2.status, "error");
    let stats_a2 = result_a2.stats.unwrap();
    assert_eq!(
        stats_a2.new_blobs, 0,
        "switching back to backend A must not need any new blob uploads"
    );
    assert_eq!(stats_a2.existing_blobs, 1);

    let upload_calls_to_a = mock_server_a
        .received_requests()
        .await
        .unwrap()
        .iter()
        .filter(|r| r.url.path() == "/batch-upload")
        .count();
    assert_eq!(
        upload_calls_to_a, 1,
        "backend A must only have received the single initial upload"
    );
}

// ============================================================================
// F1/F2 -- all-or-nothing reconciliation for multi-chunk files
// ============================================================================

fn chunked_options(max_lines: usize) -> ConfigOptions {
    ConfigOptions {
        no_adaptive: true,
        max_lines_per_blob: Some(max_lines),
        ..Default::default()
    }
}

/// Six lines at two lines per blob => three chunks, so the file straddles
/// whatever batch boundary the client picks.
const SIX_LINES: &str = "l1\nl2\nl3\nl4\nl5\nl6\n";

async fn blobs_uploaded_for(mock_server: &MockServer, needle: &str) -> usize {
    mock_server
        .received_requests()
        .await
        .unwrap()
        .iter()
        .filter(|r| r.url.path() == "/batch-upload")
        .map(|r| {
            let body: serde_json::Value = serde_json::from_slice(&r.body).unwrap();
            body["blobs"]
                .as_array()
                .map(|blobs| {
                    blobs
                        .iter()
                        .filter(|b| b["path"].as_str().unwrap_or_default().contains(needle))
                        .count()
                })
                .unwrap_or(0)
        })
        .sum()
}

/// Scenario: a file large enough to be split into several chunks has one of
/// those chunks lost by the server (a partially-accepted batch). Keeping the
/// confirmed chunks would be worse than keeping nothing: the surviving entry
/// is a cache hit on every later run, so the missing chunk is never
/// re-uploaded and retrieval quietly returns a half-indexed file -- with no
/// error anywhere, because every hash it does send is one the server knows.
/// The whole entry must therefore be dropped and the file re-processed whole.
#[tokio::test]
async fn a_file_whose_chunks_are_only_partly_confirmed_is_re_indexed_whole_not_left_half_indexed() {
    let mock_server = MockServer::start().await;

    Mock::given(method("POST"))
        .and(path("/batch-upload"))
        .respond_with(|req: &Request| {
            let body: serde_json::Value = serde_json::from_slice(&req.body).unwrap();
            let blobs = body["blobs"].as_array().cloned().unwrap_or_default();
            // Confirm everything except chunk 2 of the file.
            let confirmed: Vec<String> = blobs
                .iter()
                .filter(|b| !b["path"].as_str().unwrap_or_default().contains("#chunk2of"))
                .map(|b| {
                    let p = b["path"].as_str().unwrap();
                    let c = b["content"].as_str().unwrap();
                    IndexManager::calculate_blob_name(p, c)
                })
                .collect();
            ResponseTemplate::new(200).set_body_json(json!({
                "blob_names": confirmed,
                "skipped_blobs": [],
            }))
        })
        .mount(&mock_server)
        .await;

    let temp_dir = TempDir::new().unwrap();
    fs::write(temp_dir.path().join("big.txt"), SIX_LINES).unwrap();

    let config = Config::new(
        mock_server.uri(),
        "test-token".to_string(),
        chunked_options(2),
    )
    .unwrap();
    let manager = IndexManager::new(config, temp_dir.path().to_path_buf()).unwrap();

    let result = manager.index_project().await;
    assert_ne!(result.status, "error", "{}", result.message);
    assert_eq!(
        blobs_uploaded_for(&mock_server, "big.txt").await,
        3,
        "sanity: the file must actually have been split into three chunks"
    );

    let index = manager.load_index();
    assert!(
        !index.entries.contains_key("big.txt"),
        "a file with an unconfirmed chunk must be dropped entirely, not stored \
         with the confirmed subset -- a surviving partial entry is a permanent \
         cache hit and the lost chunk would never be re-uploaded"
    );

    // The follow-up run must re-send every chunk, not only the lost one.
    let result2 = manager.index_project().await;
    assert_ne!(result2.status, "error");
    assert_eq!(
        blobs_uploaded_for(&mock_server, "big.txt").await,
        6,
        "all three chunks must be re-uploaded on the next run (3 + 3), because \
         the file is re-processed as a whole"
    );
}

/// Scenario: the same all-or-nothing rule, but reached from the retrieval
/// side. The server reports just one chunk of a multi-chunk file as unknown.
/// Dropping only that hash would leave the file half-indexed and invisible to
/// future repairs, so the repair must evict the whole file and re-upload
/// every chunk.
#[tokio::test]
async fn search_repair_evicts_the_whole_file_when_only_one_of_its_chunks_is_unknown() {
    let mock_server = MockServer::start().await;

    Mock::given(method("POST"))
        .and(path("/batch-upload"))
        .respond_with(confirm_all_blobs_responder)
        .mount(&mock_server)
        .await;

    let temp_dir = TempDir::new().unwrap();
    fs::write(temp_dir.path().join("big.txt"), SIX_LINES).unwrap();
    // Middle chunk only: lines 3-4, joined without a trailing newline.
    let middle_chunk_hash = IndexManager::calculate_blob_name("big.txt#chunk2of3", "l3\nl4");

    let retrieval_call_count = Arc::new(AtomicUsize::new(0));
    let retrieval_call_count_for_mock = retrieval_call_count.clone();
    let unknown_hash = middle_chunk_hash.clone();

    Mock::given(method("POST"))
        .and(path("/agents/codebase-retrieval"))
        .respond_with(move |_req: &Request| {
            let call_index = retrieval_call_count_for_mock.fetch_add(1, Ordering::SeqCst);
            if call_index == 0 {
                ResponseTemplate::new(400)
                    .set_body_string(format!("Bad Request - unknown blobs: {}", unknown_hash))
            } else {
                ResponseTemplate::new(200)
                    .set_body_json(json!({ "formatted_retrieval": "healed after full re-index" }))
            }
        })
        .mount(&mock_server)
        .await;

    let config = Config::new(
        mock_server.uri(),
        "test-token".to_string(),
        chunked_options(2),
    )
    .unwrap();
    let manager = IndexManager::new(config, temp_dir.path().to_path_buf()).unwrap();

    // Establish the baseline index first, and prove the hash we are about to
    // declare unknown is really one of this file's chunks (guards against the
    // test silently passing if chunking or sanitization ever changes).
    let seed = manager.index_project().await;
    assert_ne!(seed.status, "error", "{}", seed.message);
    assert!(
        manager
            .load_index()
            .entries
            .get("big.txt")
            .expect("big.txt must be indexed")
            .blob_hashes
            .contains(&middle_chunk_hash),
        "sanity: the hash used as 'unknown' must be a real chunk of big.txt"
    );

    let uploads_before_repair = blobs_uploaded_for(&mock_server, "big.txt").await;
    assert_eq!(uploads_before_repair, 3);

    let result = manager.search_context("find something").await;
    assert!(
        result.is_ok(),
        "search must self-heal, got: {:?}",
        result.err()
    );
    assert_eq!(result.unwrap(), "healed after full re-index");

    assert_eq!(
        blobs_uploaded_for(&mock_server, "big.txt").await - uploads_before_repair,
        3,
        "the repair must re-upload all three chunks, not just the one the \
         server named -- otherwise the file stays half-indexed"
    );
}
