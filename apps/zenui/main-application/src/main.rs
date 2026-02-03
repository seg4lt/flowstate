mod server_entry;

use std::sync::mpsc::channel;

use anyhow::{Context, Result};
use tao::dpi::LogicalSize;
use tao::event::{Event, WindowEvent};
use tao::event_loop::{ControlFlow, EventLoop};
use tao::window::WindowBuilder;
use wry::{Rect, WebViewBuilder};
use wry::dpi::{LogicalPosition, LogicalSize as WryLogicalSize};
use zenui_daemon_client::{ClientConfig, DaemonHandle, connect_or_spawn};

#[derive(Debug, Clone)]
enum WindowCommand {
    Minimize,
    Maximize,
    Close,
    Drag { x: f64, y: f64 },
}

fn main() {
    // Fat-binary dispatch: `zenui server ...` runs the daemon CLI; anything
    // else launches the tao/wry shell window. The shell's daemon-client
    // re-execs this same binary with `server start ...` to bring up the
    // daemon as an isolated child process.
    let argv: Vec<String> = std::env::args().collect();
    if argv.get(1).map(String::as_str) == Some("server") {
        std::process::exit(server_entry::run(argv));
    }

    if let Err(error) = run() {
        eprintln!("zenui failed: {error:?}");
        std::process::exit(1);
    }
}

fn run() -> Result<()> {
    // The tao+wry shell only speaks HTTP/WS (the webview doesn't know how
    // to load a Unix socket). Ask daemon-client for an HTTP transport
    // specifically; fail loudly if the running daemon doesn't offer one.
    let client_config =
        ClientConfig::for_current_project().context("resolve daemon client config")?;
    let handle: DaemonHandle = connect_or_spawn(&client_config)
        .context("failed to attach to or spawn zenui-server daemon")?;
    let http = handle.as_http().ok_or_else(|| {
        anyhow::anyhow!(
            "tao-web-shell requires an HTTP transport; daemon offered: {:?}",
            handle.address
        )
    })?;
    eprintln!(
        "zenui: attached to daemon at {} (pid={})",
        http.http_base, handle.pid
    );
    // Clone URL strings out of the borrow before dropping the handle so
    // we can pass them to wry and into the event-loop closure.
    let webview_url = http.http_base.to_string();

    let event_loop = EventLoop::new();
    let window = WindowBuilder::new()
        .with_title("ZenUI")
        .with_inner_size(LogicalSize::new(1400.0, 900.0))
        .with_decorations(false)
        .with_transparent(true)
        .build(&event_loop)
        .context("failed to create native window")?;

    #[cfg(target_os = "macos")]
    {
        use tao::platform::macos::WindowExtMacOS;
        window.set_fullsize_content_view(true);
        window.set_titlebar_transparent(true);

        // Initialize menu bar for copy/paste support
        init_menu_bar();
    }

    let window_id = window.id();
    let (tx, rx) = channel::<WindowCommand>();

    let webview = WebViewBuilder::new()
        .with_url(&webview_url)
        .with_devtools(cfg!(debug_assertions))
        .with_transparent(true)
        .with_accept_first_mouse(true)
        .with_ipc_handler(move |request| {
            let body = request.body();
            if let Ok(data) = serde_json::from_str::<serde_json::Value>(body) {
                if let Some(cmd) = data.get("cmd").and_then(|v| v.as_str()) {
                    let command = match cmd {
                        "minimize" => Some(WindowCommand::Minimize),
                        "maximize" => Some(WindowCommand::Maximize),
                        "close" => Some(WindowCommand::Close),
                        "drag" => {
                            let x = data.get("x").and_then(|v| v.as_f64()).unwrap_or(0.0);
                            let y = data.get("y").and_then(|v| v.as_f64()).unwrap_or(0.0);
                            Some(WindowCommand::Drag { x, y })
                        }
                        _ => None,
                    };
                    if let Some(command) = command {
                        let _ = tx.send(command);
                    }
                }
            }
        })
        .build(&window)
        .context("failed to create webview")?;

    // The daemon runs in its own process, so closing the window just
    // detaches from it. We do not need to keep `handle` alive for the
    // duration of the event loop — its fields are already consumed.
    drop(handle);

    event_loop.run(move |event, _, control_flow| {
        let _ = &webview;
        *control_flow = ControlFlow::Wait;

        // Handle window commands from IPC
        if let Ok(command) = rx.try_recv() {
            match command {
                WindowCommand::Minimize => window.set_minimized(true),
                WindowCommand::Maximize => {
                    if window.is_maximized() {
                        window.set_maximized(false);
                    } else {
                        window.set_maximized(true);
                    }
                }
                WindowCommand::Close => {
                    *control_flow = ControlFlow::Exit;
                }
                WindowCommand::Drag { .. } => {
                    // Window dragging is handled by the drag_window API
                    // This command is a placeholder for future drag implementation
                    #[cfg(target_os = "macos")]
                    {
                        // On macOS with undecorated windows, dragging is handled
                        // by the system when clicking on the title bar area
                    }
                }
            }
        }

        match event {
            Event::WindowEvent {
                window_id: event_window_id,
                event: WindowEvent::CloseRequested,
                ..
            } if event_window_id == window_id => {
                *control_flow = ControlFlow::Exit;
            }
            Event::WindowEvent {
                window_id: event_window_id,
                event: WindowEvent::Resized(_),
                ..
            } if event_window_id == window_id => {
                let size = window.inner_size().to_logical::<f64>(window.scale_factor());
                let _ = webview.set_bounds(Rect {
                    position: LogicalPosition::new(0.0, 0.0).into(),
                    size: WryLogicalSize::new(size.width, size.height).into(),
                });
            }
            _ => {}
        }
    });
}

#[cfg(target_os = "macos")]
fn init_menu_bar() {
    use muda::{Menu, PredefinedMenuItem, Submenu};

    // Create the menu bar
    let menu_bar = Menu::new();

    // Create Edit menu with copy/paste
    let edit_menu = Submenu::new("Edit", true);
    edit_menu.append(&PredefinedMenuItem::undo(None)).ok();
    edit_menu.append(&PredefinedMenuItem::redo(None)).ok();
    edit_menu.append(&PredefinedMenuItem::separator()).ok();
    edit_menu.append(&PredefinedMenuItem::cut(None)).ok();
    edit_menu.append(&PredefinedMenuItem::copy(None)).ok();
    edit_menu.append(&PredefinedMenuItem::paste(None)).ok();
    edit_menu.append(&PredefinedMenuItem::select_all(None)).ok();

    // Add to menu bar
    menu_bar.append(&edit_menu).ok();

    // Initialize the menu bar for this window
    menu_bar.init_for_nsapp();
}

#[cfg(not(target_os = "macos"))]
fn init_menu_bar() {
    // No-op on non-macOS platforms
}
