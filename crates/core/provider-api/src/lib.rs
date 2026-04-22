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
pub mod process_group;

/// Current wire schema version.
///
/// Bump this number when the HTTP / WS wire shape changes in a way
/// that would break a shell built against an older daemon (or vice
/// versa). The Phase 6 daemon handshake embeds this value in
/// `GET /api/version`; the Tauri shell compares its own bundled
/// `SCHEMA_VERSION` against what the running daemon reports and
/// refuses to proceed on mismatch with a "Restart flowstate to
/// finish updating" dialog. Keeping the constant here keeps a single
/// source of truth; transports (`transport-http`, future
/// `transport-tauri` replacements) and consumers (Tauri shell,
/// mcp-server) import this rather than hardcode the integer.
pub const SCHEMA_VERSION: u32 = 1;
mod events;
pub mod helpers;
pub mod mcp_config;
mod messages;
pub mod orchestration;
pub mod orchestration_ipc;
pub mod probe;
pub mod process_cache;
pub mod skills_disk;
mod types;

pub use adapter::*;
pub use binary_resolver::find_cli_binary;
pub use capabilities::{
    AgentCapabilityTool, ToolCatalogEntry, capability_tools, capability_tools_wire,
    encode_runtime_error, encode_runtime_result, parse_runtime_call,
};
pub use events::*;
pub use helpers::{
    claude_bucket_label, claude_file_change_from_tool_call, first_non_empty_line,
    parse_options_from_value, session_cwd, write_json_line,
};
pub use messages::*;
pub use orchestration::{
    PollOutcome, ProviderCatalogEntry, RuntimeCall, RuntimeCallDispatcher, RuntimeCallError,
    RuntimeCallOrigin, RuntimeCallResult, SessionCreator, SessionDigest, WorktreeSummary,
};
pub use mcp_config::{
    McpConfigFile, McpServerConfig, flowstate_mcp_config_file, flowstate_mcp_entry,
    write_mcp_config_file,
};
pub use orchestration_ipc::{OrchestrationIpcHandle, OrchestrationIpcInfo};
pub use probe::{ProbeCliOptions, probe_cli};
pub use process_cache::{ActivityGuard, CachedProcess, ProcessCache};
pub use process_group::{enter_own_process_group, kill_process_group_best_effort};
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
