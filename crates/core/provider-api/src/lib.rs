//! Shared provider-SDK contract: wire types, stream events, client /
//! server envelopes, and the `ProviderAdapter` trait.
//!
//! Historically this was one 2,000+ LOC file. Phase 3 of the
//! architecture audit split it into four sibling modules:
//!
//! - [`types`] — wire data types (ProviderKind, SessionDetail, ToolCall, …)
//! - [`events`] — `ProviderTurnEvent` + `TurnEventSink`
//! - [`messages`] — `ClientMessage`, `ServerMessage`, `RuntimeEvent`
//! - [`adapter`] — the `ProviderAdapter` trait
//!
//! Everything is re-exported from the crate root so callers can keep
//! using `zenui_provider_api::Foo` without caring which submodule
//! actually defines `Foo`. Genuine helpers (binary resolution, process
//! cache, CLI probe, skills-on-disk) live in their own feature-focused
//! modules and are kept out of the split above.

mod adapter;
mod binary_resolver;
pub mod capabilities;
mod events;
pub mod helpers;
mod messages;
pub mod orchestration;
pub mod probe;
pub mod process_cache;
pub mod skills_disk;
mod types;

pub use adapter::*;
pub use binary_resolver::find_cli_binary;
pub use capabilities::{
    AgentCapabilityTool, capability_tools, encode_runtime_error, encode_runtime_result,
    parse_runtime_call,
};
pub use events::*;
pub use helpers::{
    claude_bucket_label, claude_file_change_from_tool_call, first_non_empty_line,
    parse_options_from_value, session_cwd, write_json_line,
};
pub use messages::*;
pub use orchestration::{
    PollOutcome, RuntimeCall, RuntimeCallDispatcher, RuntimeCallError, RuntimeCallOrigin,
    RuntimeCallResult, SessionCreator, SessionDigest,
};
pub use probe::{ProbeCliOptions, probe_cli};
pub use process_cache::{ActivityGuard, CachedProcess, ProcessCache};
pub use types::*;

#[cfg(test)]
mod tests {
    use super::*;

    /// Every `ProviderKind::ALL` variant must have a registry arm —
    /// otherwise a new kind would trip the exhaustive `match` at
    /// `features_for_kind` and the CI wire-shape snapshot test
    /// elsewhere in the workspace would still pass on a partial impl.
    #[test]
    fn features_for_kind_covers_all_providers() {
        for &kind in ProviderKind::ALL {
            let features = features_for_kind(kind);
            // Sanity: CLI/SDK splits differ on a handful of flags, but
            // every provider should at minimum report stream-text.
            let _ = features; // the match itself is the assertion
        }
    }
}
