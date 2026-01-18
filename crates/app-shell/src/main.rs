use std::net::{Ipv4Addr, SocketAddr, SocketAddrV4};
use std::sync::mpsc::{channel, Sender};

use anyhow::{Context, Result};
use tao::dpi::LogicalSize;
use tao::event::{Event, WindowEvent};
use tao::event_loop::{ControlFlow, EventLoop};
use tao::window::WindowBuilder;
use wry::WebViewBuilder;
use zenui_app_shell::bootstrap;

#[derive(Debug, Clone)]
enum WindowCommand {
    Minimize,
    Maximize,
    Close,
    Drag { x: f64, y: f64 },
}

fn main() {
    if let Err(error) = run() {
        eprintln!("zenui failed: {error:?}");
        std::process::exit(1);
    }
}

fn run() -> Result<()> {
    let app = bootstrap(
        SocketAddr::V4(SocketAddrV4::new(Ipv4Addr::LOCALHOST, 0)),
        "zenui.db",
    )?;

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
        .with_url(&app.server.frontend_url())
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

    event_loop.run(move |event, _, control_flow| {
        let _ = (&app.tokio_runtime, &app.server, &app.runtime_core, &webview);
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
            _ => {}
        }
    });
}

#[cfg(target_os = "macos")]
fn init_menu_bar() {
    use muda::{Menu, MenuItem, PredefinedMenuItem, Submenu};

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
