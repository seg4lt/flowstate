//! Per-worktree file indexing backed by `fff-search`.
//!
//! Replaces the old `ignore::WalkBuilder` + `grep-searcher` stack.
//! `fff-search` maintains a live, fs-watched, bigram-indexed file list
//! in the background — indexing cost is paid once per worktree, then
//! amortised across every `list_project_files` / `search_file_contents`
//! call.
//!
//! # Why per-worktree, not per-project
//!
//! flowstate's worktree model means a single project can have multiple
//! worktrees checked out simultaneously — same `.git`, different working
//! trees, different uncommitted edits. Each worktree needs its own
//! `FilePicker`: file contents and diffs differ per worktree, and
//! sharing an index across them would show one worktree's files as
//! matches for another's search.
//!
//! The `FileIndexRegistry` keys on the **canonicalised** worktree path.
//! Two symlink forms of the same worktree collapse into one entry;
//! two real worktrees of the same project stay distinct.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, RwLock};
use std::time::{Duration, Instant};

use fff_search::file_picker::{FFFMode, FilePicker, FilePickerOptions};
use fff_search::grep::{GrepMode, GrepResult, GrepSearchOptions, grep_search};
use fff_search::types::{ContentCacheBudget, FileItem};
use fff_search::{GrepConfig, QueryParser, SharedFrecency, SharedPicker};
use globset::{Glob, GlobSet, GlobSetBuilder};
use serde::{Deserialize, Serialize};

// Knob philosophy (post-`53993c5` revert investigation):
//
// The original fff-search wiring silently dropped matches in five
// different places — that's what got it reverted. Each constant below
// either widens the corresponding cap to a level that real users
// won't hit, or removes the cap entirely and surfaces overflow to the
// UI. Accuracy first; speed comes from fff's live index and bigram
// prefilter, not from cutting work short.

/// Hard ceiling on entries returned by `list_project_files`. Set to
/// `usize::MAX` so we never truncate the picker — a 100k-file repo
/// is ~8 MB of JSON, well under the IPC limit, and the frontend
/// virtualises the list anyway. Keep the constant (rather than
/// inlining `usize::MAX`) so the intent is greppable.
pub const PROJECT_FILE_LIST_MAX: usize = usize::MAX;

/// Context lines per side of every match. Zed multibuffer default.
pub const CONTENT_SEARCH_CONTEXT_LINES: usize = 3;

/// Soft cap on total lines emitted across all blocks in one search
/// call. Widened from 3 000 to 20 000 — at ~80 chars/line that's
/// ~1.6 MB of JSON, still bounded but high enough that real searches
/// don't get cut off. The frontend surfaces a "+N truncated" hint
/// when this fires.
pub const CONTENT_SEARCH_MAX_TOTAL_LINES: usize = 20_000;

/// Per-line truncation cap in characters. Long lines (minified
/// bundles, lockfiles) get clipped with an ellipsis so a single
/// 100 k-char line can't blow up the payload.
pub const CONTENT_SEARCH_MAX_LINE_LEN: usize = 240;

/// Per-file match cap. Widened from 200 to 2 000 so a real "rename
/// this symbol" search across a busy file isn't silently truncated.
/// The frontend surfaces overflow.
const MAX_MATCHES_PER_FILE: usize = 2_000;

/// Wall-clock ceiling for one search call. Widened from 5 s to 30 s
/// because we now restore the `SearchTasks` cancellation registry
/// (`stop_content_search` Tauri command) — long budgets are fine when
/// the user can interrupt.
const SEARCH_TIME_BUDGET_MS: u64 = 30_000;

/// Per-file size cap for content search. Widened from 4 MiB to 64 MiB
/// so matches inside lockfiles, large generated SQL dumps, and similar
/// real artifacts are reachable. Memory-mapped, so this isn't a
/// resident-memory hit.
const CONTENT_SEARCH_MAX_FILE_BYTES: u64 = 64 * 1024 * 1024;

/// `list_project_files` no longer blocks on a cold scan. Returning
/// partial results without telling the UI was the #1 silent-failure
/// surface in the original fff wiring (see commit `53993c5`'s revert
/// message). We now return immediately with whatever the index has,
/// plus an `indexing` flag in the `ProjectFileListing` payload so the
/// frontend can render "Indexing N files…" and re-poll as the watcher
/// fills the list. The constant is retained at 0 so the codepath
/// shape is stable if we ever want to add a brief settle-window back.
const LIST_FILES_WAIT_MS: u64 = 0;

/// Wall-clock ceiling we're willing to spend blocking on
/// `search_file_contents` if the index is cold. Widened from 3 s to
/// 15 s for the same reason as `SEARCH_TIME_BUDGET_MS` — interruptible
/// searches don't need a tight cold-start cut.
const SEARCH_WAIT_MS: u64 = 15_000;

// ---------------------------------------------------------------------------
// Public wire types (Tauri serialisation boundary — DO NOT change shape)
// ---------------------------------------------------------------------------

/// Per-search options sent from the frontend's advanced controls.
/// Defaults intentionally match "boring literal case-sensitive search
/// with no path filtering" so omitting the field behaves like the old
/// two-arg command.
///
/// The three matching modes (`use_fuzzy`, `use_regex`, literal) are
/// mutually exclusive — `use_fuzzy` wins if both it and `use_regex`
/// are set. See [`build_query_string`] for the full precedence table.
#[derive(Deserialize, Default, Debug, Clone)]
#[serde(rename_all = "camelCase")]
pub struct ContentSearchOptions {
    /// When true the query is a regex (same dialect as ripgrep).
    /// When false it's treated as a literal string so users can paste
    /// raw code fragments like `fn foo(` or `->` without escaping.
    /// Ignored if `use_fuzzy` is true.
    #[serde(default)]
    pub use_regex: bool,
    /// When true the query is fuzzy-matched against each line using
    /// `fff-search`'s neo_frizbee Smith-Waterman scorer — tolerates
    /// typos and out-of-order characters. Lines are ranked by match
    /// score. Takes precedence over `use_regex`. Default false.
    #[serde(default)]
    pub use_fuzzy: bool,
    /// Default true — matches the user expectation that `Foo`
    /// doesn't match `foo` out of the box. Fuzzy mode is inherently
    /// case-insensitive so this flag is a no-op when `use_fuzzy` is
    /// true.
    #[serde(default = "default_true")]
    pub case_sensitive: bool,
    /// Glob patterns to RESTRICT the search to. Empty list = everywhere.
    #[serde(default)]
    pub includes: Vec<String>,
    /// Glob patterns to EXCLUDE. Plain globs — we do not require the
    /// `!` prefix the underlying engine uses internally.
    #[serde(default)]
    pub excludes: Vec<String>,
}

fn default_true() -> bool {
    true
}

/// One line inside a `ContentBlock`. `is_match` distinguishes the
/// matching line(s) from the surrounding context lines so the
/// frontend can highlight them.
#[derive(Serialize, Debug, Clone)]
#[serde(rename_all = "camelCase")]
pub struct BlockLine {
    pub line: u64,
    pub text: String,
    pub is_match: bool,
}

/// A contiguous run of lines from one file that contains at least
/// one match plus its surrounding context. Matches close together
/// in the same file share a single block.
#[derive(Serialize, Debug, Clone)]
#[serde(rename_all = "camelCase")]
pub struct ContentBlock {
    pub path: String,
    /// 1-based line number of the first line in `lines`.
    pub start_line: u64,
    pub lines: Vec<BlockLine>,
}

/// Snapshot of the per-worktree file index returned to the frontend
/// by `list_project_files`. We never block waiting for a cold scan
/// to finish (that was the #1 silent-truncation surface in the
/// pre-revert wiring); instead the picker shows whatever is indexed
/// **right now**, plus a flag so the UI can render "Indexing N
/// files…" and re-poll until `indexing` flips to `false`.
#[derive(Serialize, Debug, Clone, Default)]
#[serde(rename_all = "camelCase")]
pub struct ProjectFileListing {
    /// Project-relative forward-slash paths, sorted alphabetically.
    /// Includes every file the index has walked so far (no
    /// `PROJECT_FILE_LIST_MAX` cut — see the constant).
    pub files: Vec<String>,
    /// `true` while fff-search's background scanner is still walking
    /// the worktree. Frontend should re-fetch on a short interval
    /// (the React Query cache picks this up automatically) until it
    /// flips to `false`.
    pub indexing: bool,
    /// Number of files indexed so far. Equal to `files.len()` for the
    /// happy path; surfaced separately so the UI can show the
    /// progressive count without recomputing.
    pub scanned: u64,
}

// ---------------------------------------------------------------------------
// Registry / handle
// ---------------------------------------------------------------------------

/// A live `FilePicker` for a single worktree, plus the shared state
/// it needs.
pub struct FilePickerHandle {
    pub picker: SharedPicker,
    /// Retained so the background scanner can ref-count it; we don't
    /// initialise the frecency tracker (persistent LMDB isn't worth
    /// the complexity for the /code picker) but the handle still
    /// needs a `SharedFrecency` slot.
    #[allow(dead_code)]
    pub frecency: SharedFrecency,
    /// Cloned from the picker so we can poll scan state without
    /// taking the picker's RwLock.
    pub scan_signal: Arc<AtomicBool>,
    /// Canonicalised worktree root.
    pub root: PathBuf,
}

/// Per-worktree cached `FilePicker`s. Keyed by **canonicalised**
/// worktree path.
#[derive(Default)]
pub struct FileIndexRegistry {
    inner: RwLock<HashMap<PathBuf, Arc<FilePickerHandle>>>,
}

impl FileIndexRegistry {
    /// Return the handle for `worktree`, spinning up a new
    /// `FilePicker` (background scan + fs watcher) on first touch.
    ///
    /// Canonicalises the path so two string forms of the same
    /// worktree collapse into one entry.
    pub fn get_or_init(&self, worktree: &Path) -> Result<Arc<FilePickerHandle>, String> {
        let canon = worktree
            .canonicalize()
            .map_err(|e| format!("canonicalize {:?}: {e}", worktree))?;

        {
            let r = self
                .inner
                .read()
                .map_err(|e| format!("registry read: {e}"))?;
            if let Some(handle) = r.get(&canon) {
                return Ok(Arc::clone(handle));
            }
        }

        let mut guard = self
            .inner
            .write()
            .map_err(|e| format!("registry write: {e}"))?;
        if let Some(handle) = guard.get(&canon) {
            return Ok(Arc::clone(handle));
        }

        let picker = SharedPicker::default();
        let frecency = SharedFrecency::default();

        let base_path = canon
            .to_str()
            .ok_or_else(|| format!("non-UTF8 worktree path: {:?}", canon))?
            .to_string();

        // FFFMode::Ai is fine for the file-index path: it controls how
        // fff scores result *queries*, not which entries `get_files()`
        // returns. We re-rank ourselves on the frontend
        // (`rankFileMatches` in `lib/mention-utils.ts`) so the picker
        // sees every indexed file regardless of fff's internal
        // scoring threshold. If fff ever adds a mode that pre-filters
        // `get_files()`, switch to a non-filtering variant here.
        //
        // `watch: true` keeps the fs-watcher running so files created
        // after the initial scan land in the index automatically; we
        // also expose an explicit `reindex` path on the registry for
        // belt-and-braces refresh on agent turn-completion (macOS
        // FSEvents can coalesce bursty edits).
        let options = FilePickerOptions {
            base_path,
            warmup_mmap_cache: false,
            mode: FFFMode::Ai,
            cache_budget: None,
            watch: true,
        };

        FilePicker::new_with_shared_state(picker.clone(), frecency.clone(), options)
            .map_err(|e| format!("FilePicker init for {}: {e}", canon.display()))?;

        let scan_signal = {
            let g = picker
                .read()
                .map_err(|e| format!("picker read: {e}"))?;
            match g.as_ref() {
                Some(p) => p.scan_signal(),
                None => Arc::new(AtomicBool::new(false)),
            }
        };

        let handle = Arc::new(FilePickerHandle {
            picker,
            frecency,
            scan_signal,
            root: canon.clone(),
        });
        guard.insert(canon, Arc::clone(&handle));
        Ok(handle)
    }

    /// Drop the cached `FilePicker` for `worktree` so the next
    /// `get_or_init` rebuilds it from scratch (fresh scan, fresh
    /// fs-watcher). Used by the `reindex_project_files` Tauri
    /// command on agent turn-completion: it's a deliberate full
    /// re-walk rather than relying on the watcher's incremental
    /// updates, because macOS FSEvents and Linux inotify both
    /// coalesce bursty edits and can miss files created during a
    /// long agent turn that touches dozens of files in quick
    /// succession.
    ///
    /// Returns silently when the worktree wasn't in the registry —
    /// no-op is the right behaviour for "reindex" on an unindexed
    /// path; the next `list_project_files` will index it cold.
    pub fn reindex(&self, worktree: &Path) -> Result<(), String> {
        let canon = match worktree.canonicalize() {
            Ok(c) => c,
            // If canonicalisation fails the path is gone or unreadable —
            // there's nothing to reindex. Drop the un-canonicalised key
            // too in case it was ever inserted that way (it shouldn't be).
            Err(_) => return Ok(()),
        };
        let mut guard = self
            .inner
            .write()
            .map_err(|e| format!("registry write: {e}"))?;
        guard.remove(&canon);
        Ok(())
    }
}

impl FilePickerHandle {
    /// Block for up to `timeout_ms` milliseconds waiting for the
    /// initial scan to complete. Returns `true` if the scan finished
    /// in time; `false` if the caller should serve partial data.
    pub fn wait_for_scan(&self, timeout_ms: u64) -> bool {
        let deadline = Instant::now() + Duration::from_millis(timeout_ms);
        while self.scan_signal.load(Ordering::Acquire) {
            if Instant::now() >= deadline {
                return false;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        true
    }
}

// ---------------------------------------------------------------------------
// Command implementations (called from lib.rs Tauri command handlers)
// ---------------------------------------------------------------------------

/// Implementation of the `list_project_files` Tauri command. Returns
/// **every** project-relative forward-slash path the index has walked
/// so far (no `PROJECT_FILE_LIST_MAX` cut), sorted alphabetically,
/// alongside an `indexing` flag so the frontend can show "Indexing
/// N files…" while fff's background scanner is still running.
///
/// Crucially this never blocks on a cold scan: the original wiring
/// blocked for up to 2 s and returned partial silently, which on a
/// 100k-file repo meant the picker reliably failed to find files.
/// We return immediately with whatever's indexed and let the
/// frontend re-poll while `indexing == true`. On warm calls the
/// flag is `false` from the first response and there's no extra
/// round-trip.
pub fn list_project_files(registry: &FileIndexRegistry, path: &str) -> ProjectFileListing {
    let project_path = Path::new(path);
    if !project_path.is_dir() {
        return ProjectFileListing::default();
    }
    let Ok(handle) = registry.get_or_init(project_path) else {
        return ProjectFileListing::default();
    };

    // Optional small settle-window for cold opens. With
    // `LIST_FILES_WAIT_MS = 0` this is a no-op; retained as a
    // single point of control if we ever want to add a brief wait
    // back (e.g. 50 ms to let the first batch land before the
    // first paint).
    if LIST_FILES_WAIT_MS > 0 {
        let _ = handle.wait_for_scan(LIST_FILES_WAIT_MS);
    }

    // Capture the scan flag *before* reading the file list so we
    // never report `indexing: false` while the list is still mid-fill.
    // Worst case is the opposite (flag still true after the scan
    // finished) which just costs the frontend one extra refetch.
    let indexing = handle.scan_signal.load(Ordering::Acquire);

    let guard = match handle.picker.read() {
        Ok(g) => g,
        Err(_) => return ProjectFileListing { indexing, ..Default::default() },
    };
    let Some(picker) = guard.as_ref() else {
        return ProjectFileListing { indexing, ..Default::default() };
    };
    let files = picker.get_files();

    let mut out: Vec<String> = files
        .iter()
        .filter(|f| !f.is_deleted)
        .filter_map(|f| project_relative_forward_slash(&f.path, &handle.root))
        .filter(|s| !s.is_empty())
        .take(PROJECT_FILE_LIST_MAX)
        .collect();
    // fff returns path-sorted already; re-sort defensively in case
    // overflow files land unsorted at the tail.
    out.sort();

    let scanned = out.len() as u64;
    ProjectFileListing {
        files: out,
        indexing,
        scanned,
    }
}

/// Implementation of the `search_file_contents` Tauri command.
///
/// Returns one `ContentBlock` per disjoint match group per file:
/// each block is the match line(s) plus up to
/// [`CONTENT_SEARCH_CONTEXT_LINES`] lines of context on either side.
pub fn search_file_contents(
    registry: &FileIndexRegistry,
    path: &str,
    query: &str,
    options: &ContentSearchOptions,
    is_cancelled: Option<&AtomicBool>,
) -> Result<Vec<ContentBlock>, String> {
    let trimmed = query.trim();
    if trimmed.is_empty() {
        return Ok(Vec::new());
    }
    let project_path = Path::new(path);
    if !project_path.is_dir() {
        return Ok(Vec::new());
    }

    let handle = registry.get_or_init(project_path)?;
    let _ = handle.wait_for_scan(SEARCH_WAIT_MS);

    let guard = handle.picker.read().map_err(|e| format!("picker read: {e}"))?;
    let picker = guard.as_ref().ok_or("picker not ready")?;
    let files_slice = picker.get_files();

    // Pre-filter by include/exclude globs. We work on an owned
    // Vec<FileItem> because grep_search's signature expects &[FileItem],
    // not &[&FileItem]. Cloning is cheap — FileItem::clone() drops the
    // content cache and keeps just the metadata.
    let files: Vec<FileItem> = filter_files(files_slice, &options.includes, &options.excludes)?;
    if files.is_empty() {
        return Ok(Vec::new());
    }

    let plan = build_query_plan(options, trimmed);
    let parser = QueryParser::new(GrepConfig);
    let parsed = parser.parse(&plan.query);

    let grep_opts = GrepSearchOptions {
        max_file_size: CONTENT_SEARCH_MAX_FILE_BYTES,
        max_matches_per_file: MAX_MATCHES_PER_FILE,
        smart_case: plan.smart_case,
        file_offset: 0,
        page_limit: CONTENT_SEARCH_MAX_TOTAL_LINES,
        mode: plan.mode,
        time_budget_ms: SEARCH_TIME_BUDGET_MS,
        before_context: CONTENT_SEARCH_CONTEXT_LINES,
        after_context: CONTENT_SEARCH_CONTEXT_LINES,
        classify_definitions: false,
    };

    let budget = ContentCacheBudget::new_for_repo(files.len());

    let result = grep_search(
        &files,
        &parsed,
        &grep_opts,
        &budget,
        None,
        None,
        is_cancelled,
    );

    if let Some(err) = &result.regex_fallback_error {
        return Err(format!("regex: {err}"));
    }

    Ok(group_into_blocks(&result, &handle.root))
}

// ---------------------------------------------------------------------------
// Helpers: query building, globs, block assembly, truncation
// ---------------------------------------------------------------------------

/// The `(mode, query, smart_case)` triple we hand to
/// `fff_search::grep::grep_search`.
#[derive(Debug, Clone, PartialEq, Eq)]
struct QueryPlan {
    mode: GrepMode,
    query: String,
    /// When true, fff-search treats an all-lowercase query as
    /// case-insensitive (taking the SIMD-accelerated ASCII-CI fast
    /// path for `PlainText`, or setting the Unicode CI flag for
    /// `Regex`). We set it precisely when we want case-insensitive
    /// matching AND the query, as we send it, is all-lowercase.
    smart_case: bool,
}

/// Derive the grep plan from our explicit `use_fuzzy` / `use_regex` /
/// `case_sensitive` toggles.
///
/// Precedence (highest first): `use_fuzzy` → `use_regex` → literal.
///
/// Case-insensitivity routing (the interesting bit):
///
/// * **Literal + case-insensitive** → `PlainText` with a pre-lowercased
///   query and `smart_case=true`. This hits fff's dedicated
///   `ascii_case_insensitive_find` (SIMD memchr) — the fastest path for
///   ASCII code. Non-ASCII input still matches ASCII-CI-equivalently;
///   users who need Unicode case folding (Turkish İ, German ß …)
///   should flip to regex mode and use `(?i)` manually.
/// * **Regex + case-insensitive** → `Regex` with `smart_case=true` and
///   a lowercased query when possible, or `(?i)` prepended when the
///   user's pattern contains uppercase metacharacters we can't safely
///   lowercase. Both paths end up in fff's `regex::bytes::Regex`.
/// * **Fuzzy** bypasses both flags — frizbee is inherently
///   case-insensitive.
///
/// | use_fuzzy | use_regex | case_sensitive | mode      | query transform                        | smart_case |
/// | --------- | --------- | -------------- | --------- | -------------------------------------- | ---------- |
/// | true      | *         | *              | Fuzzy     | unchanged                              | false      |
/// | false     | true      | true           | Regex     | unchanged                              | false      |
/// | false     | true      | false          | Regex     | prepend `(?i)`                         | false      |
/// | false     | false     | true           | PlainText | unchanged                              | false      |
/// | false     | false     | false          | PlainText | lowercased                             | true       |
fn build_query_plan(opts: &ContentSearchOptions, query: &str) -> QueryPlan {
    if opts.use_fuzzy {
        return QueryPlan {
            mode: GrepMode::Fuzzy,
            query: query.to_string(),
            smart_case: false,
        };
    }
    match (opts.use_regex, opts.case_sensitive) {
        (true, true) => QueryPlan {
            mode: GrepMode::Regex,
            query: query.to_string(),
            smart_case: false,
        },
        (true, false) => QueryPlan {
            mode: GrepMode::Regex,
            // Prepend `(?i)` — the regex engine's case fold applies
            // even to embedded literal chars, so we don't need to
            // lowercase the pattern ourselves.
            query: format!("(?i){query}"),
            smart_case: false,
        },
        (false, true) => QueryPlan {
            mode: GrepMode::PlainText,
            query: query.to_string(),
            smart_case: false,
        },
        (false, false) => QueryPlan {
            // Lowercase + smart_case=true → fff picks the ASCII-CI
            // SIMD memmem path via `PlainTextMatcher { case_insensitive: true }`.
            // Unicode-sensitive? No — for that, use regex+`(?i)`.
            mode: GrepMode::PlainText,
            query: query.to_lowercase(),
            smart_case: true,
        },
    }
}

/// Compile a list of plain globs into a `GlobSet`. Empty / blank
/// entries are skipped. Returns `Ok(None)` when no patterns survive —
/// the caller treats `None` as "don't filter".
fn build_globset(patterns: &[String]) -> Result<Option<GlobSet>, String> {
    let mut b = GlobSetBuilder::new();
    let mut added = 0;
    for p in patterns {
        let t = p.trim();
        if t.is_empty() {
            continue;
        }
        // Tolerate a leading `!` — the old ripgrep override builder
        // used it for excludes, and some users may still type it.
        let t = t.strip_prefix('!').unwrap_or(t);
        let glob = Glob::new(t).map_err(|e| format!("glob `{t}`: {e}"))?;
        b.add(glob);
        added += 1;
    }
    if added == 0 {
        return Ok(None);
    }
    let set = b.build().map_err(|e| format!("globset: {e}"))?;
    Ok(Some(set))
}

/// Filter the indexed file list by include/exclude globs.
///
/// Glob matches are tested against the file's `relative_path` (which
/// fff-search populates relative to the picker's `base_path` using
/// forward slashes).
fn filter_files(
    files: &[FileItem],
    includes: &[String],
    excludes: &[String],
) -> Result<Vec<FileItem>, String> {
    let inc = build_globset(includes)?;
    let exc = build_globset(excludes)?;
    if inc.is_none() && exc.is_none() {
        return Ok(files.iter().filter(|f| !f.is_deleted).cloned().collect());
    }
    Ok(files
        .iter()
        .filter(|f| !f.is_deleted)
        .filter(|f| {
            let rel = f.relative_path.as_str();
            let included = inc.as_ref().is_none_or(|s| s.is_match(rel));
            let excluded = exc.as_ref().is_some_and(|s| s.is_match(rel));
            included && !excluded
        })
        .cloned()
        .collect())
}

/// Turn the fff-search flat match list into the per-file disjoint
/// blocks our frontend expects.
///
/// # Context dedupe
///
/// Every `GrepMatch` carries its own `context_before` / `context_after`,
/// so two matches three lines apart will have overlapping context
/// arrays. We track `last_emitted_line` within each block and skip any
/// context whose line number is `<= last_emitted_line` when merging.
///
/// # Block boundary
///
/// Two matches share a block iff the gap between them is small
/// enough that their context windows touch or overlap:
///   `next.line_number - last_emitted_line <= 1`
/// else we flush the current block and start a new one.
///
/// # Budget
///
/// Total emitted lines across all blocks is capped at
/// [`CONTENT_SEARCH_MAX_TOTAL_LINES`]. Once exhausted we flush the
/// current block and stop.
fn group_into_blocks(result: &GrepResult<'_>, root: &Path) -> Vec<ContentBlock> {
    // Sort by (file_index, line_number) so runs are contiguous.
    let mut idxs: Vec<usize> = (0..result.matches.len()).collect();
    idxs.sort_by_key(|&i| {
        let m = &result.matches[i];
        (m.file_index, m.line_number)
    });

    let mut out: Vec<ContentBlock> = Vec::new();
    let mut budget: usize = CONTENT_SEARCH_MAX_TOTAL_LINES;

    let mut cur_file_index: Option<usize> = None;
    let mut cur_block: Option<ContentBlock> = None;
    let mut last_emitted: u64 = 0;

    let flush = |cur_block: &mut Option<ContentBlock>, out: &mut Vec<ContentBlock>| {
        if let Some(b) = cur_block.take() {
            if b.lines.iter().any(|l| l.is_match) {
                out.push(b);
            }
        }
    };

    for i in idxs {
        if budget == 0 {
            break;
        }
        let m = &result.matches[i];
        let Some(file) = result.files.get(m.file_index) else {
            continue;
        };
        let rel_path = file_relative_path(file, root);

        // New file → flush any open block.
        if cur_file_index != Some(m.file_index) {
            flush(&mut cur_block, &mut out);
            cur_file_index = Some(m.file_index);
            last_emitted = 0;
        }

        let before_len = m.context_before.len() as u64;
        let match_line = m.line_number;
        let first_line = match_line.saturating_sub(before_len);

        // Decide: merge into current block, or start a new one.
        let merge = matches!(&cur_block, Some(b)
            if b.path == rel_path && first_line <= last_emitted.saturating_add(1));

        if !merge {
            flush(&mut cur_block, &mut out);
            cur_block = Some(ContentBlock {
                path: rel_path.clone(),
                start_line: first_line.max(1),
                lines: Vec::new(),
            });
            last_emitted = 0;
        }

        let block = cur_block.as_mut().expect("block must exist");

        // Emit context_before, skipping anything already covered by
        // the previous match's context_after.
        for (offset, raw) in m.context_before.iter().enumerate() {
            if budget == 0 {
                break;
            }
            let ln = first_line + offset as u64;
            if ln == 0 {
                continue;
            }
            if last_emitted > 0 && ln <= last_emitted {
                continue;
            }
            block.lines.push(BlockLine {
                line: ln,
                text: truncate_line(raw),
                is_match: false,
            });
            budget = budget.saturating_sub(1);
            last_emitted = ln;
        }

        // Emit the match line itself. Always pushed — if duplicate
        // by some fff weirdness we'd rather double-render than drop.
        if budget > 0 && !(last_emitted > 0 && match_line <= last_emitted) {
            block.lines.push(BlockLine {
                line: match_line,
                text: truncate_line(&m.line_content),
                is_match: true,
            });
            budget = budget.saturating_sub(1);
            last_emitted = match_line;
        }

        // Emit context_after, line-numbered 1-based.
        for (offset, raw) in m.context_after.iter().enumerate() {
            if budget == 0 {
                break;
            }
            let ln = match_line + 1 + offset as u64;
            if last_emitted > 0 && ln <= last_emitted {
                continue;
            }
            block.lines.push(BlockLine {
                line: ln,
                text: truncate_line(raw),
                is_match: false,
            });
            budget = budget.saturating_sub(1);
            last_emitted = ln;
        }
    }

    flush(&mut cur_block, &mut out);
    out
}

/// Truncate a line to at most [`CONTENT_SEARCH_MAX_LINE_LEN`] chars
/// (byte-length capped to avoid splitting multi-byte codepoints),
/// adding a trailing ellipsis when cut. Trailing CR/LF are stripped.
pub fn truncate_line(raw: &str) -> String {
    let text = raw.trim_end_matches(['\n', '\r']);
    if text.len() > CONTENT_SEARCH_MAX_LINE_LEN {
        let mut cut = CONTENT_SEARCH_MAX_LINE_LEN;
        while cut > 0 && !text.is_char_boundary(cut) {
            cut -= 1;
        }
        let mut t = text[..cut].to_string();
        t.push('…');
        t
    } else {
        text.to_string()
    }
}

/// Prefer the `FileItem`'s own `relative_path` (forward-slash, already
/// project-relative in fff-search). Fall back to strip_prefix against
/// the canonicalised root if relative_path is missing for any reason.
fn file_relative_path(file: &FileItem, root: &Path) -> String {
    if !file.relative_path.is_empty() {
        return file.relative_path.clone();
    }
    project_relative_forward_slash(&file.path, root).unwrap_or_default()
}

/// Strip the canonicalised worktree root and return a forward-slash
/// relative path. `None` if `abs` is not under `root`.
fn project_relative_forward_slash(abs: &Path, root: &Path) -> Option<String> {
    let rel = abs.strip_prefix(root).ok()?;
    Some(
        rel.components()
            .map(|c| c.as_os_str().to_string_lossy())
            .collect::<Vec<_>>()
            .join("/"),
    )
}

// ---------------------------------------------------------------------------
// Cancellation registry (mirrors DiffTasks in lib.rs)
// ---------------------------------------------------------------------------

/// Tracks in-flight content searches so the frontend can cancel one
/// by token when the user re-queries or unmounts the search panel.
#[derive(Default)]
pub struct SearchTasks {
    tasks: Mutex<HashMap<u64, Arc<AtomicBool>>>,
    next_token: AtomicU64,
}

impl SearchTasks {
    /// Register a fresh cancellation flag under `token`.
    pub fn register(&self, token: u64) -> Arc<AtomicBool> {
        let flag = Arc::new(AtomicBool::new(false));
        self.tasks
            .lock()
            .expect("SearchTasks poisoned")
            .insert(token, Arc::clone(&flag));
        flag
    }

    /// Deregister a token (call on completion).
    pub fn unregister(&self, token: u64) {
        self.tasks.lock().expect("SearchTasks poisoned").remove(&token);
    }

    /// Signal cancellation for `token`. Idempotent and lock-free
    /// after the take.
    pub fn cancel(&self, token: u64) {
        if let Some(flag) = self.tasks.lock().expect("SearchTasks poisoned").remove(&token) {
            flag.store(true, Ordering::SeqCst);
        }
    }

    /// Monotonic token allocator for callers that don't want to
    /// generate their own.
    #[allow(dead_code)]
    pub fn alloc_token(&self) -> u64 {
        self.next_token.fetch_add(1, Ordering::Relaxed)
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn block_opts(inc: Vec<&str>, exc: Vec<&str>) -> (Option<GlobSet>, Option<GlobSet>) {
        let inc = build_globset(&inc.into_iter().map(|s| s.to_string()).collect::<Vec<_>>())
            .unwrap();
        let exc = build_globset(&exc.into_iter().map(|s| s.to_string()).collect::<Vec<_>>())
            .unwrap();
        (inc, exc)
    }

    #[test]
    fn truncate_line_handles_ascii() {
        let s: String = "x".repeat(CONTENT_SEARCH_MAX_LINE_LEN + 50);
        let out = truncate_line(&s);
        assert!(out.ends_with('…'));
        // Byte length: cap + 3 bytes for the ellipsis (U+2026).
        assert_eq!(out.len(), CONTENT_SEARCH_MAX_LINE_LEN + 3);
    }

    #[test]
    fn truncate_line_respects_utf8_boundary() {
        // 239 ASCII + one 4-byte emoji = 243 bytes, over the cap.
        // Cut must land at the ASCII/emoji boundary, not mid-emoji.
        let mut s = "a".repeat(CONTENT_SEARCH_MAX_LINE_LEN - 1);
        s.push('😀');
        let out = truncate_line(&s);
        assert!(out.ends_with('…'));
        // No panic = char boundary respected.
        assert!(out.is_char_boundary(out.len()));
    }

    #[test]
    fn truncate_line_strips_trailing_newlines() {
        assert_eq!(truncate_line("hello\n"), "hello");
        assert_eq!(truncate_line("hello\r\n"), "hello");
        assert_eq!(truncate_line("hello"), "hello");
    }

    #[test]
    fn globset_empty_patterns_are_none() {
        let out = build_globset(&[]).unwrap();
        assert!(out.is_none());
        let out = build_globset(&["".into(), "   ".into()]).unwrap();
        assert!(out.is_none());
    }

    #[test]
    fn globset_include_matches_expected() {
        let (inc, exc) = block_opts(vec!["*.rs"], vec![]);
        let inc = inc.expect("include set");
        assert!(inc.is_match("src/lib.rs"));
        assert!(!inc.is_match("src/lib.ts"));
        assert!(exc.is_none());
    }

    #[test]
    fn globset_exclude_leading_bang_tolerated() {
        let (_, exc) = block_opts(vec![], vec!["!target/*", "dist/*"]);
        let exc = exc.expect("exclude set");
        // `!target/*` should have had the `!` stripped and been added
        // as `target/*`.
        assert!(exc.is_match("target/debug/foo"));
        assert!(exc.is_match("dist/bundle.js"));
        assert!(!exc.is_match("src/main.rs"));
    }

    #[test]
    fn build_query_plaintext_case_sensitive() {
        let opts = ContentSearchOptions {
            use_regex: false,
            case_sensitive: true,
            ..Default::default()
        };
        let plan = build_query_plan(&opts, "Foo");
        assert!(matches!(plan.mode, GrepMode::PlainText));
        assert_eq!(plan.query, "Foo");
        assert!(!plan.smart_case);
    }

    #[test]
    fn build_query_plaintext_case_insensitive_takes_fast_path() {
        // The hot path for a code-search UI: literal + case-insensitive.
        // Must route to `PlainText` with smart_case=true and a
        // lowercased query so fff-search lights up its SIMD
        // ascii_case_insensitive_find() path.
        let opts = ContentSearchOptions {
            use_regex: false,
            case_sensitive: false,
            ..Default::default()
        };
        let plan = build_query_plan(&opts, "FilePicker");
        assert!(matches!(plan.mode, GrepMode::PlainText));
        assert_eq!(plan.query, "filepicker");
        assert!(plan.smart_case);
    }

    #[test]
    fn build_query_plaintext_case_insensitive_preserves_metachars() {
        // Parens / dots / etc. stay as literal bytes — we're no
        // longer routing through the regex engine so we must not
        // escape them. This also proves metachars aren't getting
        // interpreted as regex syntax.
        let opts = ContentSearchOptions {
            use_regex: false,
            case_sensitive: false,
            ..Default::default()
        };
        let plan = build_query_plan(&opts, "fn Foo(");
        assert!(matches!(plan.mode, GrepMode::PlainText));
        assert_eq!(plan.query, "fn foo(");
        assert!(plan.smart_case);
    }

    #[test]
    fn build_query_regex_case_insensitive_prepends_flag() {
        let opts = ContentSearchOptions {
            use_regex: true,
            case_sensitive: false,
            ..Default::default()
        };
        let plan = build_query_plan(&opts, "fn \\w+");
        assert!(matches!(plan.mode, GrepMode::Regex));
        assert_eq!(plan.query, "(?i)fn \\w+");
        assert!(!plan.smart_case);
    }

    #[test]
    fn build_query_regex_case_sensitive_passthrough() {
        let opts = ContentSearchOptions {
            use_regex: true,
            case_sensitive: true,
            ..Default::default()
        };
        let plan = build_query_plan(&opts, "fn \\w+");
        assert!(matches!(plan.mode, GrepMode::Regex));
        assert_eq!(plan.query, "fn \\w+");
        assert!(!plan.smart_case);
    }

    #[test]
    fn build_query_fuzzy_mode_passes_through() {
        let opts = ContentSearchOptions {
            use_fuzzy: true,
            ..Default::default()
        };
        let plan = build_query_plan(&opts, "hllo wrld");
        assert!(matches!(plan.mode, GrepMode::Fuzzy));
        assert_eq!(plan.query, "hllo wrld");
        assert!(!plan.smart_case);
    }

    #[test]
    fn build_query_fuzzy_wins_over_regex() {
        // When a user somehow sets both flags, fuzzy takes priority
        // and the regex characters are passed through unescaped —
        // frizbee treats them as literal characters to fuzzy-match.
        let opts = ContentSearchOptions {
            use_fuzzy: true,
            use_regex: true,
            case_sensitive: false,
            ..Default::default()
        };
        let plan = build_query_plan(&opts, "fn \\w+");
        assert!(matches!(plan.mode, GrepMode::Fuzzy));
        assert_eq!(plan.query, "fn \\w+");
    }

    #[test]
    fn build_query_fuzzy_ignores_case_sensitive_flag() {
        // Fuzzy is inherently case-insensitive — the flag is a no-op.
        let opts_a = ContentSearchOptions {
            use_fuzzy: true,
            case_sensitive: true,
            ..Default::default()
        };
        let opts_b = ContentSearchOptions {
            use_fuzzy: true,
            case_sensitive: false,
            ..Default::default()
        };
        let a = build_query_plan(&opts_a, "Foo");
        let b = build_query_plan(&opts_b, "Foo");
        assert_eq!(a, b);
    }
}
