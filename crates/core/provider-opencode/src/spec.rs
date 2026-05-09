//! Types mechanically generated from the committed opencode `/doc`
//! OpenAPI snapshot.
//!
//! Do **not** edit `spec_types.rs` by hand — `build.rs` overwrites it
//! on every build from
//! `tests/fixtures/openapi-<VERSION>.json`. To re-baseline:
//!
//! 1. Run the parity check in `CLAUDE.md::Verify feature parity`.
//! 2. Replace the fixture with the fresh snapshot (and bump the
//!    filename / `SPEC_PATH` in `build.rs`).
//! 3. `cargo build` — the generator regenerates this module
//!    automatically; new schemas appear, removed ones disappear,
//!    optional / required field changes flip the corresponding
//!    `Option<T>` wrappers.
//! 4. If the build panics, `build.rs` saw a schema shape it doesn't
//!    handle yet — extend `rust_type_for` / `emit_schema` and retry.
//!
//! ## Coverage
//!
//! `/doc` is **partial** as of 1.14.41 — it covers only the auth
//! and logging endpoints (operation IDs `auth.set`, `auth.remove`,
//! `app.log`). The session, event-stream, agent, and permission
//! endpoints we actually depend on are **not** in the spec; for
//! those, see [`crate::http`] (hand-written, snapshot-asserted via
//! `tests/`).
//!
//! When opencode expands `/doc` coverage, the new types appear here
//! automatically and we can migrate `http.rs` to consume them.
//!
//! See [`PROTOCOL.md`] (`Live introspection (GET /doc)`) for the
//! full caveat list.
//!
//! [`PROTOCOL.md`]: ../../PROTOCOL.md

#![allow(missing_docs)] // generated module — docstrings live on each emitted item

include!(concat!(env!("OUT_DIR"), "/spec_types.rs"));

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// The snapshot we ship with should declare a non-empty info
    /// version. If this fails the generator wasn't run, or the
    /// snapshot is missing `/info/version`.
    #[test]
    fn version_constants_present() {
        assert!(!OPENAPI_INFO_VERSION.is_empty());
        assert!(!OPENAPI_VERSION.is_empty());
        // Keep the format sanity-checked — major opencode upgrades
        // typically only bump the patch component.
        assert!(
            OPENAPI_VERSION.starts_with("3."),
            "expected an OpenAPI 3.x spec, got {OPENAPI_VERSION}"
        );
    }

    /// `Auth` is an `anyOf` over three concrete shapes, distinguished
    /// by their `type` const. Round-trip each variant to make sure
    /// the generated `untagged` enum picks the right arm.
    #[test]
    fn auth_oauth_round_trip() {
        let payload = json!({
            "type": "oauth",
            "refresh": "rt_abc",
            "access": "at_abc",
            "expires": 1_736_000_000_i64,
            "accountId": "acct_x",
        });
        let parsed: Auth = serde_json::from_value(payload.clone()).expect("decode oauth");
        match &parsed {
            Auth::OAuth(o) => {
                assert_eq!(o.refresh, "rt_abc");
                assert_eq!(o.access, "at_abc");
                assert_eq!(o.expires, 1_736_000_000);
                assert_eq!(o.account_id.as_deref(), Some("acct_x"));
                assert!(o.enterprise_url.is_none());
            }
            other => panic!("expected OAuth variant, got {other:?}"),
        }
        // Tagged enum: re-serialization injects `"type": "oauth"` and
        // skips `None` fields. Compare on the canonical shape.
        let re = serde_json::to_value(&parsed).expect("encode");
        assert_eq!(re["type"], "oauth");
        assert_eq!(re["refresh"], "rt_abc");
        assert_eq!(re["accountId"], "acct_x");
    }

    #[test]
    fn auth_api_key_round_trip() {
        let payload = json!({
            "type": "api",
            "key": "sk-...",
            "metadata": { "label": "personal" }
        });
        let parsed: Auth = serde_json::from_value(payload.clone()).expect("decode api");
        match &parsed {
            Auth::ApiAuth(a) => {
                assert_eq!(a.key, "sk-...");
                assert_eq!(
                    a.metadata.as_ref().unwrap().get("label").map(String::as_str),
                    Some("personal")
                );
            }
            other => panic!("expected ApiAuth, got {other:?}"),
        }
        let re = serde_json::to_value(&parsed).expect("encode");
        assert_eq!(re["type"], "api");
        assert_eq!(re["key"], "sk-...");
    }

    #[test]
    fn auth_wellknown_round_trip() {
        let payload = json!({
            "type": "wellknown",
            "key": "github",
            "token": "ghu_..."
        });
        let parsed: Auth = serde_json::from_value(payload.clone()).expect("decode wellknown");
        match parsed {
            Auth::WellKnownAuth(w) => {
                assert_eq!(w.key, "github");
                assert_eq!(w.token, "ghu_...");
            }
            other => panic!("expected WellKnownAuth, got {other:?}"),
        }
    }

    /// The discriminator pattern is the entire point of the
    /// generator's tagged-enum upgrade — make sure a payload whose
    /// `type` doesn't match any variant fails to deserialize, rather
    /// than silently falling through to an untagged best-effort
    /// match.
    #[test]
    fn auth_unknown_type_rejected() {
        let payload = json!({
            "type": "magic",
            "key": "x",
            "token": "y"
        });
        let result: Result<Auth, _> = serde_json::from_value(payload);
        assert!(
            result.is_err(),
            "unknown discriminator should fail, got {result:?}"
        );
    }

    /// `BadRequestError` is a fixed envelope we'd see on 400
    /// responses for the auth endpoints. Make sure the required
    /// `success: false` const survives serde round-trip.
    #[test]
    fn bad_request_error_round_trip() {
        let payload = json!({
            "data": null,
            "errors": [ { "providerID": "must be a string" } ],
            "success": false
        });
        let parsed: BadRequestError = serde_json::from_value(payload).expect("decode");
        assert!(!parsed.success);
        assert_eq!(parsed.errors.len(), 1);
    }
}
