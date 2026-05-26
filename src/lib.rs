//! Knapsack — a *conditional* token reducer for agents.
//!
//! Rucksack compresses each tool output in isolation: H(output). Knapsack compresses it
//! relative to what the model has already seen: H(output | seen). On the dominant agentic
//! pattern — iterative loops over slowly-changing artifacts — that is the difference
//! between re-paying full price every turn and paying only for what changed.
//!
//! Architecture: a deterministic, byte-exact core (hashing, blocks, store, ledger, delta,
//! recall) with a thin, replaceable integration boundary (`api`). The compact VIEW may be
//! lossy; the STORE and the expand path are byte-exact. That invariant is non-negotiable.

pub mod ab;
pub mod api;
pub mod bench;
pub mod block;
pub mod config;
pub mod content_type;
pub mod gc;
pub mod hash;
pub mod hook;
pub mod install;
pub mod json;
pub mod ledger;
pub mod mcp;
pub mod meta;
pub mod metrics;
pub mod pack;
pub mod pack_doc;
pub mod read_hook;
pub mod recall;
pub mod regex;
pub mod why_log;
pub mod sha256;
pub mod status;
pub mod store;
pub mod structural;
pub mod token_estimate;
pub mod transcript;

pub use api::{evict, expand_handle, pack_output, record_residency, ExpandRequest, PackRequest};
pub use content_type::{detect, ContentType};
pub use hash::{handle, Handle};
pub use ledger::{Ledger, Residency};
pub use pack::{pack, reconstruct, PackResult};
pub use recall::{expand, RecallOut};
pub use store::Store;
pub use token_estimate::{tokens, tokens_bytes};
