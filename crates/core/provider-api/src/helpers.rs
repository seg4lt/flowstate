//! Small cross-adapter utilities lifted out of individual provider crates.
//!
//! These are intentionally tiny and free of `ProviderAdapter`-specific
//! coupling — the goal is to have exactly one copy of each so independent
//! adapters can't drift on shared concerns (error-line extraction, CLI
//! probing, etc.). If you find yourself reaching for one of these inside
//! a provider crate, import it from here instead of copy-pasting.

use std::path::{Path, PathBuf};

use serde::Serialize;
use serde_json::Value;
use tokio::io::{AsyncWrite, AsyncWriteExt};

use crate::{FileOperation, ProviderTurnEvent, SessionDetail, UserInputOption};

/// Parse an `options` field from a Claude-family `AskUserQuestion`
/// tool-call into the `UserInputOption` shape the UI expects.
///
/// Accepts two provider-side shapes (both observed in Claude's CLI and
/// SDK streams):
///   - Array of objects: `[{ "label": "...", "description": "..." }]`
///   - Array of strings: `["Option A", "Option B"]`
///
/// Option ids are synthesized as `"{question_id}_opt{i}"`. The
/// round-trip path (answer selection → `updated_input.answers` payload)
/// resolves each selected option back to its label, so ids are internal
/// to the adapter and only need to be unique per question.
pub fn parse_options_from_value(val: Option<&Value>, question_id: &str) -> Vec<UserInputOption> {
    let arr = match val.and_then(Value::as_array) {
        Some(a) => a,
        None => return Vec::new(),
    };
    arr.iter()
        .enumerate()
        .map(|(oi, opt)| {
            let id = format!("{question_id}_opt{oi}");
            if let Some(label) = opt.get("label").and_then(Value::as_str) {
                UserInputOption {
                    id,
                    label: label.to_string(),
                    description: opt
                        .get("description")
                        .and_then(Value::as_str)
                        .map(str::to_string),
                }
            } else {
                UserInputOption {
                    id,
                    label: opt.as_str().unwrap_or("").to_string(),
                    description: None,
                }
            }
        })
        .collect()
}

/// Build a `ProviderTurnEvent::FileChange` from a Claude tool-call, or
/// return `None` if `tool_name` isn't one that produces a file change.
///
/// Claude-family adapters (the CLI reading JSONL events and the SDK
/// bridge emitting `tool_use` blocks) both see the same tool names and
/// argument shapes: `Write { file_path, content }`, `Edit { file_path,
/// old_string, new_string }`, plus NotebookEdit / MultiEdit variants
/// Claude is rolling out. Centralizing the mapping here means adding a
/// new tool (or tweaking the arg names upstream) is a single-file edit.
///
/// # Parameters
/// - `call_id`: opaque id echoed back in the matching `ToolCallCompleted`
///   event; persistence keys off this.
/// - `tool_name`: the Claude tool id (`"Write"`, `"Edit"`, `"MultiEdit"`,
///   `"NotebookEdit"`). Unknown names return `None`.
/// - `args`: the tool's `input` object as provided by Claude.
pub fn claude_file_change_from_tool_call(
    call_id: &str,
    tool_name: &str,
    args: &Value,
) -> Option<ProviderTurnEvent> {
    let path = args
        .get("file_path")
        .or_else(|| args.get("notebook_path"))
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    let (operation, before, after) = match tool_name {
        "Write" => (
            FileOperation::Write,
            None,
            args.get("content")
                .and_then(Value::as_str)
                .map(str::to_string),
        ),
        "Edit" | "NotebookEdit" => (
            FileOperation::Edit,
            args.get("old_string")
                .and_then(Value::as_str)
                .map(str::to_string),
            args.get("new_string")
                .and_then(Value::as_str)
                .map(str::to_string),
        ),
        // `MultiEdit { file_path, edits: [{old_string, new_string}, ...] }`.
        // We surface it as a single Edit event — UI renders file-level
        // change anyway; the individual edits aren't currently shown.
        "MultiEdit" => {
            let edits = args.get("edits").and_then(Value::as_array);
            let first_old = edits
                .and_then(|arr| arr.first())
                .and_then(|edit| edit.get("old_string"))
                .and_then(Value::as_str)
                .map(str::to_string);
            let last_new = edits
                .and_then(|arr| arr.last())
                .and_then(|edit| edit.get("new_string"))
                .and_then(Value::as_str)
                .map(str::to_string);
            (FileOperation::Edit, first_old, last_new)
        }
        _ => return None,
    };
    Some(ProviderTurnEvent::FileChange {
        call_id: call_id.to_string(),
        path,
        operation,
        before,
        after,
    })
}

/// Serialize `value` as compact JSON, write it to `writer` followed by a
/// newline, and flush.
///
/// Three adapters (`provider-claude-sdk`, `provider-claude-cli`,
/// `provider-codex`) use the same newline-framed JSONL stdio protocol to
/// talk to their respective child processes. This helper lives here so
/// error wording, flush semantics, and ordering (write body, write '\n',
/// flush) stay identical across them. `describe` is a short noun for the
/// target (e.g. `"bridge"`, `"claude CLI"`, `"codex app-server"`) and
/// appears in error messages.
///
/// Caller owns the lock: pass `&mut *guard` when writing through
/// `Arc<Mutex<ChildStdin>>`. That keeps the helper agnostic to whether
/// the writer is shared or owned exclusively.
pub async fn write_json_line<W, T>(
    writer: &mut W,
    value: &T,
    describe: &str,
) -> Result<(), String>
where
    W: AsyncWrite + Unpin + ?Sized,
    T: Serialize + ?Sized,
{
    let encoded = serde_json::to_string(value)
        .map_err(|e| format!("Failed to serialize {describe} message: {e}"))?;
    writer
        .write_all(encoded.as_bytes())
        .await
        .map_err(|e| format!("Failed to write to {describe} stdin: {e}"))?;
    writer
        .write_all(b"\n")
        .await
        .map_err(|e| format!("Failed to write newline to {describe} stdin: {e}"))?;
    writer
        .flush()
        .await
        .map_err(|e| format!("Failed to flush {describe} stdin: {e}"))
}

/// Human-readable label for an Anthropic rate-limit bucket id.
///
/// Both Claude adapters (`provider-claude-cli` reading the CLI's JSON event
/// stream and `provider-claude-sdk` forwarding from the SDK bridge) surface
/// the same set of bucket ids. Keeping the id→label table here ensures the
/// two Rust adapters — and any future Claude adapter — render identical
/// copy. The TS bridge also carries a fallback copy as its own defensive
/// measure, but Rust is the canonical source.
///
/// Unknown bucket ids are returned verbatim so the UI shows *something*
/// rather than silently dropping a newly-added bucket.
pub fn claude_bucket_label(bucket: &str) -> String {
    match bucket {
        "five_hour" => "5-hour limit".to_string(),
        "seven_day" => "Weekly · all models".to_string(),
        "seven_day_opus" => "Weekly · Opus".to_string(),
        "seven_day_sonnet" => "Weekly · Sonnet".to_string(),
        "overage" => "Overage".to_string(),
        other => other.to_string(),
    }
}

/// Resolve the working directory for a turn on `session`, falling back to
/// `fallback` (typically the adapter's own `working_directory`) when the
/// session has no explicit `cwd` set.
///
/// Lifted from five byte-identical copies across the provider crates so
/// that a future change to the cwd-resolution rule (e.g. validating the
/// path exists, normalizing symlinks) happens in one place.
pub fn session_cwd(session: &SessionDetail, fallback: &Path) -> PathBuf {
    session
        .cwd
        .as_ref()
        .map(PathBuf::from)
        .unwrap_or_else(|| fallback.to_path_buf())
}

/// Return the first non-empty trimmed line of `bytes`, decoded as UTF-8
/// with lossy replacement for invalid sequences.
///
/// Used by provider health probes to surface a compact "what went wrong"
/// from a child process's stdout/stderr. Lossy decode is deliberate:
/// truncating error messages on non-UTF-8 locales produces worse UX than
/// showing the replacement character.
pub fn first_non_empty_line(bytes: &[u8]) -> Option<String> {
    let text = String::from_utf8_lossy(bytes);
    text.lines()
        .map(str::trim)
        .find(|line| !line.is_empty())
        .map(ToOwned::to_owned)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn returns_first_non_empty_trimmed_line() {
        assert_eq!(
            first_non_empty_line(b"\n\n  hello\n").as_deref(),
            Some("hello"),
        );
    }

    #[test]
    fn none_for_empty_or_whitespace_only() {
        assert_eq!(first_non_empty_line(b""), None);
        assert_eq!(first_non_empty_line(b"\n   \n\t\n"), None);
    }

    #[test]
    fn lossy_decode_preserves_useful_content() {
        // Invalid UTF-8 byte 0xFF mid-string should not drop the whole line.
        let mut bytes = b"claude-cli v1.2.3 ".to_vec();
        bytes.push(0xFF);
        bytes.extend_from_slice(b"\n");
        let out = first_non_empty_line(&bytes).expect("should decode");
        assert!(out.starts_with("claude-cli v1.2.3"));
    }
}
