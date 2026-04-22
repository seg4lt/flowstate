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
                // Phase 6 entry point — parses
                //   flowstate daemon --data-dir <PATH>
                //                    [--idle-timeout <SECS>]
                //                    [--drain-grace <SECS>]
                // and delegates to `flowstate_app_layer::daemon_main::
                // run_blocking`, which:
                //   1. Opens SQLite stores under --data-dir (WAL mode).
                //   2. Constructs the 6 provider adapters.
                //   3. Binds HttpTransport on 127.0.0.1:0 with the
                //      app-layer HTTP router merged in.
                //   4. Writes <data_dir>/daemon.handshake.
                //   5. Blocks until SIGINT/SIGTERM or /api/shutdown,
                //      then drain-shutdown + exit.
                //
                // The Tauri shell spawns this subcommand as a child
                // on app startup and reads the handshake file to
                // discover the base URL for `DaemonClient`. Dev mode
                // can skip the spawn and run embedded (the
                // pre-Phase-6 path still works for now).
                use std::path::PathBuf;
                use std::time::Duration;
                let mut data_dir: Option<PathBuf> = None;
                let mut idle_timeout: Option<Duration> = None;
                let mut drain_grace: Option<Duration> = None;
                while let Some(arg) = args.next() {
                    match arg.as_str() {
                        "--data-dir" => {
                            data_dir = args.next().map(PathBuf::from);
                        }
                        "--idle-timeout" => {
                            idle_timeout = args
                                .next()
                                .and_then(|s| s.parse::<u64>().ok())
                                .map(Duration::from_secs);
                        }
                        "--drain-grace" => {
                            drain_grace = args
                                .next()
                                .and_then(|s| s.parse::<u64>().ok())
                                .map(Duration::from_secs);
                        }
                        "--help" | "-h" => {
                            eprintln!(
                                "flowstate daemon — standalone long-running daemon\n\
                                 \n\
                                 Usage: flowstate daemon --data-dir PATH [--idle-timeout SECS] [--drain-grace SECS]\n\
                                 \n\
                                 Writes <data_dir>/daemon.handshake with the loopback URL\n\
                                 and PID once bound. Blocks until SIGINT/SIGTERM."
                            );
                            return;
                        }
                        other => {
                            eprintln!("flowstate daemon: unknown flag {other}");
                            std::process::exit(2);
                        }
                    }
                }
                let data_dir = match data_dir {
                    Some(d) => d,
                    None => {
                        eprintln!(
                            "flowstate daemon: --data-dir <PATH> is required\n\
                             Run with --help for usage."
                        );
                        std::process::exit(2);
                    }
                };
                let res = flowstate_app_layer::daemon_main::run_blocking(
                    flowstate_app_layer::daemon_main::DaemonMainArgs {
                        data_dir,
                        idle_timeout,
                        drain_grace,
                    },
                );
                if let Err(err) = res {
                    eprintln!("flowstate daemon: {err:?}");
                    std::process::exit(1);
                }
                return;
            }
            "--help" | "-h" | "help" => {
                eprintln!(
                    "flowstate — multi-agent orchestration app\n\
                     \n\
                     Usage:\n\
                       flowstate                 Launch the UI (default; daemon embedded)\n\
                       flowstate daemon ...      Run the standalone daemon (Phase 6 WIP)\n\
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
