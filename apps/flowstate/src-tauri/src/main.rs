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
            "--help" | "-h" | "help" => {
                // Preserve Tauri's default launch for bare invocation;
                // only intercept when we recognise a subcommand. Help
                // for known subcommands is printed by the subcommand
                // itself when it parses `--help` from its own argv.
                eprintln!(
                    "flowstate — multi-agent orchestration app\n\
                     \n\
                     Usage:\n\
                       flowstate                 Launch the UI (default)\n\
                       flowstate mcp-server ...  Run the cross-provider MCP stdio server\n\
                     \n\
                     `flowstate mcp-server --help` for subcommand flags."
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
