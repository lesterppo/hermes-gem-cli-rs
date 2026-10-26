//! Rust port of lesterppo/hermes-gem-cli (WebAPI approach).
//!
//! The Python original is a wrapper around the `gemini-webapi` library plus
//! a browser-shaped batchexecute client (`BrowserBatchRPC`) for read RPCs.
//! This crate reimplements that protocol directly in Rust: init-token
//! extraction from gemini.google.com/app, StreamGenerate chat, and
//! batchexecute RPCs (LIST_BOTS, GET_USER_STATUS, LIST_CONVERSATIONS,
//! LIST_CONVERSATION_TURNS, CREATE_BOT, DELETE_BOT).
//!
//! Auth tiers (mirroring the Python original, in order):
//! 1. `GEMINI_SID` / `GEMINI_TS` env vars
//! 2. `~/.gemini-cli/auth.json` (`__Secure-1PSID` / `__Secure-1PSIDTS`)
//! 3. Chromium cookie stores (v10 CBC-peanuts and, when a Local State key
//!    is present, AES-GCM decryption). Values are held in memory only and
//!    are never printed, logged, or written to the repo.
//!
//! Security note: cookie *values* are never emitted in output. Doctor
//! output reports only existence / liveness booleans.

pub mod auth;
pub mod client;
pub mod protocol;
pub mod util;

pub use auth::{auth_cache_path, scan_browsers, Auth, AuthCandidate, resolve_auth, validate_auth};
pub use client::{ChatResult, GeminiClient, Gem, ModelInfo};
pub use protocol::{
    build_stream_request, default_metadata, extract_gem_id, extract_tokens,
    parse_batchexecute, parse_stream_response, BatchexecuteTokens,
};
pub use util::{classify_error, sanitize_no_proxy};
