//! Integration coverage for the tokenizer-selection boundary that the module's own unit
//! tests can't reach: `resolve()`'s precedence over the `KNAPSACK_TOKENIZER` env var. The
//! env var is process-global, so these go through `EnvSandbox` (global lock + restore) to
//! stay deterministic under the parallel test runner.

mod common;
use common::EnvSandbox;

use knapsack::tokenizer::{resolve, Backend, Model};

#[test]
fn resolve_defaults_to_estimate_when_unset() {
    let mut sb = EnvSandbox::new("tok-default");
    sb.unset("KNAPSACK_TOKENIZER");
    assert_eq!(resolve(None, None), Ok(Backend::Estimate));
}

#[test]
fn resolve_reads_env_var() {
    let mut sb = EnvSandbox::new("tok-env");
    sb.set("KNAPSACK_TOKENIZER", "gpt-o200k");
    assert_eq!(resolve(None, None), Ok(Backend::GptO200k));
}

#[test]
fn resolve_blank_env_var_falls_back_to_estimate() {
    // An empty value is a shell-substitution accident, not a request for an empty backend;
    // treat it as unset rather than erroring.
    let mut sb = EnvSandbox::new("tok-blank");
    sb.set("KNAPSACK_TOKENIZER", "   ");
    assert_eq!(resolve(None, None), Ok(Backend::Estimate));
}

#[test]
fn resolve_flag_overrides_env_var() {
    let mut sb = EnvSandbox::new("tok-precedence");
    sb.set("KNAPSACK_TOKENIZER", "gpt-o200k");
    // Explicit --tokenizer wins over the ambient env var.
    assert_eq!(
        resolve(Some("claude-api"), Some(Model::ClaudeHaiku)),
        Ok(Backend::ClaudeApi(Model::ClaudeHaiku))
    );
}

#[test]
fn resolve_propagates_unknown_spec_error() {
    let mut sb = EnvSandbox::new("tok-unknown");
    sb.unset("KNAPSACK_TOKENIZER");
    assert!(resolve(Some("nonsense"), None).is_err());
}
