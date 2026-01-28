use std::net::SocketAddr;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow, bail};
use clap::{Args, Parser, Subcommand};
use zenui_daemon_core::{DaemonConfig, ReadyFile, run_blocking};

#[derive(Debug, Parser)]
#[command(
    name = "zenui-server",
    about = "The ZenUI daemon: owns the runtime and serves clients over HTTP/WS.",
    version
)]
struct Cli {
    #[command(subcommand)]
    command: Commands,
}

#[derive(Debug, Subcommand)]
enum Commands {
    /// Launch the daemon. With --foreground the daemon runs in this process
    /// and logs to stderr; without it the daemon detaches and re-execs with
    /// --foreground, redirecting logs to `$logdir/zenui-server.log`.
    Start(StartArgs),
    /// Ask the running daemon to shut down cleanly. Reads the ready file,
    /// POSTs /api/shutdown, and waits up to 10 seconds for the ready file
    /// to disappear.
    Stop(StopArgs),
    /// Print the contents of the daemon's ready file for this project, or
    /// a message if no daemon is running.
    Status(StatusArgs),
}

#[derive(Debug, Args)]
struct StartArgs {
    /// Run the daemon in the foreground instead of detaching.
    #[arg(long)]
    foreground: bool,
    /// Bind address (defaults to 127.0.0.1 with an OS-assigned port).
    #[arg(long, default_value = "127.0.0.1:0")]
    bind: String,
    /// Project root (defaults to the current working directory).
    #[arg(long)]
    project_root: Option<PathBuf>,
    /// Frontend dist directory (optional override).
    #[arg(long)]
    frontend_dist: Option<PathBuf>,
    /// Idle timeout in seconds before auto-shutdown.
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

fn main() {
    if let Err(err) = real_main() {
        eprintln!("zenui-server error: {err:?}");
        std::process::exit(1);
    }
}

fn real_main() -> Result<()> {
    let cli = Cli::parse();
    match cli.command {
        Commands::Start(args) => run_start(args),
        Commands::Stop(args) => run_stop(args),
        Commands::Status(args) => run_status(args),
    }
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
        bind_addr,
        database_name: "zenui.db".to_string(),
        project_root: project_root.clone(),
        idle_timeout: Duration::from_secs(args.idle_timeout_secs),
        shutdown_grace: Duration::from_secs(5),
        frontend_dist: args.frontend_dist.clone(),
        log_file: None,
        detach: false,
    };

    run_blocking(config).context("daemon exited with error")
}

/// Fork-exec ourselves with `--foreground` set, detaching so the shell can
/// return immediately and the daemon outlives its parent.
fn spawn_detached(project_root: &PathBuf, args: &StartArgs, _bind: SocketAddr) -> Result<()> {
    let current_exe = std::env::current_exe().context("locate current executable")?;

    let mut cmd = Command::new(&current_exe);
    cmd.arg("start")
        .arg("--foreground")
        .arg("--bind")
        .arg(&args.bind)
        .arg("--project-root")
        .arg(project_root)
        .arg("--idle-timeout-secs")
        .arg(args.idle_timeout_secs.to_string());
    if let Some(dist) = args.frontend_dist.as_ref() {
        cmd.arg("--frontend-dist").arg(dist);
    }

    // Redirect stdio so the detached child doesn't share our terminal.
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
        // setsid() so the child becomes its own session leader and survives
        // the parent shell closing its tty.
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
        .context("failed to spawn detached zenui-server")?;
    eprintln!(
        "zenui-server spawned (pid={}), waiting for ready file; logs: {}",
        child.id(),
        log_path.display()
    );

    // Poll for the ready file to appear so the parent can confirm boot.
    let ready = ReadyFile::for_project(project_root)?;
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        if let Some(content) = ready.read()? {
            println!(
                "zenui-server ready at {} (pid={})",
                content.http_base, content.pid
            );
            return Ok(());
        }
        if Instant::now() >= deadline {
            bail!(
                "timeout waiting for zenui-server ready file; see {}",
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
            println!("zenui-server: no ready file for {}", project_root.display());
            return Ok(());
        }
    };

    let shutdown_url = format!("{}/api/shutdown", content.http_base);
    let response = ureq::post(&shutdown_url)
        .timeout(Duration::from_secs(5))
        .call();
    match response {
        Ok(_) => println!("zenui-server: shutdown request accepted"),
        Err(ureq::Error::Status(code, _)) if code == 204 || code == 200 => {
            println!("zenui-server: shutdown request accepted");
        }
        Err(err) => return Err(anyhow!("/api/shutdown failed: {err}")),
    }

    // Wait for the ready file to be deleted, then we know the daemon is gone.
    let deadline = Instant::now() + Duration::from_secs(10);
    while Instant::now() < deadline {
        if ready.read()?.is_none() {
            println!("zenui-server: exited cleanly");
            return Ok(());
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    bail!("zenui-server did not delete its ready file within 10s")
}

fn run_status(args: StatusArgs) -> Result<()> {
    let project_root = resolve_project_root(args.project_root)?;
    let ready = ReadyFile::for_project(&project_root)?;
    match ready.read()? {
        Some(content) => {
            println!(
                "zenui-server running\n  pid:           {}\n  http_base:     {}\n  ws_url:        {}\n  protocol:      {}\n  started_at:    {}\n  version:       {}\n  project_root:  {}\n  ready_file:    {}",
                content.pid,
                content.http_base,
                content.ws_url,
                content.protocol_version,
                content.started_at,
                content.daemon_version,
                content.project_root,
                ready.path().display(),
            );
        }
        None => {
            println!(
                "zenui-server: no ready file for {}",
                project_root.display()
            );
        }
    }
    Ok(())
}

fn log_file_path() -> Result<PathBuf> {
    let base = std::env::temp_dir();
    let dir = base.join("zenui").join("logs");
    Ok(dir.join("zenui-server.log"))
}
