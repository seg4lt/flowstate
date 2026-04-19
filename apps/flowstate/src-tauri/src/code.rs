// ─────────────────────────────────────────────────────────────────
// /code editor view — file picker + single-file read
// ─────────────────────────────────────────────────────────────────

use std::path::Path;
use std::sync::{Arc, Mutex};

use serde::{Deserialize, Serialize};

/// Cap the picker list so we never send a million entries to the
/// frontend for a huge repo. 20k is more than enough for a Cmd+P
/// picker — anyone with more than that would already be using real
/// file search (see fff-search upgrade path in follow-ups).
const PROJECT_FILE_LIST_MAX: usize = 20_000;

/// Maximum file size we'll inline into the code view. The editor
/// uses @pierre/diffs' Virtualizer so a 10k-line plain-text file is
/// fine, but anything past this is probably generated / binary /
/// not useful to read inline and we return a placeholder marker.
const CODE_VIEW_MAX_FILE_BYTES: u64 = 4 * 1024 * 1024;

/// List every file in `path` that isn't ignored by .gitignore,
/// .ignore, etc. Respects hidden-file convention (skips dotfiles)
/// and avoids the usual suspects (node_modules, target, dist, …)
/// via `ignore::WalkBuilder`'s standard filters. Returns relative
/// paths (forward-slash), sorted, capped at PROJECT_FILE_LIST_MAX.
///
/// Uses `WalkBuilder::build_parallel` so the gitignore walk fans
/// out across CPU cores. On a multi-core machine with SSD this is
/// 2-4x faster than the serial walker for large repos, which is
/// the dominant cost on a cold open of the /code view's picker.
#[tauri::command]
pub fn list_project_files(path: String) -> Vec<String> {
    use ignore::WalkState;

    let project_path = Path::new(&path);
    if !project_path.is_dir() {
        return Vec::new();
    }

    // Match `git status` visibility: honor .gitignore (local, global,
    // and .git/info/exclude), but don't silently drop dotfolders or
    // `.ignore` files the way ripgrep does by default.
    let entries: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
    let project_path_owned = project_path.to_path_buf();

    let thread_count = std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(4);

    ignore::WalkBuilder::new(project_path)
        .hidden(false)
        .ignore(false)
        .git_ignore(true)
        .git_global(true)
        .git_exclude(true)
        .parents(true)
        .require_git(false)
        .threads(thread_count)
        .build_parallel()
        .run(|| {
            let entries = Arc::clone(&entries);
            let project_path = project_path_owned.clone();
            Box::new(move |result| {
                let Ok(entry) = result else {
                    return WalkState::Continue;
                };
                // Only files — directories get walked into automatically.
                if !entry.file_type().is_some_and(|t| t.is_file()) {
                    return WalkState::Continue;
                }
                let abs = entry.path();
                let Ok(rel) = abs.strip_prefix(&project_path) else {
                    return WalkState::Continue;
                };
                // Forward-slash path, platform-normalised, so the
                // frontend can pattern-match without caring about
                // Windows back-slashes.
                let rel_str = rel
                    .components()
                    .map(|c| c.as_os_str().to_string_lossy())
                    .collect::<Vec<_>>()
                    .join("/");
                if rel_str.is_empty() {
                    return WalkState::Continue;
                }
                let mut guard = match entries.lock() {
                    Ok(g) => g,
                    Err(poisoned) => poisoned.into_inner(),
                };
                if guard.len() >= PROJECT_FILE_LIST_MAX {
                    return WalkState::Quit;
                }
                guard.push(rel_str);
                WalkState::Continue
            })
        });

    let mut entries = Arc::try_unwrap(entries)
        .ok()
        .and_then(|m| m.into_inner().ok())
        .unwrap_or_default();
    entries.sort();
    entries
}

/// Return the contents of a single project file as a UTF-8 string.
/// Used by the /code editor view when the user opens a file from
/// the picker. Caps the payload so opening a binary / generated
/// mega-file doesn't freeze the bridge.
#[tauri::command]
pub fn read_project_file(path: String, file: String) -> Result<String, String> {
    let project_path = Path::new(&path);
    let abs = project_path.join(&file);
    // Canonicalise both and make sure the requested file is
    // actually inside the project root. Without this, a crafted
    // `file = "../../etc/passwd"` could escape — not a big deal
    // for a local-only desktop app but cheap to defend against.
    let project_canon = project_path
        .canonicalize()
        .map_err(|e| format!("project path: {e}"))?;
    let abs_canon = abs
        .canonicalize()
        .map_err(|e| format!("file path: {e}"))?;
    if !abs_canon.starts_with(&project_canon) {
        return Err("file is outside the project root".into());
    }
    let meta = std::fs::metadata(&abs_canon)
        .map_err(|e| format!("metadata: {e}"))?;
    if meta.len() > CODE_VIEW_MAX_FILE_BYTES {
        return Err(format!(
            "file too large to inline: {} bytes (max {})",
            meta.len(),
            CODE_VIEW_MAX_FILE_BYTES
        ));
    }
    std::fs::read_to_string(&abs_canon).map_err(|e| format!("read: {e}"))
}

/// One line inside a `ContentBlock`. `is_match` distinguishes the
/// matching line(s) from the surrounding context lines so the
/// frontend can highlight them.
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct BlockLine {
    line: u64,
    text: String,
    is_match: bool,
}

/// A contiguous run of lines from one file that contains at least
/// one match plus its surrounding context. Matches close together
/// in the same file share a single block (`grep_searcher` issues
/// a `context_break` between disjoint groups, which we use as the
/// block boundary).
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ContentBlock {
    path: String,
    /// 1-based line number of the first line in `lines` — handy
    /// for the frontend gutter even though every line carries its
    /// own `line` field too.
    start_line: u64,
    lines: Vec<BlockLine>,
}

/// How many context lines to capture on each side of every match.
/// Three is the Zed multibuffer default and it's what the picker
/// header explains as "top 3 / bottom 3". Expand-to-more is a
/// next-step item.
const CONTENT_SEARCH_CONTEXT_LINES: usize = 3;

/// Soft cap on total lines streamed across all blocks for one
/// query. Bounds the IPC payload so a pathological query (`a`)
/// can't ship megabytes through the bridge. Each line averages
/// around 60–80 chars, so 3000 lines ≈ ~200 KB of JSON.
const CONTENT_SEARCH_MAX_TOTAL_LINES: usize = 3_000;

/// Per-line truncation. Long lines (minified bundles, lockfiles)
/// get clipped + ellipsised so a single 100k-char line can't blow
/// up the payload either.
const CONTENT_SEARCH_MAX_LINE_LEN: usize = 240;

/// Custom `grep_searcher::Sink` that builds `ContentBlock`s as it
/// receives lines from the searcher. The default `sinks::UTF8`
/// only forwards match lines; we need both match AND context, so
/// we implement Sink ourselves and use `context_break` events to
/// separate disjoint match groups within a file.
struct BlockSink {
    rel_path: String,
    finished_blocks: Vec<ContentBlock>,
    current: Option<ContentBlock>,
    /// Shared budget across all files for one query. Decremented
    /// on every line we accept; once it hits zero we tell the
    /// searcher to stop by returning `Ok(false)`.
    line_budget_remaining: usize,
}

impl BlockSink {
    fn new(rel_path: String, line_budget_remaining: usize) -> Self {
        Self {
            rel_path,
            finished_blocks: Vec::new(),
            current: None,
            line_budget_remaining,
        }
    }

    fn push_line(&mut self, line_number: u64, text: String, is_match: bool) {
        if self.current.is_none() {
            self.current = Some(ContentBlock {
                path: self.rel_path.clone(),
                start_line: line_number,
                lines: Vec::new(),
            });
        }
        if let Some(block) = self.current.as_mut() {
            block.lines.push(BlockLine {
                line: line_number,
                text,
                is_match,
            });
            self.line_budget_remaining =
                self.line_budget_remaining.saturating_sub(1);
        }
    }

    fn flush_current(&mut self) {
        if let Some(block) = self.current.take() {
            // Only keep blocks that actually contain at least one
            // match. A pure-context block (no matched lines) means
            // the searcher emitted before/after context for a match
            // we already accounted for in a previous block — skip.
            if block.lines.iter().any(|l| l.is_match) {
                self.finished_blocks.push(block);
            }
        }
    }
}

fn truncate_line(raw: &[u8]) -> String {
    let text = std::str::from_utf8(raw).unwrap_or("");
    let text = text.trim_end_matches(['\n', '\r']);
    if text.len() > CONTENT_SEARCH_MAX_LINE_LEN {
        // Find a char boundary at or before the cap so we don't
        // split a multi-byte character mid-codepoint.
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

impl grep_searcher::Sink for BlockSink {
    type Error = std::io::Error;

    fn matched(
        &mut self,
        _searcher: &grep_searcher::Searcher,
        mat: &grep_searcher::SinkMatch<'_>,
    ) -> Result<bool, Self::Error> {
        if self.line_budget_remaining == 0 {
            return Ok(false);
        }
        let line_number = mat.line_number().unwrap_or(0);
        let text = truncate_line(mat.bytes());
        self.push_line(line_number, text, true);
        Ok(self.line_budget_remaining > 0)
    }

    fn context(
        &mut self,
        _searcher: &grep_searcher::Searcher,
        ctx: &grep_searcher::SinkContext<'_>,
    ) -> Result<bool, Self::Error> {
        if self.line_budget_remaining == 0 {
            return Ok(false);
        }
        let line_number = ctx.line_number().unwrap_or(0);
        let text = truncate_line(ctx.bytes());
        self.push_line(line_number, text, false);
        Ok(self.line_budget_remaining > 0)
    }

    fn context_break(
        &mut self,
        _searcher: &grep_searcher::Searcher,
    ) -> Result<bool, Self::Error> {
        self.flush_current();
        Ok(self.line_budget_remaining > 0)
    }

    fn finish(
        &mut self,
        _searcher: &grep_searcher::Searcher,
        _finish: &grep_searcher::SinkFinish,
    ) -> Result<(), Self::Error> {
        self.flush_current();
        Ok(())
    }
}

/// Per-search options sent from the frontend's advanced controls.
/// Defaults intentionally match "boring literal case-sensitive
/// search with no path filtering" so omitting the field on the
/// frontend behaves like the old two-arg command.
#[derive(Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct ContentSearchOptions {
    /// When true the query is treated as a `regex` crate regex,
    /// matching ripgrep's default dialect. When false the query
    /// is passed to grep-regex with `.fixed_strings(true)` so
    /// users can paste raw code fragments without escaping.
    #[serde(default)]
    use_regex: bool,
    /// Default true (matches the user's expectation that "Foo"
    /// doesn't match "foo" out of the box). The `aA` toggle in
    /// the UI flips this off.
    #[serde(default = "default_true")]
    case_sensitive: bool,
    /// Glob patterns to RESTRICT the walk to (ripgrep
    /// OverrideBuilder includes). Empty list means "everywhere".
    #[serde(default)]
    includes: Vec<String>,
    /// Glob patterns to EXCLUDE from the walk. The frontend sends
    /// plain globs; we prefix them with `!` for OverrideBuilder
    /// since that's the convention it expects.
    #[serde(default)]
    excludes: Vec<String>,
}

fn default_true() -> bool {
    true
}

/// Live content search across the project, ripgrep-style. Walks
/// the same gitignore-aware tree as `list_project_files` (with
/// optional include/exclude glob overrides) and runs each file
/// through ripgrep's own `Searcher`. The query is literal by
/// default (`fixed_strings(true)`) so users can paste raw code
/// fragments like `fn foo(` or `->` without escaping; flipping
/// `useRegex` switches into the full `regex` crate dialect.
/// `caseSensitive` defaults to true; flip it off for an
/// `aA`-style insensitive search.
///
/// Returns one `ContentBlock` per disjoint match group per file:
/// each block is the match line(s) plus 3 lines of context on
/// either side. The frontend renders these as Zed-style
/// multibuffer chunks.
#[tauri::command]
pub fn search_file_contents(
    path: String,
    query: String,
    options: ContentSearchOptions,
) -> Result<Vec<ContentBlock>, String> {
    use grep_regex::RegexMatcherBuilder;
    use grep_searcher::SearcherBuilder;
    use ignore::overrides::OverrideBuilder;

    let trimmed = query.trim();
    if trimmed.is_empty() {
        return Ok(Vec::new());
    }
    let project_path = Path::new(&path);
    if !project_path.is_dir() {
        return Ok(Vec::new());
    }

    let matcher = RegexMatcherBuilder::new()
        .case_insensitive(!options.case_sensitive)
        .fixed_strings(!options.use_regex)
        .build(trimmed)
        .map_err(|e| format!("regex build: {e}"))?;

    let mut searcher = SearcherBuilder::new()
        .line_number(true)
        .before_context(CONTENT_SEARCH_CONTEXT_LINES)
        .after_context(CONTENT_SEARCH_CONTEXT_LINES)
        .build();

    // Build glob overrides from include/exclude lists, if any.
    // OverrideBuilder treats a leading `!` as "exclude" and bare
    // patterns as "include" — we hide that detail from users and
    // let them type plain globs in the exclude box.
    let overrides = if !options.includes.is_empty() || !options.excludes.is_empty() {
        let mut ob = OverrideBuilder::new(project_path);
        for inc in &options.includes {
            let trimmed = inc.trim();
            if trimmed.is_empty() {
                continue;
            }
            ob.add(trimmed)
                .map_err(|e| format!("include glob `{trimmed}`: {e}"))?;
        }
        for exc in &options.excludes {
            let trimmed = exc.trim();
            if trimmed.is_empty() {
                continue;
            }
            let pat = if trimmed.starts_with('!') {
                trimmed.to_string()
            } else {
                format!("!{trimmed}")
            };
            ob.add(&pat)
                .map_err(|e| format!("exclude glob `{trimmed}`: {e}"))?;
        }
        Some(
            ob.build()
                .map_err(|e| format!("override build: {e}"))?,
        )
    } else {
        None
    };

    let mut wb = ignore::WalkBuilder::new(project_path);
    wb.hidden(false)
        .ignore(false)
        .git_ignore(true)
        .git_global(true)
        .git_exclude(true)
        .parents(true)
        .require_git(false);
    if let Some(ov) = overrides {
        wb.overrides(ov);
    }

    let mut all_blocks: Vec<ContentBlock> = Vec::new();
    let mut lines_remaining = CONTENT_SEARCH_MAX_TOTAL_LINES;

    'walk: for result in wb.build() {
        if lines_remaining == 0 {
            break;
        }
        let Ok(entry) = result else { continue };
        if !entry.file_type().is_some_and(|t| t.is_file()) {
            continue;
        }
        let abs = entry.path();
        let Ok(rel) = abs.strip_prefix(project_path) else {
            continue;
        };
        let rel_str = rel
            .components()
            .map(|c| c.as_os_str().to_string_lossy())
            .collect::<Vec<_>>()
            .join("/");
        if rel_str.is_empty() {
            continue;
        }

        let mut sink = BlockSink::new(rel_str, lines_remaining);
        let _ = searcher.search_path(&matcher, abs, &mut sink);
        // The sink decrements its own budget; carry the remainder
        // into the next file's sink so the global cap holds.
        lines_remaining = sink.line_budget_remaining;
        all_blocks.append(&mut sink.finished_blocks);

        if lines_remaining == 0 {
            break 'walk;
        }
    }

    Ok(all_blocks)
}
