# Flowstate — Architecture & Code Audit

**Audit date:** 2026-04-18
**Scope:** `/Users/babal/Code/flowstate` (Rust workspace + Tauri app + React frontend, ~50k LOC).
**Method:** six parallel Opus deep-reads (core abstraction, provider crates, Tauri backend, frontend, middleman, cross-cutting).
**Rule:** no new features. Only fixes that pay back in correctness, layering, or maintenance. Every finding cites `file:line` and proposes a minimal change.

---

## Executive summary

The skeleton is healthier than average for a 50k-LOC app:

- The layered rule "apps → middleman → core, no cycles" is *almost* honored.
- Providers do not cross-reference each other; the "independent sibling" rule holds.
- `TODO/FIXME/unimplemented!` hygiene is excellent (0 hits in production code).
- Rust↔TS types are kept in sync **by hand** — the TS file even documents the serde casing rules at the top. That's discipline, but it's also a ticking drift bomb.

The rot lives in four places:

1. **God files** that make every change expensive — e.g. `runtime-core/src/lib.rs` is 4,032 lines, `chat-view.tsx` is 2,073, `provider-claude-sdk/src/lib.rs` is 2,373. Every edit forces the reviewer to reload the whole file.
2. **Provider abstraction leaks** — `runtime-core` hard-codes which providers default-on; `daemon-core` constructs concrete provider adapters behind feature flags; all five provider crates reimplement the same process-cache, idle-watchdog, and helper functions.
3. **Wrong-layer code** — `src-tauri/src/usage.rs` is 1,263 lines of analytics ORM that should be in a core crate; `src-tauri/src/lib.rs` contains ~1,100 lines of generic git-CLI plumbing; `embedded-node` sits in `middleman/` but is used only by *core* providers, which is an actual layering violation.
4. **No Rust↔TS codegen and no PR CI** — 583 lines of hand-mirrored types, no drift alarm, and CI only runs on tag push. Any wire-schema break rides to `main` unchecked.

The seven phases below are ordered so each one removes friction for the next. Phases 0–2 have the highest bang-for-buck: small surgical edits that delete real drift hazards.

### Phase priority at a glance

| Phase | Focus | Severity | Approx effort |
|---|---|---|---|
| 0 | Safety net: PR CI + Rust→TS codegen | blockers for everything after | 1 day |
| 1 | Kill abstraction leaks in `runtime-core` & `daemon-core` | P0 | 1–2 days |
| 2 | Extract duplicated provider scaffolding | P0/P1 | 2–3 days |
| 3 | Split god files (Rust) | P1 | 2–3 days |
| 4 | Move `usage.rs` + git-ops out of `src-tauri` | P0/P1 | 2–3 days |
| 5 | Split god components / slice `app-store` (React) | P0/P1 | 2–3 days |
| 6 | Housekeeping — dead crates, deps, README drift | P1/P2 | 0.5–1 day |

Severity key: **P0** = correctness or layer-breaking, **P1** = high-gain cleanup, **P2** = opportunistic polish.

---

## Phase 0 — Safety net (do this first)

**Why first:** structural refactors need a tripwire. Right now a wire-protocol change can land silently (no type checker catches it, no CI runs on PR). Fix that *before* touching anything bigger.

### 0.1 [P0] No PR CI — only tag-triggered builds
**File:** `.github/workflows/build.yml`
The only workflow runs on `push: tags: ['v*']`. That means `cargo test`, `pnpm test`, `cargo clippy`, `cargo fmt --check` never execute on a pull request. Regressions land on `main` and are only noticed at release-cut time — by which point the bad commit is buried under N others.

**Fix:** add `.github/workflows/ci.yml` triggered on `pull_request` that runs:
- `cargo fmt --check`
- `cargo clippy --all-features -- -D warnings`
- `cargo test --workspace`
- `pnpm --dir apps/flowstate test`
- One `cargo check --no-default-features --features provider-claude-sdk,transport-tauri` job to catch feature-flag bitrot (see §6.2).

### 0.2 [P0] Rust↔TS types are hand-mirrored with no drift alarm
**Files:** `apps/flowstate/src/lib/types.ts:1` (583 LOC) mirrors `crates/core/provider-api/src/lib.rs` (2,071 LOC).
The TS file literally opens with *"Types mirroring rs-agent-sdk/crates/core/provider-api/src/lib.rs"* and below that documents the serde casing rules (camelCase fields, snake_case tag values, snake_case enum-variant fields). This works until someone adds a field to a Rust struct and forgets the TS side — at which point the frontend silently deserializes half a message.

The cost is already visible: the Tauri command layer (~45 handlers) crosses this boundary 100+ times with no compile-time check that both sides agree.

**Fix:** adopt `ts-rs` (smallest, least invasive option for `#[serde(tag)]` enums).
1. Add `ts-rs = { version = "10", features = ["chrono-impl", "serde-compat"] }` as a dev-dep in `provider-api`.
2. Derive `TS` alongside `Serialize` on every wire type, gated by `#[cfg(test)]` so release builds are unaffected.
3. Add one `#[test]` that dumps generated bindings into `apps/flowstate/src/lib/generated/types.ts`.
4. CI step: run the test, then `git diff --exit-code` on the generated dir.
5. Rewrite `types.ts` to re-export from `generated/` + keep only purely-frontend types like `AttachedImage` and `RetryState`.

Expected: ~500 lines of hand-written TS deleted; wire-format drift becomes a failing test, not a midnight Discord message.

### 0.3 [P1] `mise.toml` pins only `node`
**File:** `apps/flowstate/mise.toml`
Your own global rule says "use mise for missing sdks". Right now only Node is pinned, so contributors may end up on different Rust / pnpm / bun versions than CI expects. Add `rust = "stable"`, `bun = "latest"`, `pnpm = "10"`.

### 0.4 [P1] Zero tests inside provider adapters
Adapters (`provider-claude-sdk`, `provider-claude-cli`, `provider-codex`, `provider-github-copilot`, `provider-github-copilot-cli`) have **no tests at all**. These are exactly the crates where a field rename in `provider-api` causes silent JSON breakage.

**Fix:** one snapshot test per adapter that round-trips a canonical `ProviderTurnEvent` through JSON. Doesn't need to cover behavior — just shape. This catches ~90% of wire-format breaks at `cargo test` time and unblocks Phases 2–3.

---

## Phase 1 — Kill abstraction leaks

**Why:** you said the rule is "provider-api is abstract, provider-* is the only place provider-specific details live." Today that rule is broken in ~10 specific spots. Each leak is small; collectively they mean adding a 6th provider is a multi-file edit across layers that should know nothing about it.

### 1.1 [P0] `runtime-core` hard-codes which providers are default-enabled
**File:** `crates/core/runtime-core/src/lib.rs:270`
```rust
matches!(kind, ProviderKind::Claude | ProviderKind::GitHubCopilot)
```
This line decides that Claude and GitHub Copilot are on by default and the rest are off. It sits inside `is_provider_enabled`, in the one crate that is supposed to be provider-agnostic. Every new provider will require editing this call site.

**Fix:** add a default method on the `ProviderAdapter` trait in `provider-api`:
```rust
fn default_enabled(&self) -> bool { true }
```
CLI flavors and Codex override to `false`. `is_provider_enabled` consults the adapter map instead of pattern-matching.

### 1.2 [P0] `daemon-core` owns provider construction behind feature flags
**File:** `crates/middleman/daemon-core/src/lib.rs:43–60, 227–267`
`build_adapters` directly imports and constructs `ClaudeSdkAdapter`, `ClaudeCliAdapter`, `CodexAdapter`, `GitHubCopilotAdapter`, `GitHubCopilotCliAdapter` — each behind a Cargo feature flag. Per your own architecture notes, middleman "bridges core to outside; no domain behavior." Picking which provider concrete classes exist *is* domain behavior.

Every new provider drags a middleman edit and another feature flag. The workspace already has five — it's accreting.

**Fix:**
1. Delete `build_adapters` and the five feature-gated `use` blocks.
2. `DaemonConfig` grows a field `adapters: Vec<Arc<dyn ProviderAdapter>>`.
3. `apps/flowstate/src-tauri/src/lib.rs` — the app layer that *does* know about providers — constructs the adapters it wants and hands them to `DaemonConfig`.
4. Drop the five optional deps from `daemon-core/Cargo.toml:16–27`.

This is the single edit that most restores the architecture.

### 1.3 [P0] Runtime-core stamps the host app's name into wire payloads
**File:** `crates/core/runtime-core/src/lib.rs:526`
```rust
app_name: "zenui".to_string(),
```
Runtime-core is the SDK contract — it must not know the host app's name (Flowstate, ZenUI, whatever wraps it next). This literal leaks through to every connected client.

**Fix:** `RuntimeCore::new` takes `app_name: String`. Callers in `daemon-core` forward the value from their config.

### 1.4 [P0] `runtime-core::rewind_files` does raw filesystem I/O
**File:** `crates/core/runtime-core/src/lib.rs:2146–2227`
This function reads and writes files directly with `tokio::fs`. That forces `runtime-core` to depend on `tokio` with the `fs` feature — a real architectural layering says file I/O is a persistence concern.

**Fix:** define a trait in `persistence`:
```rust
trait FileCheckpointStore {
    async fn write_file(...);
    async fn remove_file(...);
}
```
Implement on `PersistenceService`. `runtime-core` orchestrates; `persistence` does I/O. This makes `rewind` testable with a mock store, which today it is not.

### 1.5 [P0] `embedded-node` lives in the wrong layer — actual cycle-adjacent violation
**Files:** `crates/middleman/embedded-node/` — imported only by `crates/core/provider-claude-sdk/Cargo.toml:17` and `crates/core/provider-github-copilot/Cargo.toml:17`.
Two **core** crates depend on a **middleman** crate. That is the exact inversion your README forbids ("core never depends on middleman or apps"). The compiler doesn't catch it because there's no formal dependency check; it just happens to work.

**Fix:** `git mv crates/middleman/embedded-node crates/core/embedded-node`. Update path deps and the workspace `members` list. The crate stays byte-identical; it just sits in the correct folder.

Bonus: `embedded-node/build.rs:84` shells out to `curl` to download the Node runtime — this fails on Windows CI without Git Bash. Replace with `ureq` (already a transitive dep).

### 1.6 [P1] `ProviderKind` is a closed enum of literal provider names
**File:** `crates/core/provider-api/src/lib.rs:16–46`
Every variant name (`Codex`, `Claude`, `GitHubCopilot`, `ClaudeCli`, `GitHubCopilotCli`) is a leak by construction: the abstract contract names its concrete implementors. The wire format bakes those strings in forever.

Consequence: `provider_kind_to_str` appears in three places (see §2.4), plus implicit serde `rename_all`, plus a 4th copy in Tauri `usage.rs`. All four must stay in lockstep with no compile-time check.

**Fix (short term):** give the enum an inherent method `fn as_tag(self) -> &'static str` and delete all duplicated copies (§2.4).
**Fix (long term):** replace with `struct ProviderId(pub String)` + an `AdapterRegistry` where each adapter registers its own id. Out-of-scope for this phase, but file the issue.

### 1.7 [P1] Type-erased escape hatches on wire types
**Files:** `provider-api/src/lib.rs:254, 857`
`SubagentEvent.event: serde_json::Value` and `provider_state.metadata: Option<Value>` let any adapter smuggle provider-specific JSON across the "abstract" boundary. Proof: `update_session_settings` at `runtime-core/src/lib.rs:2262–2278` directly writes a `compactCustomInstructions` key into the metadata blob — from runtime-core, which again, should not know these keys exist.

**Fix:** model the known payloads as typed enum variants. For truly provider-specific extras, put them behind a typed `ProviderMetadata` associated type on the adapter trait, not on the shared wire struct.

### 1.8 [P1] `ProviderAdapter` has 9 methods; 6 are default-no-op
**File:** `crates/core/provider-api/src/lib.rs:1909–2071`
The trait combines "every adapter must implement this" with "some adapters might do this" in one flat list. This is why `ProviderFeatures` (lib.rs:514–557) is a hand-maintained struct with 10 capability booleans that mirror the optional methods. Every new capability: new boolean, new trait method, new default. Two things to keep in sync forever.

**Fix:** split into a base `ProviderAdapter` plus capability traits (`trait ContextUsage`, `trait Rewindable`, `trait FileCheckpoints`). Runtime-core downcasts at a single site. `ProviderFeatures` becomes derivable — "does this adapter implement `Rewindable`?" — rather than manually maintained.

### 1.9 [P1] `DaemonStatus` lives in `runtime-core` with an apology comment
**File:** `crates/core/runtime-core/src/lib.rs:44–51`
The struct is a daemon-core concept. A comment in the code says it lives here "to avoid circular deps" — but actually it just needs an abstract observer, not the concrete type.

**Fix:** move `DaemonStatus` to `daemon-core`. In `runtime-core`, `ConnectionObserver::status()` returns `Option<serde_json::Value>` (or a newly-defined abstract type). Kills one item of public surface from runtime-core.

### 1.10 [P2] `format_turn_context` is dead code that also leaks host-app branding
**File:** `crates/core/provider-api/src/lib.rs:874–897`
Zero callers (grep confirms). The docstring even says *"unused dead code and will be removed in a follow-up"*. Plus the function body contains the literal prompt *"You are operating inside ZenUI…"* — another host-name leak. Delete it now.

### 1.11 [P2] `CommandKind::TuiOnly` variant is never constructed
**File:** `crates/core/provider-api/src/lib.rs:584`
Speculative scaffolding ("reserved for future Codex integration") that enlarges the public serde contract. Delete until a real caller lands.

---

## Phase 2 — Extract duplicated provider scaffolding

**Why:** the five provider crates were grown independently and each reimplements the same handful of concerns. That's fine during exploration, but now the duplicates have started to **drift** — different behavior in each copy for what should be identical logic. Every drift is a future bug. Lift the common parts into `provider-api` (or a tiny shared helper) and let each provider keep only what's genuinely unique to it.

### 2.1 [P0] Three parallel process-cache + idle-watchdog implementations (~250 LOC of copy-paste)
**Files:**
- `provider-claude-sdk/src/lib.rs:49–146` — `CachedBridge`, `ActivityGuard`, 120s idle, 30s watchdog.
- `provider-github-copilot/src/lib.rs:55–114` — identical shape.
- `provider-github-copilot-cli/src/lib.rs:286–348` — same concept under different names.

All three keep a per-session long-lived subprocess, an atomic in-flight counter, a `last_activity` timestamp, and a background task that kills idle children every 30s. Drop-based stamping on guards. Three independent implementations of the same state machine.

**Fix:** one reusable component in `provider-api` (or a new `crates/core/provider-runtime` helper):
```rust
pub struct ProcessCache<T: Send + 'static> {
    // HashMap<SessionId, Cached<T>> + watchdog
}
impl<T> ProcessCache<T> {
    pub fn entry(&self, sid: &str) -> Option<Handle<T>>;
    pub fn insert_with_guard(&self, sid: String, proc: T) -> Handle<T>;
    pub async fn invalidate(&self, sid: &str) -> Option<T>;
    // watchdog spawned on first insert; caller supplies an async `kill(&T)` closure
}
```
Claude-SDK's extra `pending_rpcs` map stays in its own wrapper — it's a legitimate SDK feature. The generic piece (cache + watchdog + guard) comes out.

### 2.2 [P1] `session_cwd` — five verbatim copies of the same 6-line function
**Files:**
- `provider-claude-sdk/src/lib.rs:25–31`
- `provider-claude-cli/src/lib.rs:37–43`
- `provider-codex/src/lib.rs:20–26`
- `provider-github-copilot/src/lib.rs:27–33`
- `provider-github-copilot-cli/src/lib.rs:25–31`

All five are byte-identical. The pattern will keep repeating for every new provider.

**Fix:** lift to `provider-api` as a free function `pub fn session_cwd(session: &SessionDetail, fallback: &Path) -> PathBuf`, or as a default method on `ProviderAdapter`. Remove all five copies.

### 2.3 [P1] TS bridges each reimplement binary PATH resolution
**Files:**
- `provider-claude-sdk/bridge/src/index.ts:101–156` (`resolveLocalClaudeBinary`)
- `provider-github-copilot/bridge/src/index.ts:28–74` (`resolveCopilotBinary`)

Both walk `PATH`, handle `PATHEXT` on Windows, and fall back to platform-specific default locations. The Rust resolver in `provider-api/src/binary_resolver.rs` already diverges subtly from both TS copies. Three implementations of the same idea → three different edge cases.

**Fix:** one shared TS module at `crates/core/provider-api/bridge-ts/src/resolve.ts` consumed by both bridges with a two-line wrapper per bridge (passes the binary name). ~120 LOC deleted and behavior converges.

### 2.4 [P1] `provider_kind_to_str` lives in three places
**Files:**
- `crates/core/persistence/src/lib.rs:1231–1249`
- `crates/core/provider-api/src/skills_disk.rs:244–252` (called `provider_tag`)
- `apps/flowstate/src-tauri/src/usage.rs:567–585`

Plus the implicit serde `rename_all` on the enum itself. Four ways to convert `ProviderKind ↔ string`, any one of which can go out of sync. The `usage.rs` copy has an extra footgun: unknown strings silently fall back to `Codex` (line 583), which masks bugs rather than surfacing them.

**Fix:**
```rust
impl ProviderKind {
    pub fn as_tag(self) -> &'static str;
    pub fn from_tag(s: &str) -> Option<Self>;
}
```
Delete the three copies. Add a `#[cfg(test)]` round-trip test that every `ProviderKind::ALL` value round-trips.

### 2.5 [P1] TypeScript bridge scaffolding duplicated — plus one real leak
**Files:** `provider-claude-sdk/bridge/src/index.ts` (1,825 LOC) and `provider-github-copilot/bridge/src/index.ts` (955 LOC).

Both bridges independently implement:
- `writeStream`/`writeJson` helpers (2-line functions, identical).
- Stdin readline loop with per-message dispatch.
- `pendingPermissions` / `pendingUserInputs` `Map<string, resolver>` pattern.
- `drainPendingOnAbort` — **Claude-SDK has it (line 175–184); Copilot does not.** The Copilot bridge leaks pending promises when a session aborts mid-flight.

**Fix:** a tiny shared `bridge-ts` module with `writeStream`, `resolvePending` helpers, and a standardized `drainOnAbort`. Each bridge becomes a thin consumer. ~200 LOC gone and the Copilot leak closes for free.

### 2.6 [P1] Question-dialog parsing drifted across four providers
**Files:**
- `provider-claude-cli/src/lib.rs:1247–1363` — longest, handles 3 shapes, synthesizes IDs as `"q{i}"`.
- `provider-claude-sdk/src/lib.rs:1985–2034` — uses array index as ID.
- `provider-codex/src/lib.rs:865–918` — uses server-provided IDs (the only scheme that round-trips correctly with multi-question prompts).
- `provider-github-copilot-cli/src/lib.rs:970–986` — single-question only; UUID.

The ID schemes are incompatible. Every adapter also has its own "build the answer payload back to the provider" logic that has to match.

**Fix:**
- Extract `parse_options_from_value` (the object-or-string options parser, copy-pasted in 3 adapters) into `provider-api`.
- Standardize ID assignment: server-provided ID if present, else `q{i}`. Apply to all four adapters.
- Keep the shape-parsing per-adapter (the wire shapes really do differ).

Result: ~60 LOC saved and multi-question prompts round-trip correctly on every provider.

### 2.7 [P1] `Write`/`Edit` → `FileChange` extraction is inconsistent across providers
**Files:**
- `provider-claude-cli/src/lib.rs:550–592` — handles only `Write` and `Edit`. No `Delete`, no `NotebookEdit`.
- `provider-claude-sdk` — bridge emits explicit `file_change` events.
- `provider-codex/src/lib.rs:1316–1349` — has its own mapping.
- Copilot providers — emit no `FileChange` at all.

This is why the rewind / file-checkpoint feature only works on Claude SDK sessions (`file_checkpoints: true` is set on that adapter only). The other adapters *could* opt in but don't, because the code to extract file changes is duplicated in three shapes and missing in two.

**Fix:** a helper `FileChange::from_tool_call(name, args)` in `provider-api` covering the common tools (`Write`, `Edit`, `Delete`, `MultiEdit`). Each adapter calls it and picks up the missing cases for free. Correctness win, not a LOC win.

### 2.8 [P2] JSONL write framing — three duplicates, one genuinely different
**Files:**
- `provider-claude-sdk/src/lib.rs:1919–1938` — serialize, write, newline, flush.
- `provider-claude-cli/src/lib.rs:1190–1209` — same.
- `provider-codex/src/lib.rs:629–644` — same.
- `provider-github-copilot-cli/src/lib.rs:40–53` — **different**: vscode-jsonrpc `Content-Length:` framing. Leave alone.

**Fix:** trait `JsonStdioWriter` in provider-api with a blanket impl on `Arc<Mutex<ChildStdin>>`. The three adapters use it; Copilot-CLI keeps its LSP-style framing.

### 2.9 [P2] `probe_cli` health probe is generic but only Codex uses it
**Files:**
- `provider-codex/src/lib.rs:701–774` — a clean "run `--version`, run `auth status`, return `ProviderStatus`" helper.
- `provider-claude-cli/src/lib.rs:923–1004` — reimplements the same flow inline.

**Fix:** lift `probe_cli` into `provider-api`. Claude-CLI calls it instead of having its own copy. Copilot-CLI keeps its own flow (it speaks JSON-RPC, not CLI args, to check health).

### 2.10 [P2] `first_non_empty_line` — two copies with different behavior
**Files:**
- `provider-claude-cli/src/lib.rs:1211–1218` — strict UTF-8 (rejects non-UTF-8 bytes).
- `provider-codex/src/lib.rs:1351–1357` — lossy UTF-8 (always decodes something).

Both extract the first line of stderr/stdout from health probes. The strict version silently drops error messages on non-UTF-8 locales, which is worse UX.

**Fix:** keep the lossy version, lift it to `provider-api`, delete the strict copy.

### 2.11 [P2] Claude bucket label table duplicated between Rust and the TS bridge
**File:** `provider-claude-cli/src/lib.rs:22–33` + `provider-claude-sdk/bridge/src/index.ts:937–953`.
The code comment literally says *"Duplicated in provider-claude-sdk's bridge to keep each Claude adapter self-contained."* But the labels are a product copy decision — they'll change in lockstep. Keeping them in sync manually is just drift risk with no isolation benefit.

**Fix:** put the id→label map in `provider-api` (Rust). Have the TS bridge forward the raw bucket id and let Rust do the labelling on the way out.

### 2.12 [P2] Inconsistent tracing targets
- `provider-claude-cli` uses `target: "claude-cli"`.
- `provider-github-copilot-cli` uses `"copilot-cli"`.
- `provider-codex` uses unqualified `warn!`.

Standardize on `target: "provider-<kind>"` so that `RUST_LOG=zenui_provider_codex=debug` behaves uniformly. 5 LOC.

### 2.13 [P2] Dead variants in Claude SDK wire types
**File:** `provider-claude-sdk/src/lib.rs:110–118, 330–337`
Commented-out `RewindFiles`/`SeedReadState` RPC variants and `#[allow(dead_code)]` on `BridgeResponse::Interrupted` and `PermissionModeSet`. Either the speculative variants should land behind real callers or they should go.

---

## Phase 3 — Split god files (Rust)

**Why:** files over ~700 LOC start hurting in compounding ways — reviewers can't hold the whole thing in their head, rustc has to recompile the whole unit for any change, and merge conflicts multiply. Your current champions go up to 4,032 LOC. This phase is mechanical: extract self-contained sections into sibling modules. Zero behavior change.

### 3.1 [P0] `runtime-core/src/lib.rs` — 4,032 LOC of mixed concerns
**File:** `crates/core/runtime-core/src/lib.rs`

The file contains, roughly:
- `RuntimeCore` struct and lifecycle (~370 lines)
- `handle_client_message` — a 370-line match statement (L710–1081)
- `send_turn` — an **880-line function** (L1253–2133) that does persistence, broadcast, event accumulation, interrupt handling, content-block coalescing, permission safety net, and compact merging
- `rewind_files` (L2146–2227)
- Startup reconciliation, health check spawners, model-refresh spawners
- Session-command catalog refresh
- ~1,500 LOC of integration-style tests at the bottom

**Fix — concrete file split under `crates/core/runtime-core/src/`:**
- `core.rs` — `RuntimeCore` struct, constructors, observer wiring, seed/enablement.
- `bootstrap.rs` — `bootstrap`, `snapshot`, `reconcile_startup`, `is_cache_stale`.
- `health.rs` — health-check and model-refresh spawners.
- `dispatch.rs` — the `handle_client_message` match body.
- `turn.rs` — `send_turn` + its guard types (`TurnCounterGuard`, `InFlightPermissionModeGuard`) + the per-event accumulator.
- `permission.rs` — `answer_permission`, `update_permission_mode`, `cancel_question`.
- `rewind.rs` — `rewind_files`, `RewindOutcome` (post-§1.4, calls the new `FileCheckpointStore` trait).
- `catalog.rs` — session-command catalog logic.
- `tests/` directory (not `mod tests` inline) with one file per integration area.

### 3.2 [P1] `provider-api/src/lib.rs` — 2,071 LOC of three unrelated concerns
**File:** `crates/core/provider-api/src/lib.rs`
Effectively three independent submodules glued together. Split into:
- `types.rs` — wire data types (~L14–900)
- `events.rs` — `ProviderTurnEvent`, `RuntimeEvent` (~L935–1700)
- `messages.rs` — `ClientMessage`, `ServerMessage` (~L1700–1907)
- `adapter.rs` — `ProviderAdapter` trait, `TurnEventSink` (~L1155–2071)
- `lib.rs` — re-exports only.

### 3.3 [P1] `provider-claude-sdk/src/lib.rs` — 2,373 LOC
**File:** `crates/core/provider-claude-sdk/src/lib.rs`
After §2.1 lifts the process-cache code, this file shrinks ~200 LOC. Split the rest:
- `process.rs` — `ClaudeBridgeProcess`, what remains of the cache wrapper.
- `wire.rs` — `BridgeRequest`, `BridgeResponse`, `parse_decision`, `parse_compact_trigger`, `parse_claude_questions`.
- `rpc.rs` — `BridgeRpcKind`, `BridgeRpcResponse`, `issue_rpc`.
- `stream.rs` — `forward_stream`, compact / memory handlers.
- `config.rs` — `claude_models`, `claude_sdk_features`, `read_compact_custom_instructions`.
- `lib.rs` — the `ClaudeSdkAdapter` struct and `ProviderAdapter` impl only.

### 3.4 [P1] `provider-github-copilot-cli/src/lib.rs` (1,615) and `provider-github-copilot/src/lib.rs` (1,286)
Same pattern. Extract `process`/`cache`, `wire`, `auth`, `stream` into sibling modules. The process-cache code drops out entirely after §2.1.

### 3.5 [P2] `persistence/src/lib.rs` — 1,346 LOC
Split into `sessions.rs`, `messages.rs`, `skills.rs`, `migrations.rs`. Also see §3.6.

### 3.6 [P2] Collapse archaeological SQL migration
**File:** `crates/core/persistence/src/lib.rs:781–789, 856–869`
The initial migration creates a `sessions` table with 7 columns. Six idempotent `ALTER TABLE ADD COLUMN` calls then tack on the rest. On a fresh install both run; on an old install only the ALTERs run. It works, but reads like excavation notes. For the next schema-breaking release, collapse into a single `sessions_v2` migration.

### 3.7 [P2] `orchestration` crate is 121 lines of nothing
**File:** `crates/core/orchestration/src/lib.rs`
It has four small pure functions on `SessionDetail`: `create_session`, `start_turn`, `finish_turn`, `interrupt_session`. No scheduling, no prioritization, no cross-session state — it orchestrates nothing. Paying a crate boundary cost for no isolation.

**Fix:** fold the four functions into `runtime-core::session`. Delete the crate and remove it from `[workspace.members]`.

### 3.8 [P2] Entire runtime-core uses stringly-typed errors
Every fallible method (`send_turn`, `interrupt_turn`, `delete_session`, `accept_plan`, `rewind_files`, every adapter call) returns `Result<_, String>`. That throws away structured error info and forces transports to string-match if they want to distinguish, say, "session not found" from "adapter crashed."

**Fix:** define
```rust
#[derive(Debug, thiserror::Error)]
enum RuntimeError {
    SessionNotFound,
    AdapterMissing,
    ProviderDisabled,
    Adapter(#[source] anyhow::Error),
    // ...
}
```
in `runtime-core` and replace `String` returns. This is the single highest quality-of-implementation fix in the repo.

### 3.9 [P2] 26 `.expect("sqlite mutex poisoned")` calls in persistence
**File:** `crates/core/persistence/src/lib.rs:92, 207, 222, 245, 280, 349, 374, 407, 436, 463, 476, 516, 533, 557, 583, 601, 613, 638, 660, 704, 740, 760, 771, 914, 956, 1002`
If one callback inside a query ever panics, the mutex poisons and every subsequent `.expect` wedges the whole daemon.

**Fix:** swap `std::sync::Mutex` for `parking_lot::Mutex` (no poisoning semantics) or propagate as a `RuntimeError`. Cheap, one search-and-replace.

---

## Phase 4 — Move business logic out of `src-tauri`

**Why:** `apps/flowstate/src-tauri` should be the thin glue that wires Tauri (window, menu, tray, PTY, updater) to your core/middleman code. Today it contains a full analytics store, ~1,100 lines of git plumbing, and a 2,167-line lib.rs that mixes 14 domains. Each of these wants a different home.

### 4.1 [P0] `usage.rs` is a full analytics ORM in the wrong crate
**File:** `apps/flowstate/src-tauri/src/usage.rs` — 1,263 LOC
It contains: SQLite schema migrations, transactional writes, daily rollups, zero-filled timeseries generation, group-by SQL builders, `ProviderKind ↔ string` codecs (duplicating §2.4), and 8 unit tests. None of this is Tauri-runtime — it's just "store usage events and query them."

**Fix:**
1. New crate `crates/core/usage-store` (or a module inside a new middleman persistence crate) exposing `UsageStore`, `UsageEvent::from_turn`, `summary`, `timeseries`, `top_sessions`.
2. `src-tauri/src/usage.rs` shrinks to ~80 LOC — three `#[tauri::command]` forwarders plus the subscriber wiring that *is* Tauri-lifetime glue.
3. Delete the private `provider_kind_to_str`/`from_str` (covered by §2.4).

Net: ~1,180 LOC moves into the correct layer.

### 4.2 [P0] `src-tauri/src/lib.rs` — 14 unrelated domains in one 2,167-line file
**File:** `apps/flowstate/src-tauri/src/lib.rs`

Inventory of what's in there:
1. `path_exists` (L43–46)
2. Git branch/root helpers (L55–198)
3. Git worktree CRUD (L200–520)
4. Git checkout (L529–563)
5. Git diff summary + per-file (L565–798)
6. Streamed diff (`watch_git_diff_summary`, `DiffSummaryEvent`, `DiffTasks`) (L800–1166)
7. `/code` file picker + reader (L1168–1301)
8. `open_in_editor` (L1303–1337)
9. Content search / grep (L1339–1669)
10. Tracing init (L1675–1713)
11. PTY command wrappers (L1715–1750)
12. `user_config` commands (L1760–1888)
13. `usage` commands (L1890–1926)
14. `run()` — setup, transport wiring, updater, invoke_handler (L1928–2167)

**Fix — file layout under `apps/flowstate/src-tauri/src/`:**
- `git/mod.rs`, `git/branch.rs`, `git/worktree.rs`, `git/diff.rs`, `git/diff_stream.rs`
- `code/picker.rs`, `code/reader.rs`, `code/search.rs`
- `editor.rs`, `tracing_setup.rs`
- `commands/usage.rs`, `commands/user_config.rs`, `commands/pty.rs` (thin forwarders)
- `setup.rs` — the big `.setup(|app|…)` closure
- `lib.rs` — ~120 lines of `mod` declarations + `run()`

Reviewer cost drops dramatically and rustc's incremental compile recovers (right now any one-char change here recompiles the whole unit).

### 4.3 [P1] Git operations aren't Tauri-specific either
The ~1,100 LOC of worktree/branch/checkout/diff code in lib.rs touches nothing Tauri-runtime except the `#[tauri::command]` attribute and the `Channel<T>` streaming primitive.

**Fix:** new crate `crates/middleman/git-ops` with pure functions (`list_worktrees`, `create_worktree`, `checkout`, `diff_summary`, `watch_diff`). Tauri side becomes ~10 command wrappers that forward `Channel<DiffSummaryEvent>` through a thin trait (same pattern `transport-tauri` already uses for messages). Makes the git code independently testable and reusable in a future CLI.

### 4.4 [P1] `UserConfigStore` mixes display settings with product state
**File:** `apps/flowstate/src-tauri/src/user_config.rs:1–436`
The `project_worktree` table maps parent repos to worktree children and is read by branch-switcher logic — that's domain state, not display metadata. The comment at L47–53 admits as much ("find-or-create the worktree project"). The other tables (`session_display`, `project_display`) are legitimately display-only.

**Fix:** leave the KV + display tables where they are. Move `project_worktree` + its CRUD into the daemon's persistence layer. Tauri command becomes a 3-line forwarder.

### 4.5 [P1] Nine `.expect()` calls on the critical startup path
**File:** `apps/flowstate/src-tauri/src/lib.rs:1979, 1981, 1990, 2046, 2079, 2083, 2089, 2104, 2166`
Any one of these panics aborts the app before the window is shown. The user sees a silent dock bounce with no diagnostic — no dialog, no log, no indication what went wrong. On CI or sandboxed environments these paths are more failure-prone than they look (`app_data_dir` has failed on macOS CI runners before).

**Fix:** write a small `show_fatal_dialog(app, msg)` helper using `tauri_plugin_dialog` (already a dep), call it before `std::process::exit(1)`. ~30 LOC total.

### 4.6 [P1] `provider_kind_to_str` duplicated again in `usage.rs:567–585`
Covered by §2.4 — once `ProviderKind::as_tag()` lands, delete this copy and the silent "unknown string → `Codex`" fallback bug at `usage.rs:583` goes with it.

### 4.7 [P2] Mutex poisoning is handled inconsistently
**Files:** `apps/flowstate/src-tauri/src/lib.rs:993, 1008, 1084, 1113, 1137, 1159, 1162` and `pty.rs:123, 138, 145, 169, 173, 182, 186, 193` all use `.unwrap()`. Meanwhile `user_config.rs` and `usage.rs` correctly use `poisoned.into_inner()`.

**Fix:** add a `fn lock_ok<T>(m: &Mutex<T>) -> MutexGuard<T>` helper and use it everywhere. Trivial.

### 4.8 [P2] Tauri capabilities grant more than the app needs
**File:** `apps/flowstate/src-tauri/capabilities/default.json:7`
`core:default` pulls in a broad permission set (clipboard, path, event, webview, etc). The app actually uses: `dialog`, `updater`, `process::restart`, `opener`, path (for `app_data_dir`), event.

**Fix:** replace `core:default` with the explicit allowlist: `core:path:default`, `core:event:default`, `core:window:default`, `core:webview:default`, `core:app:default`. 20 minutes, tightens the attack surface.

### 4.9 [P2] `open_in_editor` has no allowlist
**File:** `apps/flowstate/src-tauri/src/lib.rs:1319–1336`
Spawns whatever the frontend tells it to as argv[0], with `.` and `current_dir`. Low risk in a same-UID desktop app, but inconsistent with the "explicit allowlists" story elsewhere. Either add a comment explaining the intentional permissiveness or draw the allowed editors from user config.

### 4.10 [P2] `bootstrap.ws_url` is empty on the Tauri path
**File:** `crates/middleman/transport-tauri/src/commands.rs:33`
The Tauri transport passes `String::new()` as `ws_url`. If any frontend code reads `bootstrap.ws_url` it sees `""` under Tauri and a real URL under HTTP.

**Fix:** make the field `Option<String>` in `BootstrapPayload`, or (better) drop it for in-proc transports and plumb a proper transport descriptor.

---

## Phase 5 — Frontend: split god components, slice the store

**Why:** the frontend is actually well-disciplined on the feature side (provider capabilities are flag-driven, event routing is session-scoped). The pain is concentrated in two files: `chat-view.tsx` at 2,073 lines owns a dozen unrelated concerns, and `app-store.tsx` at 1,170 lines re-renders every consumer on every event. Splitting them unlocks both editability and perf.

### 5.1 [P0] `chat-view.tsx` — one component, twelve jobs (2,073 LOC)
**File:** `apps/flowstate/src/components/chat/chat-view.tsx`

A single functional component currently owns:
- Stream event reduction (`applyEventToTurns`, L122–411) — pure, belongs in `lib/`.
- A generic resize primitive `PanelDragHandle` (L421–494) — a sibling of `DragHandle` in `router.tsx`.
- ~20 `useState` hooks of transient per-session UI state.
- The Shift+Tab mode-cycling keybinding (L965–1010).
- The double-Esc interrupt gesture (L1022–1095).
- A "stuck turn" watchdog (L1672–1687).
- Strict Plan Mode enforcement (L1554–1580).
- Module-level `Map`s of drafts and queues (L70–75) — survive HMR, leak across reloads.
- Diff-panel + context-panel layout with mutual-exclusion (L1627–1638, L1975–2043).
- Title rename (L1691–1712).
- Stream subscription (L1176–1366) — and this duplicates what the store already does (§5.3).

Every render walks through all of that closure state; any child re-render cascades.

**Fix — concrete split, each piece testable on its own:**
- `src/lib/session-event-reducer.ts` ← `applyEventToTurns`, `appendTextDelta`, `appendReasoningDelta`, `applyCompactUpdate`. (~295 LOC, pure, unit-testable.)
- `src/components/ui/panel-drag-handle.tsx` ← merge with `router.tsx`'s `DragHandle`.
- `src/hooks/` ← `useModeCycleShortcut`, `useDoubleEscInterrupt`, `useStuckWatchdog`.
- `src/components/chat/diff-panel-host.tsx` ← owns `diffOpen`, `diffWidth`, `diffStyle`, `diffFullscreen`, the subscription, the `<aside>` JSX. Same for `agent-context-panel-host.tsx`.
- `src/hooks/useSessionStreamSubscription.ts` ← lines 1176–1366. Fixes the double-subscription bug in §5.3.
- `src/stores/session-transient-store.ts` ← typed replacement for the module-level Maps (kept in-memory, but with proper types and HMR reset).
- `src/hooks/useSessionRestoration.ts` ← collapses the two "restore mode / restore open flags" effects.

Target shape: `chat-view.tsx` ≈ 350 LOC of orchestration + layout.

### 5.2 [P0] `app-store.tsx` — god store that re-renders everything (1,170 LOC)
**File:** `apps/flowstate/src/stores/app-store.tsx`

The reducer switches over two message tags plus 25+ event types (L371–743). The provider mixes Tauri focus subscription (L825–848), stream connection plus a welcome-time side effect that pushes provider-enablement to the SDK (L850–916), display hydration (L895–916), and seven named mutator callbacks each doing "call Tauri then dispatch." Every `useApp()` consumer re-renders on every dispatch — so the sidebar, the toolbar, and the header all re-render when `rateLimits` changes in an unrelated session.

**Fix — slice by domain (stay on `useReducer` if you like; this is about files, not libraries):**
- `src/stores/slices/session-slice.ts` — sessions, archived, active id, focus; the `session_*` / `turn_*` cases.
- `src/stores/slices/pending-slice.ts` — pending permissions / questions / mode overrides; the `consume_*` actions and `recomputeAwaiting`.
- `src/stores/slices/provider-slice.ts` — providers, rateLimits, sessionCommands; `provider_*`, `rate_limit_updated`, `session_command_catalog_updated`.
- `src/stores/slices/project-slice.ts` — projects, projectDisplay, projectWorktrees, sessionDisplay, plus all the rename/reorder/create/link/unlink callbacks.
- `src/stores/root-store.tsx` — composes slices; owns the *single* `connectStream`; exposes `useSessionSlice()`, `usePendingSlice()`, etc. so a component can subscribe to only the slice it reads.

### 5.3 [P1] Double `connectStream` — every ServerMessage delivered twice
**Files:** `stores/app-store.tsx:852` and `components/chat/chat-view.tsx:1179`
Both sites independently allocate a Tauri `Channel<ServerMessage>`. The backend sees two subscribers; every event is routed through both channels into different handlers. This is wasted IPC, but the subtler problem is that it forces chat-view to duplicate "apply to cache" logic that conceptually belongs in the store.

**Fix:** keep exactly one `connectStream` in the root store. Expose `addEventListener(fn)` or a React context of `RuntimeEvent`. Chat-view subscribes to that instead of opening a second channel. Falls out naturally from §5.1 + §5.2.

### 5.4 [P1] Provider metadata hardcoded in five places
**Files:**
- `src/lib/defaults-settings.ts:26, 31` — `DEFAULT_ENABLED_PROVIDERS`, `ALL_PROVIDER_KINDS`.
- `src/components/sidebar/provider-constants.tsx:4, 13` — `PROVIDER_COLORS`, `ALL_PROVIDERS`.
- `src/components/settings/settings-view.tsx:53, 61, 69` — third color table, labels, order.
- `src/components/usage/usage-cost-chart.tsx:17` — fourth color table, keyed differently.
- `src/hooks/use-provider-enabled.tsx:27` — inline `ALL: ProviderKind[]`.

Adding a provider today means touching four or five frontend files plus the Rust enum. Exactly the "provider-specific knowledge should be in one place" rule you care about.

**Fix:** one module `src/lib/providers.ts`:
```ts
export const PROVIDER_KINDS: readonly ProviderKind[] = [...];
export const PROVIDER_META: Record<ProviderKind, {
  label: string; color: string; order: number;
  defaultEnabled: boolean; slashPrefix: "/" | "$";
}> = { ... };
```
Delete the four other copies and import from one place. The lone remaining provider-name branch is `src/lib/slash-commands.ts:236` (`if (provider === "codex") return $${name}`) — fold into `slashPrefix` on the meta record. After this, adding a provider = edit `providers.ts` + the Rust enum.

### 5.5 [P1] Re-render cliff in chat-view
Every keystroke that updates `permissionMode` dispatches `set_session_permission_mode` (chat-view.tsx:638–649), which re-renders every `useApp()` consumer. Fixed structurally by §5.2. Separate nit: `applyEventToTurns` at L291 (`tool_call_completed`) allocates a new `toolCalls` array even when the target tool call isn't found — cheap identity check there would skip the re-render.

### 5.6 [P2] `api.ts` — six logical bundles in one 676-line file
**File:** `apps/flowstate/src/lib/api.ts`
Not terrible, but it combines: (a) sendMessage/invoke wrappers L1–105, (b) git/worktree/pty type coercion L107–298, (c) Tauri `Channel` streaming L239–298, 621–676, (d) local-SQLite CRUD L308–418, (e) usage analytics L430–537, (f) code-view FS helpers L539–602.

**Fix:** split into `src/lib/api/{rpc,git,pty,display,usage,fs}.ts` + barrel `index.ts`. No call-site changes required.

### 5.7 [P2] Four async idioms in one component
`chat-view.tsx` mixes react-query, raw Promise-in-useEffect (with `cancelled` flag), and Tauri `Channel` callbacks. Three ways to do the same thing makes the next refactorer guess which pattern applies.

**Fix:** standardize — react-query for anything cacheable; `useEffect` only for subscriptions. Touch-ups as each hook gets extracted in §5.1.

### 5.8 [P2] Swallowed errors
- `stores/app-store.tsx:856–866` — `.catch(() => {})` on provider enablement burst. Intentional but unmarked; at minimum `console.debug` once.
- `chat-view.tsx:1045–1049` — interrupt failure is logged and dropped. User sees no toast. Wrap in a destructive toast.

### 5.9 [P2] Small housekeeping
- `src/components/chat/message-model-info.tsx` is only used by `agent-message.tsx` — move it into `messages/`.
- `src/lib/session-diff.ts` is a 7-line file with one type — inline into `git-diff-stream.ts` or delete.
- `providerKind` is prop-drilled ChatView → MessageList → TurnView → AgentMessage → MessageModelInfo. Replace with a tiny `<SessionContext value={{ sessionId, provider, model }}>` at ChatView's root; leaves read via hook. Kills four prop hops and one memo dep.

---

## Phase 6 — Housekeeping

**Why:** residual debt that's cheap to clean up but actively compounds while it sits — dead crates slowing `cargo check`, duplicate dep declarations, READMEs that describe a layout that no longer matches disk. Individually small; collectively they shave ~900 LOC and eliminate a real Windows bug.

### 6.1 [P1] Dead crates still built on every `cargo check`
**Files:**
- `crates/middleman/daemon-client` — 506 LOC. Its own `README.md:5` admits *"nothing here links daemon-client today."* Grep confirms: no `use zenui_daemon_client` anywhere.
- `crates/middleman/transport-http` — 375 LOC + README. Lives behind a feature flag that no Cargo.toml enables. The README references a `zenui` binary target that does not exist in the tree.

Both compile on every PR CI run for zero runtime benefit.

**Fix:** drop both from `[workspace.members]`. Move to an `experimental/` directory (not in the workspace) if you want to preserve the code for future use. Update any README that references them as active.

### 6.2 [P1] Feature-flag cruft in `daemon-core`
**File:** `crates/middleman/daemon-core/Cargo.toml:7`
`transport-http`, `all-transports`, and the per-provider-CLI feature flags (`provider-claude-cli`, `provider-github-copilot-cli`) are **never enabled by any consumer**. `apps/flowstate/src-tauri/Cargo.toml:47` hard-codes `features = ["all-providers", "transport-tauri"]` — so the granular flags are only reachable via `all-providers` and are never individually exercised.

That's a silent bitrot risk: a compile error inside `#[cfg(feature = "provider-codex")]` won't be caught until someone tries to build `--features provider-codex` standalone, which no one does.

**Fix:** either drop the individually-granular provider flags (keep only `all-providers`), or add a CI matrix that does `cargo check --no-default-features --features provider-<x>,transport-tauri` for each. Pick one; don't leave them uncovered.

### 6.3 [P1] `directories` vs `dirs` dependency confusion
**Files:** root `Cargo.toml:31` declares `directories = "5"` in `[workspace.dependencies]` — but **zero crates use it**. Meanwhile `dirs = "5"` (a different crate from the same author) is independently declared in:
- `provider-claude-sdk/Cargo.toml:11`
- `provider-github-copilot/Cargo.toml:12`
- `embedded-node/Cargo.toml:10`
- `apps/flowstate/src-tauri/Cargo.toml:29`

Four ad-hoc declarations for what should be a workspace-level dep.

**Fix:** delete `directories` from workspace deps; add `dirs = "5"`; convert all four per-crate declarations to `dirs.workspace = true`. One consistent version pin across the whole workspace.

### 6.4 [P1] Redundant version pins that ignore the workspace
- `persistence/Cargo.toml:11` redeclares `rusqlite` with a specific version though it's already in `[workspace.dependencies]`.
- `src-tauri/Cargo.toml:71` pins `rusqlite = "0.37"` a third time.
- `provider-claude-sdk/Cargo.toml:12` and `provider-github-copilot/Cargo.toml:12` both pin `rust-embed = "8"` identically; lift to workspace.
- `tauri = "2"` declared in both `src-tauri/Cargo.toml:16` and `transport-tauri/Cargo.toml:13`; lift to workspace.

**Fix:** lift each to `[workspace.dependencies]` and convert consumers to `.workspace = true`. Prevents a minor-version drift bug where one crate upgrades and another doesn't.

### 6.5 [P1] Tracing is initialized twice
**Files:**
- `apps/flowstate/src-tauri/src/lib.rs:1678` sets `flowstate=info,zenui=info,warn`.
- Then `bootstrap_core_async` at `crates/middleman/daemon-core/src/lib.rs:175–176` calls its own `init_tracing()` with a different filter (`zenui=debug,warn`).

Both use `.try_init()` so the second is a silent no-op **in this process**. But any fresh-process caller (a future standalone daemon, an integration test) hits the library's filter, which is not what the app set.

**Fix:** remove the `init_tracing()` call from `bootstrap_core_async`. Expose it as `pub fn init_tracing()` that binaries explicitly opt into. Libraries don't configure global logging.

### 6.6 [P1] README drift — user-visible and contributor-visible
- Root `README.md:14` advertises a Windows build. `.github/workflows/build.yml:22` has the Windows matrix block commented out. Either re-enable CI or strip Windows from the README.
- `crates/middleman/daemon-core/README.md:215` and `transport-http/README.md:19` describe a `zenui` binary that doesn't exist in the tree.
- `crates/README.md` and `crates/middleman/README.md:41` place `daemon-client` in the dep graph without noting its dormancy.

**Fix:** mechanical edits. Bring the docs in line with reality or delete the claims.

### 6.7 [P2] `bootstrap_core` (the sync variant) is unused
**File:** `crates/middleman/daemon-core/src/lib.rs`
Only `bootstrap_core_async` is called from `src-tauri`. The sync `bootstrap_core` + `run_blocking` only make sense for the phantom standalone daemon binary.

**Fix:** feature-gate behind `standalone-binary`, or delete. No point keeping an unused public entry point compiling.

### 6.8 [P2] `PROTOCOL_VERSION` is a lie
**File:** `crates/middleman/daemon-core/src/ready_file.rs:14`
The writer stamps `protocol_version: 1`. The client at `daemon-client/src/ready_file.rs` reads the field into a struct and never checks it. The code even claims *"v1 support is scheduled for removal"* — but there's no v1 branch, only v2 schema exists. The version field conveys no information.

**Fix:** either delete the field or have the client reject unknown versions with a clear upgrade message. Also add a protocol-version field to `BootstrapPayload` so WS mismatches fail loudly, not with silent half-decoding.

### 6.9 [P2] Ready-file format is duplicated, with a real Windows bug
**Files:** `daemon-core/src/ready_file.rs` vs `daemon-client/src/ready_file.rs`
The two copies must stay byte-for-byte compatible; there's no test asserting that. And they've already drifted — the `daemon-client` copy is **missing the `#[cfg(windows)] ProjectDirs` branch** at `daemon-core/src/ready_file.rs:124–129`. On Windows, client and daemon look in different directories and never find each other.

**Fix:** extract a leaf crate `zenui-ready-file` (serde + anyhow only) consumed by both sides. Mostly obsoleted if §6.1 drops `daemon-client`.

### 6.10 [P2] WebSocket session protocol copy-pasted between transports
**Files:** `transport-http/src/lib.rs:255–363` vs `transport-tauri/src/commands.rs:20–85`
Both implement the same state machine: subscribe → receive Welcome → stream Events → on `Lagged` request a fresh Snapshot and reseed the active session.

**Fix:** extract into `runtime-core::stream_session()` async helper. Both transports become thin adapters. Also mostly obsoleted if §6.1 drops `transport-http`.

### 6.11 [P2] `transport-tauri` shutdown is broken (but hidden by Tauri's process lifecycle)
**File:** `crates/middleman/transport-tauri/src/lib.rs:102–135`
The handle stores a `_shutdown_rx` that nobody ever reads. The `connect` loop doesn't observe it. On shutdown, `TauriHandle::shutdown` fires its observer callback, but the actual long-running `connect` task keeps running against a runtime that's about to drop. Works in practice only because Tauri owns the process teardown.

**Fix:** add an `AtomicBool` or `watch::Sender<bool>` to `TauriDaemonState`; `connect`'s `select!` observes it alongside `rx.recv()`. Honest shutdown path.

### 6.12 [P2] Unbounded WS channel — potential daemon OOM
**File:** `crates/middleman/transport-http/src/lib.rs:279`
`unbounded_channel::<ServerMessage>()` + `tokio::spawn` per inbound request at L342 with no backpressure. A hostile or buggy client can fill the queue without bound.

**Fix:** bounded `mpsc::channel(256)` + `try_send` that drops with a `ServerMessage::Error` on Full. Obsoleted if `transport-http` goes (§6.1).

### 6.13 [P2] Tauri `handle_message` bypasses the idle watchdog
**File:** `crates/middleman/transport-tauri/src/commands.rs:89–95`
Only `connect` notifies the observer about connect/disconnect. A client that only sends invocations (no streaming subscription) is invisible to the idle watchdog — the daemon could decide to idle-shutdown while requests are in flight.

**Fix:** wrap `handle_message` in an activity tick (observer `on_activity`, or on_connected/on_disconnected around each call).

### 6.14 [P2] Daemon stderr leaks via `Stdio::inherit()`
**File:** `crates/middleman/daemon-core/src/spawn.rs:66`
When the daemon is spawned, its stderr inherits the parent's — so daemon logs leak into whatever shell spawned the app. Redirect to `config.log_file`.

### 6.15 [P2] Hardcoded model fallback lists are 1–2 revs stale
**Files:**
- `provider-claude-cli/src/lib.rs:1468`
- `provider-claude-sdk/src/lib.rs:2338`
- `provider-github-copilot/src/lib.rs:1258`
- `provider-codex/src/lib.rs:967`

These are intentional fallbacks (used when the dynamic model fetch hasn't returned yet), but audit date is 2026-04-18 and the lists trail the current catalogs.

**Fix:** consolidate into `crates/core/provider-api/src/default_models.rs` with a dated comment and a quarterly refresh reminder. Not a bug; a freshness task.

### 6.16 [P2] Build-script `panic!` produces ugly compile output
**File:** `crates/middleman/embedded-node/build.rs:84`
On Node download failure, `panic!("failed to download Node.js from {url}")` dumps a scary stack trace.

**Fix:** `println!("cargo:error=…"); std::process::exit(1);` — same failure, clean Cargo output.

### 6.17 [P2] Silent fallthrough for unknown reasoning efforts
**File:** `crates/core/persistence/src/lib.rs:1336–1345`
`reasoning_effort_from_str` returns `None` on unknown strings. If a future provider adds a new effort level, DB round-trips preserve it but replay silently loses it.

**Fix:** `tracing::warn!` on unknown values so the drift shows up in logs.

---

## Appendix — suggested execution order for other agents

Each phase is self-contained enough that one agent can execute it without reloading the full audit. Point the agent at the phase section plus the files it touches.

1. **Phase 0** — shortest; unblocks everything else. Do first.
2. **Phase 1** — abstraction leaks; small surgical edits, each self-contained. Do before provider dedup so the new trait methods exist.
3. **Phase 2** — provider scaffolding dedup; benefits from the new trait shape in Phase 1.
4. **Phase 3** — god-file splits. Mechanical. Do last of the Rust work so earlier edits aren't fighting merge conflicts in giant files.
5. **Phase 4** — `src-tauri` surgery. Independent of Phases 1–3 but benefits from the new `usage-store` / `git-ops` crates slotting in cleanly.
6. **Phase 5** — frontend split. Independent of Rust phases except §5.4 (provider metadata), which benefits from §6.3 + §2.4 landing first.
7. **Phase 6** — housekeeping. Can run in parallel with Phase 5 or as a final sweep.

### LOC accounting

- **Phase 2 dedup:** ~840 LOC removed across five provider crates (~7% of ~10.9k adapter LOC).
- **Phase 3 god-file splits:** net-neutral on LOC; files drop to 400–700 each.
- **Phase 4 `src-tauri` surgery:** ~3,400 LOC of domain logic leaves `src-tauri`, replaced by ~800 LOC of forwarder glue.
- **Phase 5 frontend:** `chat-view.tsx` 2,073 → ~350; `app-store.tsx` 1,170 → four slice files of 200–300 each.
- **Phase 6 housekeeping:** ~900 LOC of dead crates / duplicated ready-file / duplicated WS protocol disappears if §6.1 + §6.9 + §6.10 all land.

### What's explicitly NOT recommended (wishful-refactor filter)

- **No state-library migration.** Don't swap `useReducer` for Zustand/Redux. The slice split in §5.2 achieves the goal without the churn.
- **No Storybook / component-library swap / router swap.**
- **No adapter-trait rewrite into actors or typed state machines.** §1.8 is the only trait-shape change proposed and it's additive (new capability traits), not disruptive.
- **No change to bridge IPC framing** for providers that already work (Codex, Copilot-CLI keep their native framing).
- **No new tests** except the one wire-format round-trip per provider (§0.4) that's needed to unblock refactoring safely.
- **Stale model fallback lists (§6.15)** are flagged as a freshness task, not an architecture bug.
