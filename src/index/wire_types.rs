//! Wire types shared between client request/response serialization and the
//! tier-expansion consumer. Kept in a dedicated module (not private inside
//! `manager.rs`) so integration tests can assert byte-for-byte parity with
//! `ace-backend-rs/crates/ace-api/src/wire.rs`.
//!
//! # SSOT
//!
//! Server-side authority: `ace-backend-rs/crates/ace-api/src/wire.rs:29-125`.
//! Any drift here (renamed field, wrong type, missing `#[serde(default)]`)
//! is a client-side bug -- these types exist to catch that drift in CI.

use serde::{Deserialize, Serialize};

// ── Request side ───────────────────────────────────────────────────────

/// Search request payload for `POST /agents/codebase-retrieval`.
///
/// Mirrors `ace-api/src/wire.rs::SearchRequest`. Fields must be declared in
/// the same order as the server side to keep JSON diff-friendly (serde does
/// not enforce order but review does).
#[derive(Debug, Serialize)]
pub struct SearchRequest {
    pub information_request: String,
    pub blobs: BlobsPayload,
    pub dialog: Vec<serde_json::Value>,
    pub max_output_length: i32,
    pub disable_codebase_retrieval: bool,
    pub enable_commit_retrieval: bool,
    /// Capability negotiation (see `ace-api/src/wire.rs:56-73`).
    ///
    /// Set to `true` to opt into `retrieval_tiers`; server AND-combines this
    /// flag with its `tiering_enabled` config. Old servers ignore the field.
    pub supports_tiering: bool,
}

#[derive(Debug, Serialize)]
pub struct BlobsPayload {
    pub checkpoint_id: Option<String>,
    pub added_blobs: Vec<String>,
    pub deleted_blobs: Vec<String>,
}

// ── Response side ──────────────────────────────────────────────────────

/// Search response body from `POST /agents/codebase-retrieval`.
///
/// Mirrors `ace-api/src/wire.rs::SearchResponse`. `retrieval_tiers` is
/// `Option` + `#[serde(default)]` so old-server bodies (no key at all) or
/// mid-proxy-injected `null` deserialize cleanly to `None`.
#[derive(Debug, Deserialize)]
pub struct SearchResponse {
    pub formatted_retrieval: Option<String>,
    #[serde(default)]
    pub retrieval_tiers: Option<TierInfo>,
}

/// Structured tier channel returned alongside `formatted_retrieval`.
///
/// See `ace-api/src/wire.rs:98-113`:
///   - `full` = paths of full-text blocks (bodies are already inside
///     `formatted_retrieval`); this list exists so consumers do not have to
///     parse markdown to know which files are full-doc vs hint-only.
///   - `hints` = coordinate-only entries the consumer must expand locally.
#[derive(Debug, Deserialize, Default)]
pub struct TierInfo {
    #[serde(default)]
    pub full: Vec<String>,
    #[serde(default)]
    pub hints: Vec<HintEntry>,
}

/// One hint entry. Mirrors `ace-api/src/wire.rs::HintEntry` (:115-125).
///
/// `blob_hash` is MANDATORY (no `Option`): a hint without a version stamp
/// cannot be validated on expansion, so the server refuses to emit one
/// (design.md §Error Handling: unhashable chunks degrade to full-text
/// rather than to unverifiable hints).
#[derive(Debug, Deserialize, Clone)]
pub struct HintEntry {
    /// Client-side path; may carry a `#chunkNofM` suffix (same as block
    /// headers). Consumers MUST strip the suffix before hitting the disk.
    pub path: String,
    /// 1-indexed inclusive start line (same convention as `## (Lines s-e)`
    /// headers).
    pub start_line: i32,
    /// 1-indexed inclusive end line.
    pub end_line: i32,
    /// Expansion-cost hint = `end_line - start_line + 1`. Advisory.
    pub lines: i32,
    /// `sha256(path_bytes || content_bytes)` -- two `update` calls with
    /// NO separator between them (see `ace-domain/src/blob.rs` and
    /// `IndexManager::calculate_blob_name`).
    pub blob_hash: String,
}
