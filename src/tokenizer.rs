//! Tokenizer boundary — a *selectable* token-counting backend behind one stable seam.
//!
//! Knapsack sells **measured** savings, so the credibility of every number it reports
//! rests on how those tokens are counted. Until now there was exactly one counter: the
//! char-class estimator in [`crate::token_estimate`] (a heuristic ported 1:1 from
//! Rucksack — fast, offline, zero-dep, but *estimated*, not tokenizer-exact). This module
//! adds the seam the field expects (repomix counts with tiktoken; Anthropic exposes
//! `count_tokens`) without disturbing that default.
//!
//! ## Two roles, deliberately kept apart
//! `token_estimate::tokens()` plays two parts in the codebase, and only one of them
//! belongs behind this seam:
//!   * **Engine-internal** (`pack`, `structural`, `pack_doc`, `ledger`, `bench`) — drives
//!     compression decisions and the never-worse-than-stateless guard. This MUST stay the
//!     deterministic, offline, zero-cost estimator: routing a hot pack through a network
//!     call or a multi-MB BPE would wreck latency and make `bench` irreproducible. Those
//!     call sites keep calling `token_estimate` directly and are intentionally NOT moved.
//!   * **Reporting** (the numbers a human reads) — this is the credibility surface, and the
//!     only place a heavier, exact backend earns its cost. New surfaces (`knapsack tokens`)
//!     resolve a [`Backend`] here.
//!
//! ## Backends
//!   * [`Backend::Estimate`] — the default. Delegates verbatim to `token_estimate::tokens`,
//!     so the zero-config path is byte-identical to today and stays zero-dep + offline.
//!   * [`Backend::ClaudeApi`] — opt-in, network. Exact for the model Knapsack actually
//!     reports savings on (Claude has no public offline tokenizer). Shells out to `curl`
//!     (already a hard install-time dependency; keeps the binary dep-free) and reads
//!     `input_tokens` from the `/v1/messages/count_tokens` response. Requires
//!     `ANTHROPIC_API_KEY`.
//!   * [`Backend::GptCl100k`] / [`Backend::GptO200k`] — offline, exact for GPT tokenizers
//!     (a few-percent proxy for Claude). Gated behind the `exact-tokenizer` Cargo feature
//!     so the default build keeps its tiny, zero-dependency footprint. Producing
//!     *provably* exact GPT counts needs the official multi-MB merge tables vendored AND a
//!     Unicode-correct pretokenizer; until that lands these return a typed
//!     [`TokenizerError::Unavailable`] rather than a plausible-but-wrong number — Knapsack
//!     never reports a token count it cannot stand behind.
//!
//! The compact view / store invariants are untouched: this module only *counts* text, it
//! never decides what to elide and never sees the byte-exact store.

use crate::token_estimate;

/// Which model's tokenizer a backend should emulate / query. Affects the `claude-api`
/// model id and labels; the offline `Estimate` ignores it (it is model-agnostic).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Model {
    /// Default — the Opus family. Used for `count_tokens` when no `--model` is given.
    Claude,
    /// The Sonnet family.
    ClaudeSonnet,
    /// The Haiku family.
    ClaudeHaiku,
}

impl Model {
    /// Parse the `--model` value. Unknown values are rejected loudly (the repo's
    /// reject-garbage-rather-than-silently-default convention).
    pub fn parse(s: &str) -> Option<Model> {
        match s.trim().to_ascii_lowercase().as_str() {
            "claude" | "opus" | "claude-opus" => Some(Model::Claude),
            "sonnet" | "claude-sonnet" => Some(Model::ClaudeSonnet),
            "haiku" | "claude-haiku" => Some(Model::ClaudeHaiku),
            _ => None,
        }
    }

    /// The model id sent to Anthropic's `count_tokens` endpoint. Token counts are stable
    /// across point releases within a family, so a family-representative id is sufficient.
    pub fn api_id(self) -> &'static str {
        match self {
            Model::Claude => "claude-opus-4-8",
            Model::ClaudeSonnet => "claude-sonnet-4-6",
            Model::ClaudeHaiku => "claude-haiku-4-5-20251001",
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Model::Claude => "claude-opus",
            Model::ClaudeSonnet => "claude-sonnet",
            Model::ClaudeHaiku => "claude-haiku",
        }
    }
}

/// A selectable token-counting backend. `Copy` so it threads cheaply through the reporting
/// surfaces without lifetimes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Backend {
    /// Char-class estimator — default, offline, zero-dep. Identical to `token_estimate`.
    Estimate,
    /// Anthropic `count_tokens` API — opt-in, network, exact for Claude.
    ClaudeApi(Model),
    /// OpenAI cl100k_base BPE — offline, exact for GPT-3.5/4. Feature-gated.
    GptCl100k,
    /// OpenAI o200k_base BPE — offline, exact for GPT-4o. Feature-gated.
    GptO200k,
}

/// Why a count could not be produced. Distinct variants so the CLI can give an actionable
/// message instead of a generic failure.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TokenizerError {
    /// The `--tokenizer` / env spec did not name a known backend.
    Unknown(String),
    /// A real backend that this binary/configuration cannot run right now (e.g. an
    /// offline-GPT build without the `exact-tokenizer` feature, or no vendored vocab).
    Unavailable { backend: &'static str, why: String },
    /// The backend ran but failed (network down, missing key, malformed response).
    Backend(String),
}

impl std::fmt::Display for TokenizerError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            TokenizerError::Unknown(spec) => write!(
                f,
                "unknown tokenizer '{}' (expected: estimate, claude-api, gpt-cl100k, gpt-o200k)",
                spec
            ),
            TokenizerError::Unavailable { backend, why } => {
                write!(f, "tokenizer '{}' is unavailable: {}", backend, why)
            }
            TokenizerError::Backend(msg) => write!(f, "tokenizer backend failed: {}", msg),
        }
    }
}

impl std::error::Error for TokenizerError {}

impl Backend {
    /// Parse a `--tokenizer` spec. `model` (from `--model`) only applies to `claude-api`.
    /// Accepts an optional inline model: `claude-api:sonnet`.
    pub fn parse(spec: &str, model: Option<Model>) -> Result<Backend, TokenizerError> {
        let raw = spec.trim();
        let (name, inline_model) = match raw.split_once(':') {
            Some((n, m)) => (n.trim(), Model::parse(m)),
            None => (raw, None),
        };
        let model = inline_model.or(model).unwrap_or(Model::Claude);
        match name.to_ascii_lowercase().as_str() {
            "" | "estimate" | "est" | "default" => Ok(Backend::Estimate),
            "claude-api" | "claude" | "anthropic" => Ok(Backend::ClaudeApi(model)),
            "gpt-cl100k" | "cl100k" | "cl100k_base" => Ok(Backend::GptCl100k),
            "gpt-o200k" | "o200k" | "o200k_base" | "gpt4o" | "gpt-4o" => Ok(Backend::GptO200k),
            _ => Err(TokenizerError::Unknown(raw.to_string())),
        }
    }

    /// A short, human-facing label for the backend (shown next to counts).
    pub fn label(self) -> String {
        match self {
            Backend::Estimate => "estimate".to_string(),
            Backend::ClaudeApi(m) => format!("claude-api ({})", m.label()),
            Backend::GptCl100k => "gpt-cl100k".to_string(),
            Backend::GptO200k => "gpt-o200k".to_string(),
        }
    }

    /// True for backends whose counts are exact (vs the heuristic estimate). Lets callers
    /// label a number honestly as "~N" vs "N".
    pub fn is_exact(self) -> bool {
        !matches!(self, Backend::Estimate)
    }

    /// Count the tokens in `text` under this backend.
    ///
    /// `Estimate` is infallible and offline. The exact backends may fail (network, missing
    /// key, not-built) — those return a typed error so the caller decides whether to fail
    /// loudly (an explicit `knapsack tokens` query) or fall back to the estimate (a
    /// best-effort reporting surface).
    pub fn count(self, text: &str) -> Result<usize, TokenizerError> {
        match self {
            Backend::Estimate => Ok(token_estimate::tokens(text)),
            Backend::ClaudeApi(model) => claude_api_count(model, text),
            Backend::GptCl100k => gpt_count("gpt-cl100k", text),
            Backend::GptO200k => gpt_count("gpt-o200k", text),
        }
    }

    /// Count tokens for raw bytes (lossy UTF-8 decode), matching `token_estimate::tokens_bytes`.
    pub fn count_bytes(self, bytes: &[u8]) -> Result<usize, TokenizerError> {
        self.count(&String::from_utf8_lossy(bytes))
    }
}

/// Resolve the active backend, in precedence order: an explicit `--tokenizer` flag value
/// (highest), then the `KNAPSACK_TOKENIZER` env var, then the default `Estimate`. `model`
/// comes from `--model` and only influences `claude-api`.
pub fn resolve(flag: Option<&str>, model: Option<Model>) -> Result<Backend, TokenizerError> {
    if let Some(spec) = flag {
        return Backend::parse(spec, model);
    }
    match std::env::var("KNAPSACK_TOKENIZER") {
        Ok(spec) if !spec.trim().is_empty() => Backend::parse(&spec, model),
        _ => Ok(Backend::Estimate),
    }
}

// ---------------------------------------------------------------------------
// claude-api backend: exact Claude counts via Anthropic's count_tokens endpoint.
// Implemented as two pure, unit-tested functions (request body + response parse) plus a
// thin subprocess shim, so the logic is testable without the network.
// ---------------------------------------------------------------------------

const ANTHROPIC_COUNT_URL: &str = "https://api.anthropic.com/v1/messages/count_tokens";
const ANTHROPIC_VERSION: &str = "2023-06-01";

/// Build the JSON request body for `count_tokens`. Pure + deterministic so it can be
/// pinned by a test. Uses the in-tree zero-dep JSON serializer.
pub fn build_count_request_body(model_id: &str, text: &str) -> String {
    use crate::json::Json;
    let message = Json::Obj(vec![
        ("role".to_string(), Json::Str("user".to_string())),
        ("content".to_string(), Json::Str(text.to_string())),
    ]);
    let body = Json::Obj(vec![
        ("model".to_string(), Json::Str(model_id.to_string())),
        ("messages".to_string(), Json::Arr(vec![message])),
    ]);
    crate::json::to_string(&body)
}

/// Extract `input_tokens` from a `count_tokens` response body. Returns `None` if the field
/// is absent or non-numeric (e.g. an API error envelope) — the caller maps that to a
/// `Backend` error carrying the raw response for diagnosis.
pub fn parse_input_tokens(response: &str) -> Option<usize> {
    let v = crate::json::parse(response).ok()?;
    let n = v.get("input_tokens")?.as_f64()?;
    if n.is_finite() && n >= 0.0 {
        Some(n as usize)
    } else {
        None
    }
}

fn claude_api_count(model: Model, text: &str) -> Result<usize, TokenizerError> {
    let key = std::env::var("ANTHROPIC_API_KEY").map_err(|_| {
        TokenizerError::Backend(
            "ANTHROPIC_API_KEY is not set (required for --tokenizer claude-api)".to_string(),
        )
    })?;
    if key.trim().is_empty() {
        return Err(TokenizerError::Backend(
            "ANTHROPIC_API_KEY is empty".to_string(),
        ));
    }
    let body = build_count_request_body(model.api_id(), text);
    let response = post_json(ANTHROPIC_COUNT_URL, &key, &body)?;
    parse_input_tokens(&response).ok_or_else(|| {
        // Don't leak the (possibly large) raw body or any echoed content; surface a bounded
        // hint. API errors come back as {"error": {...}}, which is enough to act on.
        let hint = crate::json::parse(&response)
            .ok()
            .and_then(|v| {
                v.get("error")
                    .and_then(|e| e.get("message"))
                    .and_then(|m| m.as_str().map(str::to_string))
            })
            .unwrap_or_else(|| "response had no numeric input_tokens".to_string());
        TokenizerError::Backend(format!("count_tokens: {}", hint))
    })
}

/// POST a JSON body to the Anthropic API via `curl`, returning the response body.
///
/// We shell out rather than take an HTTP/TLS crate so the binary stays dependency-free;
/// `curl` is already required by the one-line installer, and this path is strictly opt-in.
/// The body is passed on stdin (`--data-binary @-`) so request content never lands in the
/// process argv. The api key is passed as a header arg — acceptable under Knapsack's
/// stated local-developer-tool threat model (it already lives in this user's env).
fn post_json(url: &str, api_key: &str, body: &str) -> Result<String, TokenizerError> {
    use std::io::Write;
    use std::process::{Command, Stdio};

    let mut child = Command::new("curl")
        .args([
            "-sS",
            "--fail-with-body",
            "-X",
            "POST",
            url,
            "-H",
            "content-type: application/json",
            "-H",
            &format!("anthropic-version: {}", ANTHROPIC_VERSION),
            "-H",
            &format!("x-api-key: {}", api_key),
            "--data-binary",
            "@-",
        ])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| {
            TokenizerError::Backend(format!("could not run curl (is it installed?): {}", e))
        })?;

    if let Some(mut stdin) = child.stdin.take() {
        stdin
            .write_all(body.as_bytes())
            .map_err(|e| TokenizerError::Backend(format!("writing request body: {}", e)))?;
    }

    let out = child
        .wait_with_output()
        .map_err(|e| TokenizerError::Backend(format!("waiting for curl: {}", e)))?;

    let stdout = String::from_utf8_lossy(&out.stdout).into_owned();
    if out.status.success() {
        Ok(stdout)
    } else {
        // --fail-with-body still prints the response body on stdout for HTTP errors; prefer
        // that (it carries the API's error message) over the terse curl stderr.
        let stderr = String::from_utf8_lossy(&out.stderr);
        let detail = if stdout.trim().is_empty() {
            stderr.trim().to_string()
        } else {
            stdout
        };
        Err(TokenizerError::Backend(format!(
            "curl exited {}: {}",
            out.status.code().unwrap_or(-1),
            detail
        )))
    }
}

// ---------------------------------------------------------------------------
// gpt offline BPE backends: feature-gated, exact-or-nothing.
// ---------------------------------------------------------------------------

/// Offline GPT BPE counting. Behind the `exact-tokenizer` feature; even then it refuses to
/// emit a count until the official merge tables are vendored and a Unicode pretokenizer is
/// in place — an honest `Unavailable` beats a plausible-but-wrong "exact" number.
fn gpt_count(backend: &'static str, _text: &str) -> Result<usize, TokenizerError> {
    #[cfg(not(feature = "exact-tokenizer"))]
    {
        Err(TokenizerError::Unavailable {
            backend,
            why: "this binary was built without the `exact-tokenizer` feature; rebuild with \
                  `cargo build --release --features exact-tokenizer` (note: offline GPT BPE \
                  also requires vendored cl100k/o200k vocab — tracked as follow-up)"
                .to_string(),
        })
    }
    #[cfg(feature = "exact-tokenizer")]
    {
        // The feature is compiled in, but the vendored merge tables + Unicode-correct
        // pretokenizer are not yet present. Returning Unavailable (not a guessed count)
        // keeps the "never report a number we can't stand behind" invariant.
        Err(TokenizerError::Unavailable {
            backend,
            why: "offline GPT BPE vocab is not vendored in this build yet".to_string(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_backend_matches_engine_estimator_exactly() {
        // The whole point of keeping Estimate the default: zero-config counts must be
        // byte-identical to the engine estimator, so existing metrics never silently move.
        for s in [
            "",
            "hello world",
            "fn main() { println!(\"hi\"); }",
            "0123456789 αβγ — punctuation, lots!!!",
            &"x".repeat(1000),
        ] {
            assert_eq!(
                Backend::Estimate.count(s).unwrap(),
                token_estimate::tokens(s),
                "Estimate must equal token_estimate::tokens for {:?}",
                s
            );
        }
    }

    #[test]
    fn count_bytes_matches_lossy_decode() {
        let bytes = b"some \xff\xfe bytes";
        assert_eq!(
            Backend::Estimate.count_bytes(bytes).unwrap(),
            token_estimate::tokens_bytes(bytes)
        );
    }

    #[test]
    fn parse_accepts_known_specs_and_aliases() {
        assert_eq!(Backend::parse("estimate", None), Ok(Backend::Estimate));
        assert_eq!(Backend::parse("", None), Ok(Backend::Estimate));
        assert_eq!(Backend::parse("  DEFAULT ", None), Ok(Backend::Estimate));
        assert_eq!(Backend::parse("gpt-cl100k", None), Ok(Backend::GptCl100k));
        assert_eq!(Backend::parse("o200k", None), Ok(Backend::GptO200k));
        assert_eq!(
            Backend::parse("claude-api", None),
            Ok(Backend::ClaudeApi(Model::Claude))
        );
    }

    #[test]
    fn parse_threads_model_for_claude_api() {
        assert_eq!(
            Backend::parse("claude-api", Some(Model::ClaudeSonnet)),
            Ok(Backend::ClaudeApi(Model::ClaudeSonnet))
        );
        // Inline model wins over the --model default.
        assert_eq!(
            Backend::parse("claude-api:haiku", Some(Model::ClaudeSonnet)),
            Ok(Backend::ClaudeApi(Model::ClaudeHaiku))
        );
    }

    #[test]
    fn parse_rejects_unknown_spec() {
        match Backend::parse("tiktoken9000", None) {
            Err(TokenizerError::Unknown(s)) => assert_eq!(s, "tiktoken9000"),
            other => panic!("expected Unknown, got {:?}", other),
        }
    }

    #[test]
    fn gpt_backends_are_unavailable_not_wrong() {
        // Whether or not the feature is on, the default-vocab-less build must refuse to
        // emit a number rather than fabricate one.
        for b in [Backend::GptCl100k, Backend::GptO200k] {
            match b.count("hello") {
                Err(TokenizerError::Unavailable { .. }) => {}
                other => panic!("expected Unavailable for {:?}, got {:?}", b, other),
            }
        }
    }

    #[test]
    fn build_count_request_body_has_expected_shape() {
        let body = build_count_request_body("claude-opus-4-8", "hi");
        // Round-trip through the parser to assert structure without pinning byte layout.
        let v = crate::json::parse(&body).expect("valid json");
        assert_eq!(
            v.get("model").and_then(|m| m.as_str()),
            Some("claude-opus-4-8")
        );
        let msgs = match v.get("messages") {
            Some(crate::json::Json::Arr(a)) => a,
            _ => panic!("messages must be an array"),
        };
        assert_eq!(msgs.len(), 1);
        assert_eq!(msgs[0].get("role").and_then(|r| r.as_str()), Some("user"));
        assert_eq!(msgs[0].get("content").and_then(|c| c.as_str()), Some("hi"));
    }

    #[test]
    fn parse_input_tokens_reads_the_field() {
        assert_eq!(parse_input_tokens(r#"{"input_tokens": 42}"#), Some(42));
        assert_eq!(
            parse_input_tokens(r#"{"input_tokens": 42, "extra": true}"#),
            Some(42)
        );
    }

    #[test]
    fn parse_input_tokens_rejects_missing_or_bad() {
        assert_eq!(
            parse_input_tokens(r#"{"error": {"message": "bad key"}}"#),
            None
        );
        assert_eq!(parse_input_tokens(r#"{"input_tokens": -3}"#), None);
        assert_eq!(parse_input_tokens("not json"), None);
        assert_eq!(parse_input_tokens(r#"{"input_tokens": "x"}"#), None);
    }

    #[test]
    fn is_exact_flags_estimate_as_inexact() {
        assert!(!Backend::Estimate.is_exact());
        assert!(Backend::GptCl100k.is_exact());
        assert!(Backend::ClaudeApi(Model::Claude).is_exact());
    }
}
