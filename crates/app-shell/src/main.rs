use std::net::{Ipv4Addr, SocketAddr, SocketAddrV4};

use anyhow::{Context, Result};
use tao::dpi::LogicalSize;
use tao::event::{Event, WindowEvent};
use tao::event_loop::{ControlFlow, EventLoop};
use tao::window::WindowBuilder;
use wry::{WebViewBuilder, Wry};
use zenui_app_shell::bootstrap;

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
        .with_title("T3 Code")
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
        window.set_title_hidden(true);
    }

    let webview = WebViewBuilder::new()
        .with_url(&app.server.frontend_url())
        .with_devtools(cfg!(debug_assertions))
        .with_transparent(true)
        .with_accept_first_mouse(true)
        .build(&window)
        .context("failed to create webview")?;

    let window_id = window.id();
    let window_clone = window.clone();

    event_loop.run(move |event, _, control_flow| {
        let _ = (&app.tokio_runtime, &app.server, &app.runtime_core, &webview);
        *control_flow = ControlFlow::Wait;

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
