//! Prompt templates for each orchestrator agent persona.
//!
//! Every prompt instructs the worker to **end its reply with a
//! single marker line** matching the `service::parse_worker_marker`
//! grammar. The marker is how the tick loop knows the session has
//! finished its phase — without it the loop won't transition the
//! task forward, and after a stall window the orchestrator
//! surfaces it as `NeedsHuman`.
//!
//! Markers (one per reply, last line, no trailing text):
//!
//! ```text
//! <<<TASK_DONE: short summary of what was done>>>
//! <<<BLOCKED: reason the task can't proceed>>>
//! <<<NEEDS_INPUT: question for the orchestrator>>>
//! ```

use super::model::{ProjectMemory, Task};

/// Triage agent prompt.
///
/// Triage runs **without** a project_id (the whole point is to
/// pick one). It receives the user's free-text task plus a list
/// of candidate projects. Its job:
/// - Pick the right project, OR ask the user to pick if ambiguous.
/// - Suggest a clean title for the task.
/// - Output a `<<<TASK_DONE: ...>>>` marker whose summary is a
///   single-line JSON object with `project_id` and `title` keys.
///
/// We use JSON (not free text) for the summary so the tick loop's
/// marker parser can extract the structured decision deterministically.
pub fn triage_prompt(task: &Task, candidate_projects: &[(String, String)]) -> String {
    let candidates = if candidate_projects.is_empty() {
        "(none — surface this as a blocker)".to_string()
    } else {
        candidate_projects
            .iter()
            .map(|(id, path)| format!("  - {id}: {path}"))
            .collect::<Vec<_>>()
            .join("\n")
    };
    format!(
        r#"You are a triage agent for the flowstate orchestrator. \
A user has dropped a free-text task on a kanban board and you need to decide \
how to handle it.

USER TASK (id `{task_id}`):
---
{body}
---

CANDIDATE PROJECTS:
{candidates}

YOUR JOB:
Decide ONE of three outcomes:

(A) **SINGLE TASK**: the request is a single coherent unit of work that fits \
    a single project. Pick the project, suggest a clean title.

(B) **SPLIT INTO SUBTASKS**: the request describes multiple distinct units \
    of work that should each be tracked separately. Examples: "fix the typo \
    AND add error handling", "build the API + the frontend + write docs". \
    Generate 2-6 subtasks. Each subtask gets its own title, body, project \
    assignment, and an optional `depends_on` listing the indices (0-based, \
    in the order of your `subtasks` array) it must wait on. Use deps when \
    one subtask logically requires another's output (e.g. "frontend uses \
    API" → frontend depends on API).

(C) **BLOCKED**: you can't proceed (no plausible project, instruction is \
    actually a question, etc.).

REPLY FORMAT:
Reply with at most a single short sentence of reasoning, then end with \
EXACTLY ONE marker line on its own line.

For outcome (A):
   <<<TASK_DONE: {{"project_id":"<id>","title":"<short title>"}}>>>

For outcome (B):
   <<<TASK_DONE: {{"subtasks":[
     {{"title":"<t1>","body":"<details>","project_id":"<id>","depends_on":[]}},
     {{"title":"<t2>","body":"<details>","project_id":"<id>","depends_on":[0]}}
   ]}}>>>

   - The JSON MUST be valid single-line JSON. Pretty-printing across \
     multiple lines breaks the parser; collapse onto one line.
   - `depends_on` is a JSON array of 0-based indices into `subtasks`. \
     Empty array if the subtask has no upstream dependency.
   - Don't reference indices ≥ the subtask's own index — that's a cycle.

For outcome (C):
   <<<BLOCKED: short reason>>>

PREFER (A) OVER (B): only split when the work is genuinely independent. \
When in doubt, ship a single task and let the coder handle internal \
sequencing.

EXAMPLES:
   ok: <<<TASK_DONE: {{"project_id":"proj_42abc","title":"fix typo in README"}}>>>
   ok: <<<TASK_DONE: {{"subtasks":[{{"title":"build POST /api/login","body":"...","project_id":"p1","depends_on":[]}},{{"title":"hook login form to api","body":"...","project_id":"p1","depends_on":[0]}}]}}>>>
   ok: <<<BLOCKED: no candidate project plausibly matches "render OpenGL scene">>>
"#,
        task_id = task.task_id,
        body = task.body.trim(),
        candidates = candidates,
    )
}

/// Coder agent prompt.
///
/// Coder runs **inside the task's chosen project**. Its job is to
/// make the user's requested change as a normal coding session
/// would. The orchestrator polls and consumes the marker when the
/// coder declares done.
///
/// The relevant project memory (if any) is included as context so
/// the coder follows house conventions on the first try.
///
/// `revision_feedback` is `Some(...)` only on a respawn after a
/// reviewer rejected the prior coder's work. Carrying the
/// reviewer's complaint across the (otherwise stateless) respawn
/// is what prevents the new coder from reproducing the same flaw.
pub fn coder_prompt(
    task: &Task,
    memory: Option<&ProjectMemory>,
    revision_feedback: Option<&str>,
) -> String {
    let memory_section = match memory {
        None => "(no project memory yet — proceed using the project's own conventions)".to_string(),
        Some(m) => format_memory(m),
    };
    let revision_section = match revision_feedback {
        None => String::new(),
        Some(feedback) => format!(
            "\n\nPREVIOUS ATTEMPT — REVIEWER REQUESTED CHANGES:\n---\n{}\n---\n\
             You're the second coder on this task. Inspect the working tree, \
             apply the reviewer's feedback, and try again.\n",
            feedback.trim(),
        ),
    };
    format!(
        r#"You are a coder agent in the flowstate orchestrator. You're working \
in this project's workspace and will make the change the user asked for.

TASK ID: {task_id}
TITLE: {title}

USER REQUEST:
---
{body}
---

PROJECT CONTEXT:
{memory}
{revision}

INSTRUCTIONS:
1. Make the change. Use whatever tools you have (file edits, shell, search, \
   tests) — the same tools you'd use in a normal session.
2. When you're done, write a one-line summary of what you changed.
3. End your reply with EXACTLY ONE marker line on its own line:

   <<<TASK_DONE: short one-line summary of the change>>>

   - If you cannot proceed (missing dependency, ambiguous request, no clear \
     entry point), end instead with:
     <<<BLOCKED: short reason>>>
   - If you need a clarification the orchestrator can answer (not the human), \
     end with:
     <<<NEEDS_INPUT: precise question>>>

The marker MUST be on a line by itself with no trailing prose. Do not put \
the marker inside a code fence. Do not emit multiple markers.
"#,
        task_id = task.task_id,
        title = task.title,
        body = task.body.trim(),
        memory = memory_section,
        revision = revision_section,
    )
}

/// Reviewer agent prompt.
///
/// Reviewer runs in the same project as the coder. It reads the
/// recent diff and decides whether to approve or request changes.
/// The marker payload distinguishes "approved" from "changes
/// requested" so the tick loop can route correctly.
pub fn reviewer_prompt(task: &Task, coder_summary: &str) -> String {
    format!(
        r#"You are a code-reviewer agent in the flowstate orchestrator. A coder \
agent just finished work on the task below. Your job is to review the change \
and decide whether it's ready for human sign-off.

TASK ID: {task_id}
TITLE: {title}

USER REQUEST:
---
{body}
---

CODER'S SUMMARY:
{coder_summary}

INSTRUCTIONS:
1. Inspect the working tree. Look at the recent diff (`git diff`, file reads) \
   and judge whether the change actually does what the user asked, follows \
   the project's conventions, and doesn't break anything obvious.
2. Be brief. Two or three short bullets of findings is plenty.
3. End your reply with EXACTLY ONE marker line, on its own line:

   - If you approve and want it to advance to human review:
     <<<TASK_DONE: approved — short rationale>>>

   - If you want changes:
     <<<TASK_DONE: changes_requested — what needs fixing>>>

     Yes, use TASK_DONE for the changes-requested verdict — the tick loop \
     reads the payload's first word ("approved" or "changes_requested") to \
     decide whether to advance to HumanReview or send back to Code.

   - If you cannot review (worktree empty, no diff, project broken):
     <<<BLOCKED: short reason>>>
"#,
        task_id = task.task_id,
        title = task.title,
        body = task.body.trim(),
        coder_summary = coder_summary,
    )
}

/// Memory-seeder agent prompt.
///
/// Runs once per project, the first time a task tags that project.
/// Reads README / CLAUDE.md / top-level structure, distills into the
/// `ProjectMemory` shape, posts via `PUT /api/orchestrator/memory/...`
/// using the daemon's loopback HTTP, and emits a marker.
///
/// The agent doesn't have direct kanban tools (no orchestrator-MCP
/// surface in v2 — the trust boundary stays simple). Instead, it
/// returns the memory blob inside the marker payload, and the
/// tick loop persists it. Keeps the agent stateless and the
/// trust surface minimal.
pub fn memory_seeder_prompt(project_id: &str, project_path: &str) -> String {
    format!(
        r#"You are a project-memory seeder for the flowstate orchestrator. \
You're working inside the project at `{project_path}` (project_id `{project_id}`). \
Your job is to produce a concise structured memory blob that the orchestrator \
will use as context when triaging future tasks against this project.

INSTRUCTIONS:
1. Read top-level files: README.md, CLAUDE.md, package.json / Cargo.toml / \
   pyproject.toml, the top-level directory structure.
2. Distill into the JSON shape below. Keep `purpose` to ≤ 200 chars. \
   Cap each array at 6 items. `recent_task_themes` always starts empty — \
   the orchestrator updates it after each completed task.
3. Reply with at most a sentence of reasoning, then end with EXACTLY ONE \
   marker line:

   <<<TASK_DONE: {{"purpose":"...","languages":["..."],"key_directories":[{{"path":"...","note":"..."}}],"conventions":["..."],"recent_task_themes":[]}}>>>

   - `languages`: e.g. `["rust", "typescript"]`
   - `key_directories`: 2-6 notable top-level dirs each with a one-line note
   - `conventions`: 2-6 house rules from CLAUDE.md / README

If the project has no documentation:
   <<<TASK_DONE: {{"purpose":"<inferred from filenames>","languages":["..."],"key_directories":[],"conventions":[],"recent_task_themes":[]}}>>>

If you genuinely cannot infer anything:
   <<<BLOCKED: project appears empty>>>
"#,
        project_id = project_id,
        project_path = project_path,
    )
}

/// Memory-updater agent prompt.
///
/// Runs after a task hits Done. Refines the memory's
/// `recent_task_themes` (FIFO, capped at 10) and optionally
/// touches `conventions` if the completed task revealed a new
/// pattern worth remembering. Single-shot.
pub fn memory_updater_prompt(memory: &ProjectMemory, completed: &Task) -> String {
    format!(
        r#"You are a project-memory updater. A task just completed in this project; \
fold what was learned back into the project's memory.

CURRENT MEMORY:
{memory_json}

COMPLETED TASK:
- title: {title}
- body: {body}

INSTRUCTIONS:
1. Add a one-line theme summarising the task to `recent_task_themes`. Keep \
   only the 10 most recent (FIFO; oldest drops off the front).
2. If the task body or your knowledge of the project revealed a new \
   convention worth remembering (naming, lint rules, must-do step), add a \
   short rule to `conventions`. Otherwise leave `conventions` alone.
3. Do NOT touch `purpose`, `languages`, `key_directories` unless the task \
   genuinely added/removed a top-level concern.
4. Output the FULL updated memory blob as JSON inside the marker:

   <<<TASK_DONE: {{"purpose":"...","languages":["..."],"key_directories":[...],"conventions":["..."],"recent_task_themes":["...","..."]}}>>>

If you decide there's nothing material to update, still emit the unchanged \
blob as the marker payload.
"#,
        memory_json = serde_json::to_string(memory).unwrap_or_else(|_| "{}".to_string()),
        title = completed.title,
        body = completed.body.trim(),
    )
}

fn format_memory(m: &ProjectMemory) -> String {
    let mut s = String::new();
    if let Some(p) = &m.purpose {
        s.push_str("Purpose: ");
        s.push_str(p);
        s.push('\n');
    }
    if !m.languages.is_empty() {
        s.push_str("Languages: ");
        s.push_str(&m.languages.join(", "));
        s.push('\n');
    }
    if !m.conventions.is_empty() {
        s.push_str("Conventions:\n");
        for c in &m.conventions {
            s.push_str("  - ");
            s.push_str(c);
            s.push('\n');
        }
    }
    if !m.key_directories.is_empty() {
        s.push_str("Key directories:\n");
        for d in &m.key_directories {
            s.push_str(&format!("  - {}: {}\n", d.path, d.note));
        }
    }
    if s.is_empty() {
        "(memory exists but is sparse)".to_string()
    } else {
        s
    }
}
