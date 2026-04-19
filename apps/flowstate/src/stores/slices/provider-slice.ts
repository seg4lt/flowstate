// Provider-domain slice. Covers providers, rateLimits, and
// sessionCommands — the adapter health / model lists, account-wide
// usage buckets, and per-session slash-command catalogs.
//
// Mutations come from `provider_models_updated`,
// `provider_health_updated`, `rate_limit_updated`, and
// `session_command_catalog_updated` runtime events, plus the
// bootstrap / snapshot messages. Consumers subscribe through the
// narrow hook to keep re-render pressure off sessions and pending
// prompts.

export { useProviderSlice } from "../app-store";
