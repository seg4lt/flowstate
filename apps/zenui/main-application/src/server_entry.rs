//! Daemon entry point embedded in the single zenui binary.
//!
//! When the zenui binary is invoked as `zenui server <subcommand>`, `main()`
//! strips the `server` arg and hands the rest to [`run`], which implements
//! the same CLI that the old standalone `zenui-server` binary provided.

use std::net::SocketAddr;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow, bail};
use clap::{Args, Parser, Subcommand};
use zenui_daemon_core::{
    DaemonConfig, ReadyFile, Transport, TransportAddressInfo, run_blocking, transport_http,
};
use zenui_provider_api::ProviderKind;
use transport_http::HttpTransport;

#[derive(Debug, Parser)]
#[command(
    name = "zenui server",
    about = "The ZenUI daemon: owns the runtime and serves clients over HTTP/WS.",
    version
)]
struct Cli {
    #[command(subcommand)]
    command: Commands,
}

#[derive(Debug, Subcommand)]
enum Commands {
    Start(StartArgs),
    Stop(StopArgs),
    Status(StatusArgs),
}

#[derive(Debug, Args)]
struct StartArgs {
    #[arg(long)]
    foreground: bool,
    #[arg(long, default_value = "127.0.0.1:0")]
    bind: String,
    #[arg(long)]
    project_root: Option<PathBuf>,
    #[arg(long, default_value_t = 60)]
    idle_timeout_secs: u64,
}

#[derive(Debug, Args)]
struct StopArgs {
    #[arg(long)]
    project_root: Option<PathBuf>,
}

#[derive(Debug, Args)]
struct StatusArgs {
    #[arg(long)]
    project_root: Option<PathBuf>,
}

/// Entry point for `zenui server ...`. `argv` is the full process argv
/// (including `argv[0]` and the `server` dispatch arg); we strip them both
/// so clap sees only the subcommand and its flags.
pub fn run(argv: Vec<String>) -> i32 {
    // Synthesise argv with "zenui server" as the program name so clap's
    // error messages read naturally.
    let mut parser_argv: Vec<String> = vec!["zenui server".to_string()];
    // Skip argv[0] (the exe path) and argv[1] ("server"); keep the rest.
    parser_argv.extend(argv.into_iter().skip(2));

    let cli = match Cli::try_parse_from(parser_argv) {
        Ok(cli) => cli,
        Err(err) => {
            // clap's print() uses the right stream (stdout for --help, stderr for errors)
            // and returns the correct exit code (0 for --help/--version, 2 for errors).
            let _ = err.print();
            return err.exit_code();
        }
    };

    let result = match cli.command {
        Commands::Start(args) => run_start(args),
        Commands::Stop(args) => run_stop(args),
        Commands::Status(args) => run_status(args),
    };

    if let Err(err) = result {
        eprintln!("zenui server error: {err:?}");
        return 1;
    }
    0
}

fn resolve_project_root(arg: Option<PathBuf>) -> Result<PathBuf> {
    let root = arg
        .map(Ok)
        .unwrap_or_else(std::env::current_dir)
        .context("failed to resolve project root")?;
    let canonical = std::fs::canonicalize(&root)
        .with_context(|| format!("canonicalize {}", root.display()))?;
    Ok(canonical)
}

fn run_start(args: StartArgs) -> Result<()> {
    let project_root = resolve_project_root(args.project_root.clone())?;
    let bind_addr: SocketAddr = args
        .bind
        .parse()
        .with_context(|| format!("invalid --bind value: {}", args.bind))?;

    if !args.foreground {
        return spawn_detached(&project_root, &args, bind_addr);
    }

    let config = DaemonConfig {
        database_name: "zenui.db".to_string(),
        project_root: project_root.clone(),
        idle_timeout: Duration::from_secs(args.idle_timeout_secs),
        shutdown_grace: Duration::from_secs(5),
        log_file: None,
        detach: false,
        enabled_providers: ProviderKind::ALL.to_vec(),
    };

    let transports: Vec<Box<dyn Transport>> =
        vec![Box::new(HttpTransport::new(bind_addr))];

    run_blocking(config, transports).context("daemon exited with error")
}

/// Fork-exec ourselves with `server start --foreground`, detaching so the
/// parent shell can return immediately and the daemon outlives its caller.
fn spawn_detached(project_root: &PathBuf, args: &StartArgs, _bind: SocketAddr) -> Result<()> {
    let current_exe = std::env::current_exe().context("locate current executable")?;

    let mut cmd = Command::new(&current_exe);
    cmd.arg("server")
        .arg("start")
        .arg("--foreground")
        .arg("--bind")
        .arg(&args.bind)
        .arg("--project-root")
        .arg(project_root)
        .arg("--idle-timeout-secs")
        .arg(args.idle_timeout_secs.to_string());

    let log_path = log_file_path()?;
    if let Some(parent) = log_path.parent() {
        std::fs::create_dir_all(parent).ok();
    }
    let log_file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&log_path)
        .with_context(|| format!("open log file {}", log_path.display()))?;
    let log_file_err = log_file
        .try_clone()
        .context("clone log file handle for stderr")?;
    cmd.stdin(Stdio::null())
        .stdout(Stdio::from(log_file))
        .stderr(Stdio::from(log_file_err));

    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        unsafe {
            cmd.pre_exec(|| {
                if libc::setsid() == -1 {
                    return Err(std::io::Error::last_os_error());
                }
                Ok(())
            });
        }
    }

    let child = cmd
        .spawn()
        .context("failed to spawn detached zenui server")?;
    eprintln!(
        "zenui server spawned (pid={}), waiting for ready file; logs: {}",
        child.id(),
        log_path.display()
    );

    let ready = ReadyFile::for_project(project_root)?;
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        if let Some(content) = ready.read()? {
            let address_hint = content
                .transports
                .iter()
                .find_map(|t| match t {
                    TransportAddressInfo::Http { http_base, .. } => Some(http_base.clone()),
                    _ => None,
                })
                .unwrap_or_else(|| {
                    content
                        .transports
                        .first()
                        .map(|t| format!("transport={}", t.kind()))
                        .unwrap_or_else(|| "no-transports".to_string())
                });
            println!(
                "zenui server ready at {} (pid={})",
                address_hint, content.pid
            );
            return Ok(());
        }
        if Instant::now() >= deadline {
            bail!(
                "timeout waiting for zenui server ready file; see {}",
                log_path.display()
            );
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}

fn run_stop(args: StopArgs) -> Result<()> {
    let project_root = resolve_project_root(args.project_root)?;
    let ready = ReadyFile::for_project(&project_root)?;
    let content = match ready.read()? {
        Some(c) => c,
        None => {
            println!("zenui server: no ready file for {}", project_root.display());
            return Ok(());
        }
    };

    let http_base = content
        .transports
        .iter()
        .find_map(|t| match t {
            TransportAddressInfo::Http { http_base, .. } => Some(http_base.clone()),
            _ => None,
        })
        .ok_or_else(|| {
            anyhow!(
                "daemon has no HTTP transport to post /api/shutdown to; \
                 transports: {:?}",
                content.transports.iter().map(|t| t.kind()).collect::<Vec<_>>()
            )
        })?;

    let shutdown_url = format!("{}/api/shutdown", http_base);
    let response = ureq::post(&shutdown_url)
        .timeout(Duration::from_secs(5))
        .call();
    match response {
        Ok(_) => println!("zenui server: shutdown request accepted"),
        Err(ureq::Error::Status(code, _)) if code == 204 || code == 200 => {
            println!("zenui server: shutdown request accepted");
        }
        Err(err) => return Err(anyhow!("/api/shutdown failed: {err}")),
    }

    let deadline = Instant::now() + Duration::from_secs(10);
    while Instant::now() < deadline {
        if ready.read()?.is_none() {
            println!("zenui server: exited cleanly");
            return Ok(());
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    bail!("zenui server did not delete its ready file within 10s")
}

fn run_status(args: StatusArgs) -> Result<()> {
    let project_root = resolve_project_root(args.project_root)?;
    let ready = ReadyFile::for_project(&project_root)?;
    let content = match ready.read()? {
        Some(c) => c,
        None => {
            println!(
                "zenui server: no ready file for {}",
                project_root.display()
            );
            return Ok(());
        }
    };

    println!("zenui server running");
    println!("  pid:           {}", content.pid);
    println!("  protocol:      {}", content.protocol_version);
    println!("  started_at:    {}", content.started_at);
    println!("  version:       {}", content.daemon_version);
    println!("  project_root:  {}", content.project_root);
    println!("  ready_file:    {}", ready.path().display());
    println!("  transports:    {}", content.transports.len());
    let mut http_base_for_probe: Option<String> = None;
    for (i, entry) in content.transports.iter().enumerate() {
        match entry {
            TransportAddressInfo::Http { http_base, ws_url } => {
                println!("    [{i}] kind=http  http_base={http_base}  ws_url={ws_url}");
                if http_base_for_probe.is_none() {
                    http_base_for_probe = Some(http_base.clone());
                }
            }
            TransportAddressInfo::UnixSocket { path } => {
                println!("    [{i}] kind=unix-socket  path={path}");
            }
            TransportAddressInfo::NamedPipe { path } => {
                println!("    [{i}] kind=named-pipe  path={path}");
            }
            TransportAddressInfo::InProcess => {
                println!("    [{i}] kind=in-process");
            }
        }
    }

    let Some(http_base) = http_base_for_probe else {
        println!("  live status: (no HTTP transport to probe)");
        return Ok(());
    };
    let status_url = format!("{}/api/status", http_base);
    match ureq::get(&status_url)
        .timeout(Duration::from_millis(500))
        .call()
    {
        Ok(resp) => match resp.into_string() {
            Ok(body) => match serde_json::from_str::<serde_json::Value>(&body) {
                Ok(v) => {
                    println!("  live status:");
                    if let Some(obj) = v.as_object() {
                        for (k, val) in obj.iter() {
                            println!("    {k}: {val}");
                        }
                    }
                }
                Err(err) => println!("  live status: parse error ({err})"),
            },
            Err(err) => println!("  live status: body read error ({err})"),
        },
        Err(err) => println!("  live status: unreachable ({err})"),
    }
    Ok(())
}

fn log_file_path() -> Result<PathBuf> {
    let base = std::env::temp_dir();
    let dir = base.join("zenui").join("logs");
    Ok(dir.join("zenui-server.log"))
}
