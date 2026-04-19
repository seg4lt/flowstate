use std::path::Path;
use std::process::Command;

/// Launch an external code editor on `path` (the project root) by
/// running `<editor> .` with the child's cwd set to `path`. All the
/// standard editor launchers — `zed`, `code`, `cursor`, `idea`,
/// `subl` — treat `.` relative to cwd as "open this directory as a
/// project", and some of them behave better with `.` than with an
/// absolute path positional.
///
/// We spawn and detach a reaper thread that calls `wait()` on the
/// child so it doesn't sit around as a `<defunct>` zombie after it
/// exits. We intentionally don't kill the editor when flowstate quits.
///
/// Returns `Err` with a readable message when:
///   * `path` isn't a directory (no project to open)
///   * the editor binary isn't on `$PATH` (e.g. user picked
///     "Zed" but never installed Zed's `zed` CLI helper)
#[tauri::command]
pub fn open_in_editor(editor: String, path: String) -> Result<(), String> {
    let trimmed = editor.trim();
    if trimmed.is_empty() {
        return Err("no editor configured".into());
    }
    let project_path = Path::new(&path);
    if !project_path.is_dir() {
        return Err(format!("not a directory: {path}"));
    }
    let mut child = Command::new(trimmed)
        .arg(".")
        .current_dir(project_path)
        .spawn()
        .map_err(|e| format!("failed to launch `{trimmed}`: {e}"))?;
    std::thread::spawn(move || {
        let _ = child.wait();
    });
    Ok(())
}
