//! Mid-turn bridge RPC discriminator + response shape. Separate from
//! the session-level `BridgeRequest`/`BridgeResponse` framing because
//! the RPC machinery is orthogonal: any adapter method can issue an
//! RPC to the bridge, and the reply is routed to a per-request-id
//! oneshot instead of the streaming event channel.
//!
//! Extracted from `lib.rs` in the phase 3 god-file split.

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// `BridgeResponse::RpcResponse` variant carries `kind` so new RPCs
/// can be added without growing the response enum — the bridge and
/// adapter agree on the kind string, and the caller parses the
/// `payload` shape it knows the RPC returns. `snake_case` matches
/// the rest of our wire conventions.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum BridgeRpcKind {
    ContextUsage,
}

/// Delivered through the pending-RPC oneshot. The adapter's method
/// interprets `payload` based on the known shape for its RPC kind
/// (e.g. `ContextUsage` parses as the SDK's
/// `SDKControlGetContextUsageResponse`). `Err(_)` carries whatever
/// the bridge reported — either a null-Query guard trip or an
/// exception from the SDK call.
#[derive(Debug)]
pub(crate) struct BridgeRpcResponse {
    #[allow(dead_code)]
    pub(crate) kind: BridgeRpcKind,
    pub(crate) payload: Result<Value, String>,
}
