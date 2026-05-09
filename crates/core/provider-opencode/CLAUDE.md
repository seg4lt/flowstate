# CLAUDE.md — opencode adapter

Local notes for verifying that this adapter is still in sync with the
installed `opencode` binary. Read `PROTOCOL.md` first for the actual
contract; this file is the operator's runbook.

## Current baseline

- **opencode binary**: `1.14.41`
- **OpenAPI spec version (`info.version` from `GET /doc`)**: `0.0.3`
- **Snapshot fixture**: `tests/fixtures/openapi-1.14.41.json`
- **Generated Rust types**: `src/spec.rs` re-exports
  `OUT_DIR/spec_types.rs`, emitted by `build.rs` from the snapshot
  above. Running `cargo build` keeps the types in sync with the
  fixture; the constants `spec::OPENAPI_INFO_VERSION` and
  `spec::OPENAPI_VERSION` track what the build was generated against.

## Verify feature parity

Run this whenever opencode is upgraded, or before cutting a release that
touches `provider-opencode`. It spawns a real `opencode serve`, pulls
the live `/doc` OpenAPI spec, and diffs it against the committed
fixture.

```bash
# from the repo root
PORT=17999
opencode serve --hostname 127.0.0.1 --port $PORT > /tmp/opencode-parity.log 2>&1 &
SERVER_PID=$!
# wait for readiness (banner: "opencode server listening on http://...")
for i in $(seq 1 10); do
  grep -q listening /tmp/opencode-parity.log && break
  sleep 1
done

# 1. Capture the live /doc spec.
curl -s "http://127.0.0.1:$PORT/doc" | jq . > /tmp/opencode-doc-live.json

# 2. Diff against the committed snapshot.
diff -u crates/core/provider-opencode/tests/fixtures/openapi-1.14.41.json \
        /tmp/opencode-doc-live.json

# 3. Sanity-check the surfaces /doc does NOT cover (un-schematized
#    endpoints we depend on). All four MUST return JSON, not the SPA
#    shell HTML fallback.
for path in /agent /config/providers; do
  echo "=== GET $path ==="
  curl -s -o /dev/null -w "%{http_code} %{content_type}\n" \
    "http://127.0.0.1:$PORT$path"
done

# 4. Enumerate the permission categories the built-in agents declare.
#    Compare against the table in PROTOCOL.md ("Permission categories").
curl -s "http://127.0.0.1:$PORT/agent" \
  | jq -r '[.[] | .permission[]?.permission] | unique | .[]'

# 5. Enumerate the agent registry. Compare against PROTOCOL.md ("Agents").
curl -s "http://127.0.0.1:$PORT/agent" | jq -r '.[].name'

# 6. Tear down.
kill $SERVER_PID
```

### Expected outcomes

- **`/doc` diff is empty** → we're in sync; no action.
- **`/doc` diff is non-empty** → opencode shipped a spec change.
  1. Replace the fixture:
     `cp /tmp/opencode-doc-live.json crates/core/provider-opencode/tests/fixtures/openapi-<NEW_VERSION>.json`.
  2. Bump `SPEC_PATH` in `build.rs` to point at the new file.
  3. Run `cargo build -p zenui-provider-opencode` — the build script
     regenerates `spec_types.rs` from the new fixture. If it panics,
     opencode introduced a JSON-Schema shape `build.rs` doesn't know
     yet; extend `emit_schema` / `rust_type_for` to handle it.
  4. Run `cargo test -p zenui-provider-opencode` — the in-crate
     `spec::tests` module exercises the regenerated types.
  5. Update `PROTOCOL.md` and the baseline section above.
- **Permission set differs from `PROTOCOL.md`** → update
  `permission_rules_for()` in `src/http.rs` and refresh the table in
  `PROTOCOL.md`.
- **Agent set differs** → update `agent_for()` in `src/http.rs` (only if
  we want to surface a new agent) and the table in `PROTOCOL.md`.

## Relevant tests

| Target | Command | Network? |
|--------|---------|----------|
| In-crate unit tests (canned fixtures) | `cargo test -p zenui-provider-opencode` | No |
| Live end-to-end smoke (spawns real opencode, hits Zen) | `cargo test -p zenui-provider-opencode --test live_opencode -- --ignored --nocapture` | Yes |

## Caveats

- `GET /doc` is **partial** — only `auth.set`, `auth.remove`, `app.log`
  are covered as of 1.14.41. Empty diff is necessary but not sufficient
  for feature parity; you still need steps 3–5 to catch drift on
  `/session`, `/event`, `/agent`, `/config/providers`, the permission
  endpoints, and the question endpoints.
- Aliases like `/openapi.json`, `/swagger.json`, `/scalar`, `/reference`
  all 200 with the SPA shell HTML — they are **not** real spec endpoints.
  Use only `/doc`.
- Unknown variants, unknown agent names, and unknown permission
  categories are silently accepted by opencode (200 OK). Behavioural
  drift can therefore land without any HTTP-level signal — the SSE
  stream is still the ultimate ground truth, exercised by
  `tests/live_opencode.rs`.
