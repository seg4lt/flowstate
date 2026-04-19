// Startup-path fatal-error helper.
//
// Several spots in `run()` `.expect()` on operations that — if they
// fail — abort the app before the window is ever shown. The user
// sees a silent dock bounce with no diagnostic. This module is a
// tiny wrapper that shows a native error dialog via
// `tauri_plugin_dialog` and then exits with a non-zero status, so
// failures produce a visible, debuggable message instead of a silent
// crash.
//
// Phase 4.5 of the architecture audit.

use tauri::AppHandle;
use tauri_plugin_dialog::{DialogExt, MessageDialogKind};

/// Display a blocking native error dialog with `msg`, log the error,
/// then call `std::process::exit(1)`. Never returns.
///
/// `context` is the short human-readable phase ("open user config",
/// "resolve app data dir") — we prepend it to the detail message in
/// both the log and the dialog so a user reporting a bug has enough
/// context to identify what step failed.
pub fn show_and_exit(app: &AppHandle, context: &str, error: impl std::fmt::Display) -> ! {
    let detail = format!("{context}: {error}");
    tracing::error!("fatal startup error: {detail}");
    // `blocking_show` waits for the user to dismiss the dialog. Best
    // effort — if the dialog subsystem itself isn't ready (e.g. the
    // OS window server is unavailable) we still exit with the log
    // line above rather than hang.
    let _ = app
        .dialog()
        .message(&detail)
        .title("Flowstate failed to start")
        .kind(MessageDialogKind::Error)
        .blocking_show();
    std::process::exit(1);
}

/// Convenience wrapper: unwrap a `Result`, calling `show_and_exit`
/// with `context` on error. Used to replace `.expect("…")` on the
/// startup path.
pub trait FatalExpect<T> {
    fn fatal(self, app: &AppHandle, context: &str) -> T;
}

impl<T, E: std::fmt::Display> FatalExpect<T> for Result<T, E> {
    fn fatal(self, app: &AppHandle, context: &str) -> T {
        match self {
            Ok(v) => v,
            Err(e) => show_and_exit(app, context, e),
        }
    }
}
