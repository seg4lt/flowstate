//! App-layer implementations of the runtime-core orchestration traits.
//!
//! The runtime owns no display-layer or git knowledge — the `session_title`,
//! `project_name`, and worktree creation all live in the flowstate app's own
//! stores. These adapters bridge them into the orchestration dispatcher so
//! `list_sessions` returns the same titles the sidebar renders, and the agent
//! can spin up a git worktree with a session inside it in one tool call.

use std::path::PathBuf;
use std::sync::{Arc, Weak};

use async_trait::async_trait;
use zenui_runtime_core::{
    AppMetadataProvider, RuntimeCore, WorktreeBlueprint, WorktreeProvisioner,
};

use crate::git_worktree::create_git_worktree_internal;
use crate::user_config::{SessionDisplay, UserConfigStore};

/// Reads user-set titles / project names from the app-owned
/// `session_display` / `project_display` tables. No writes — the
/// runtime never mutates display state.
pub struct AppMetadataProviderImpl {
    store: UserConfigStore,
}

impl AppMetadataProviderImpl {
    pub fn new(store: UserConfigStore) -> Self {
        Self { store }
    }
}

#[async_trait]
impl AppMetadataProvider for AppMetadataProviderImpl {
    async fn session_title(&self, session_id: &str) -> Option<String> {
        // UserConfigStore uses a blocking `std::sync::Mutex` on the
        // rusqlite Connection; moving the query onto a blocking thread
        // keeps the async runtime responsive even when SQLite is
        // contended. The query is microsecond-level in the common
        // case, so the spawn overhead dominates only when there's
        // genuine contention — an acceptable trade.
        let store = self.store.clone();
        let id = session_id.to_string();
        tokio::task::spawn_blocking(move || {
            store
                .get_session_display(&id)
                .ok()
                .flatten()
                .and_then(|d| d.title)
        })
        .await
        .ok()
        .flatten()
    }

    async fn project_name(&self, project_id: &str) -> Option<String> {
        let store = self.store.clone();
        let id = project_id.to_string();
        tokio::task::spawn_blocking(move || {
            store
                .get_project_display(&id)
                .ok()
                .flatten()
                .and_then(|d| d.name)
        })
        .await
        .ok()
        .flatten()
    }

    async fn set_session_title(&self, session_id: &str, title: &str) -> Result<(), String> {
        // Preserve any existing `last_turn_preview` and `sort_order`
        // if the row was already touched (e.g. a previous turn ran,
        // or the user manually reordered this thread). For a fresh
        // spawn both are None on first read, which matches the
        // frontend's `setSessionDisplay` shape exactly.
        let store = self.store.clone();
        let id = session_id.to_string();
        let title = title.to_string();
        tokio::task::spawn_blocking(move || {
            let existing = store.get_session_display(&id).ok().flatten();
            let existing_preview = existing.as_ref().and_then(|d| d.last_turn_preview.clone());
            let existing_sort_order = existing.as_ref().and_then(|d| d.sort_order);
            store.set_session_display(
                &id,
                &SessionDisplay {
                    title: Some(title),
                    last_turn_preview: existing_preview,
                    sort_order: existing_sort_order,
                },
            )
        })
        .await
        .map_err(|e| format!("spawn_blocking join: {e}"))?
    }
}

/// Wraps the existing `create_git_worktree` / `list_git_worktrees_sync`
/// helpers from `lib.rs` into a runtime-facing trait, plus creates the
/// SDK project row for the new worktree and links it in
/// `project_worktree`. The dispatcher uses the returned blueprint to
/// spawn a session inside the new worktree directly.
pub struct WorktreeProvisionerImpl {
    store: UserConfigStore,
    /// Weak back-reference into the runtime so we can call
    /// `create_project_for_path` when a new worktree lands. Weak
    /// breaks the Arc cycle (RuntimeCore → provisioner → RuntimeCore).
    runtime: Weak<RuntimeCore>,
}

impl WorktreeProvisionerImpl {
    pub fn new(store: UserConfigStore, runtime: Weak<RuntimeCore>) -> Self {
        Self { store, runtime }
    }

    /// Derive the on-disk path for a new worktree, matching the
    /// frontend exactly (`apps/flowstate/src/lib/worktree-utils.ts`
    /// `deriveWorktreePath`). That's the one path-layout contract the
    /// app has with its users — the agent-driven path here would
    /// diverge silently if we reinvented it.
    ///
    /// Shape: `{base}/{project-name}-worktrees/{project-name}-{sanitized-branch}`,
    /// where:
    /// - `base` is the user's `worktree.base_path` setting when set,
    ///   else `{dirname(parent_project_path)}/worktrees`.
    /// - `project-name` is the basename of the parent repo path.
    /// - `sanitized-branch` lowercases the branch, replaces any
    ///   non-`[a-z0-9._-]` character with `-`, collapses runs of `-`,
    ///   and trims leading/trailing `-`.
    fn derive_worktree_path(
        parent_path: &str,
        branch: &str,
        configured_base: Option<&str>,
    ) -> Result<String, String> {
        let parent = PathBuf::from(parent_path);
        let project_name = parent
            .file_name()
            .and_then(|s| s.to_str())
            .ok_or_else(|| format!("parent project path `{parent_path}` has no file name"))?
            .to_string();

        let base = match configured_base {
            Some(s) if !s.is_empty() => PathBuf::from(s),
            _ => {
                let parent_dir = parent.parent().ok_or_else(|| {
                    format!("parent project path `{parent_path}` has no parent dir")
                })?;
                parent_dir.join("worktrees")
            }
        };

        let sanitized = sanitize_branch_segment(branch);
        if sanitized.is_empty() {
            return Err(format!(
                "branch name `{branch}` has no path-safe characters"
            ));
        }

        Ok(base
            .join(format!("{project_name}-worktrees"))
            .join(format!("{project_name}-{sanitized}"))
            .to_string_lossy()
            .into_owned())
    }
}

/// Mirror of the frontend's branch→path-segment sanitizer. Lowercase,
/// `[^a-z0-9._-]` → `-`, collapse runs of `-`, trim leading/trailing
/// `-`. Kept in sync with
/// `apps/flowstate/src/lib/worktree-utils.ts:deriveWorktreePath`.
fn sanitize_branch_segment(raw: &str) -> String {
    let mut out = String::with_capacity(raw.len());
    let mut prev_dash = false;
    for c in raw.chars().flat_map(|c| c.to_lowercase()) {
        let ch = if c.is_ascii_alphanumeric() || c == '.' || c == '_' || c == '-' {
            c
        } else {
            '-'
        };
        if ch == '-' {
            if prev_dash {
                continue;
            }
            prev_dash = true;
        } else {
            prev_dash = false;
        }
        out.push(ch);
    }
    out.trim_matches('-').to_string()
}

#[async_trait]
impl WorktreeProvisioner for WorktreeProvisionerImpl {
    async fn create_worktree(
        &self,
        base_project_id: &str,
        branch: &str,
        base_ref: Option<&str>,
        create_branch: bool,
    ) -> Result<WorktreeBlueprint, String> {
        let runtime = self
            .runtime
            .upgrade()
            .ok_or_else(|| "runtime dropped".to_string())?;

        // Resolve the parent project's on-disk path. Agents only pass
        // project ids; we need the filesystem path to shell out to git.
        // `snapshot()` is the cheapest public accessor — returns every
        // live project; filter in-memory.
        let projects = runtime_list_projects(&runtime).await;
        let parent_path = projects
            .into_iter()
            .find(|p| p.project_id == base_project_id)
            .and_then(|p| p.path)
            .ok_or_else(|| format!("project `{base_project_id}` has no path"))?;

        let branch = branch.to_string();
        let base_ref = base_ref.unwrap_or("HEAD").to_string();

        // Honour the user's `worktree.base_path` setting — same key
        // the frontend reads in `readWorktreeBasePath()`.
        let configured_base = {
            let store = self.store.clone();
            tokio::task::spawn_blocking(move || store.get("worktree.base_path"))
                .await
                .map_err(|e| format!("spawn_blocking join: {e}"))??
        };
        let worktree_path =
            Self::derive_worktree_path(&parent_path, &branch, configured_base.as_deref())?;

        // Ensure the parent directory exists before handing the path
        // to git. The frontend's `create-worktree-dialog.tsx` creates
        // this itself (via `ensureDir`); we do the same server-side so
        // `git worktree add` doesn't fail with "parent directory
        // doesn't exist" on the very first worktree a user creates.
        if let Some(parent_dir) = std::path::Path::new(&worktree_path).parent() {
            if let Err(e) = std::fs::create_dir_all(parent_dir) {
                return Err(format!(
                    "failed to create worktree parent dir `{}`: {e}",
                    parent_dir.display()
                ));
            }
        }

        // Run `git worktree add` on a blocking thread (Command is sync).
        let parent_clone = parent_path.clone();
        let wt_clone = worktree_path.clone();
        let branch_clone = branch.clone();
        let base_ref_clone = base_ref.clone();
        let git_result = tokio::task::spawn_blocking(move || {
            create_git_worktree_internal(
                &parent_clone,
                &wt_clone,
                &branch_clone,
                &base_ref_clone,
                !create_branch,
            )
        })
        .await
        .map_err(|e| format!("spawn_blocking join: {e}"))??;
        // `create_git_worktree_internal` returns the canonical path from
        // `git worktree list --porcelain`, which is what we persist —
        // git sometimes resolves `.` or symlinks along the way.
        let canonical_path = git_result.path;
        let canonical_branch = git_result.branch.or(Some(branch.clone()));

        // Create the SDK project row first WITHOUT firing the event,
        // then write the app-side `project_worktree` link, then fire
        // `ProjectCreated`. Order matters: the frontend treats the
        // event as its signal to hydrate the project into the sidebar,
        // so if the link isn't already persisted by the time the event
        // lands, the new worktree paints briefly as an ungrouped,
        // unnamed "Untitled project" at the top level — exactly the
        // flash reported when agents use the worktree tool. With this
        // ordering, when the frontend receives the event and queries
        // the app-side store, the link is guaranteed to be there.
        let project = runtime
            .persist_project_for_path(canonical_path.clone())
            .await?;
        let store = self.store.clone();
        let parent_id = base_project_id.to_string();
        let new_id = project.project_id.clone();
        let link_branch = canonical_branch.clone();
        tokio::task::spawn_blocking(move || {
            store.set_project_worktree(&new_id, &parent_id, link_branch.as_deref())
        })
        .await
        .map_err(|e| format!("spawn_blocking join: {e}"))??;
        runtime.publish(zenui_provider_api::RuntimeEvent::ProjectCreated {
            project: project.clone(),
        });

        Ok(WorktreeBlueprint {
            project_id: project.project_id,
            path: canonical_path,
            branch: canonical_branch,
            parent_project_id: Some(base_project_id.to_string()),
        })
    }

    async fn list_worktrees(
        &self,
        base_project_id: Option<&str>,
    ) -> Result<Vec<WorktreeBlueprint>, String> {
        let store = self.store.clone();
        let parent_filter = base_project_id.map(str::to_string);
        let rows = tokio::task::spawn_blocking(move || store.list_project_worktree())
            .await
            .map_err(|e| format!("spawn_blocking join: {e}"))??;

        let runtime = self
            .runtime
            .upgrade()
            .ok_or_else(|| "runtime dropped".to_string())?;
        let projects = runtime_list_projects(&runtime).await;
        let path_by_id: std::collections::HashMap<String, String> = projects
            .into_iter()
            .filter_map(|p| p.path.map(|path| (p.project_id, path)))
            .collect();

        let mut out = Vec::new();
        for (project_id, rec) in rows {
            if let Some(ref filter) = parent_filter {
                if rec.parent_project_id != *filter {
                    continue;
                }
            }
            let Some(path) = path_by_id.get(&project_id).cloned() else {
                continue;
            };
            out.push(WorktreeBlueprint {
                project_id,
                path,
                branch: rec.branch,
                parent_project_id: Some(rec.parent_project_id),
            });
        }
        Ok(out)
    }
}

/// Helper around the persistence project list — we can't reach into
/// `RuntimeCore`'s private `persistence` field, but `handle_client_message`
/// plus `ClientMessage::LoadSnapshot` would be overkill. We go via
/// `runtime.list_projects_snapshot()` if present, otherwise fall back
/// to the snapshot path.
async fn runtime_list_projects(
    runtime: &Arc<RuntimeCore>,
) -> Vec<zenui_provider_api::ProjectRecord> {
    runtime.snapshot().await.projects
}
