// Prevents additional console window on Windows in release, DO NOT REMOVE!!
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

fn main() {
    // Argv dispatch for non-UI entry points that share the same
    // `flowstate` binary. This MUST run before Tauri's `Builder::
    // default()` so the Tauri webview runtime is never initialised
    // when the process is invoked as a CLI helper — provider adapters
    // launch us with `current_exe() mcp-server --session-id …`, and
    // booting Tauri from that context would try to open a window in
    // a non-main thread and crash.
    //
    // Keep this dispatcher tiny: every subcommand is a library call
    // into a separate crate. The subcommand owns its own tokio
    // runtime when needed; we exit with its return code.
    //
    // New subcommands go here, not in Tauri's command handlers. If
    // something needs both argv access and Tauri state, prefer two
    // subcommands (one CLI, one Tauri) with a shared library crate
    // underneath.
    let mut args = std::env::args().skip(1);
    if let Some(subcommand) = args.next() {
        match subcommand.as_str() {
            "mcp-server" => {
                if let Err(err) = flowstate_mcp_server::run_blocking() {
                    eprintln!("flowstate mcp-server: {err:?}");
                    std::process::exit(1);
                }
                return;
            }
            "daemon" => {
                // Historical subcommand. The daemon code used to run
                // in a separate process behind `flowstate daemon
                // --data-dir …`; it now runs in-process inside the
                // Tauri shell. Reject the subcommand explicitly
                // rather than silently falling through to the UI —
                // anyone still invoking this (old wrapper scripts,
                // supervisor configs) should see a clear message
                // instead of watching a UI window open on a headless
                // host.
                eprintln!(
                    "flowstate: the `daemon` subcommand has been removed.\n\
                     The daemon now runs in-process as part of the UI.\n\
                     Launch flowstate without a subcommand."
                );
                std::process::exit(2);
            }
            "--help" | "-h" | "help" => {
                eprintln!(
                    "flowstate — multi-agent orchestration app\n\
                     \n\
                     Usage:\n\
                       flowstate                 Launch the UI\n\
                       flowstate mcp-server ...  Run the cross-provider MCP stdio server\n\
                     \n\
                     `flowstate <subcommand> --help` for subcommand flags."
                );
                return;
            }
            _ => {
                // Unknown first-arg: fall through to Tauri. Tauri
                // itself ignores positional args today, so this
                // preserves existing behaviour for anyone who was
                // passing flags we don't own.
            }
        }
    }

    flowstate_lib::run()
}
