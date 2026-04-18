//! vscode-jsonrpc (Content-Length framed) I/O helpers and the
//! small message constructors used by the copilot CLI adapter.
//!
//! Extracted from `lib.rs` in the phase 3 god-file split. The actual
//! CopilotCliProcess handle + dispatcher task live in `process.rs`.

use serde_json::Value;
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::process::{ChildStdin, ChildStdout};
use tokio::sync::Mutex;

pub(crate) async fn write_rpc_frame(stdin: &Mutex<ChildStdin>, msg: &Value) -> Result<(), String> {
    let json = serde_json::to_string(msg).map_err(|e| format!("rpc serialize: {e}"))?;
    let frame = format!("Content-Length: {}\r\n\r\n{}", json.len(), json);
    let mut guard = stdin.lock().await;
    guard
        .write_all(frame.as_bytes())
        .await
        .map_err(|e| format!("rpc write: {e}"))?;
    guard
        .flush()
        .await
        .map_err(|e| format!("rpc flush: {e}"))
}

/// Read one Content-Length-framed JSON-RPC message from the reader.
/// Returns None when the stream ends.
pub(crate) async fn read_rpc_frame(reader: &mut BufReader<ChildStdout>) -> Option<Value> {
    // Read headers until an empty line.
    let mut content_length: Option<usize> = None;
    loop {
        let mut line = String::new();
        match reader.read_line(&mut line).await {
            Ok(0) | Err(_) => return None, // EOF or error
            Ok(_) => {}
        }
        let trimmed = line.trim_end_matches(['\r', '\n']);
        if trimmed.is_empty() {
            break; // end of headers
        }
        if let Some(val) = trimmed.strip_prefix("Content-Length:") {
            if let Ok(n) = val.trim().parse::<usize>() {
                content_length = Some(n);
            }
        }
    }

    let n = content_length?;
    if n == 0 {
        return None;
    }

    let mut body = vec![0u8; n];
    match reader.read_exact(&mut body).await {
        Ok(_) => {}
        Err(_) => return None,
    }

    serde_json::from_slice(&body).ok()
}

// ── RPC helpers ───────────────────────────────────────────────────────────────

// ── RPC helpers ───────────────────────────────────────────────────────────────

pub(crate) fn make_request(id: u64, method: &str, params: Value) -> Value {
    serde_json::json!({
        "jsonrpc": "2.0",
        "id": id,
        "method": method,
        "params": params,
    })
}

pub(crate) fn make_response(id: &Value, result: Value) -> Value {
    serde_json::json!({
        "jsonrpc": "2.0",
        "id": id,
        "result": result,
    })
}

pub(crate) fn make_error_response(id: &Value, code: i64, message: &str) -> Value {
    serde_json::json!({
        "jsonrpc": "2.0",
        "id": id,
        "error": { "code": code, "message": message },
    })
}

// ── Pending requests and server callbacks ─────────────────────────────────────
