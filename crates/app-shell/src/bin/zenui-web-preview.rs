use std::net::{Ipv4Addr, SocketAddr, SocketAddrV4};

use anyhow::Result;
use zenui_app_shell::bootstrap;

fn main() {
    if let Err(error) = run() {
        eprintln!("zenui web preview failed: {error:?}");
        std::process::exit(1);
    }
}

fn run() -> Result<()> {
    let app = bootstrap(
        SocketAddr::V4(SocketAddrV4::new(Ipv4Addr::LOCALHOST, 3780)),
        "zenui-web-preview.db",
    )?;

    println!(
        "ZenUI browser preview listening on {}",
        app.server.frontend_url()
    );

    loop {
        let _ = (&app.tokio_runtime, &app.server, &app.runtime_core);
        std::thread::park();
    }
}
